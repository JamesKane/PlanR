//! Notes and what goes with them: a handler that takes notes from itself,
//! from the kernel (a closed pipe, a fault) and from the clock (`alarm`);
//! blocking calls a note cuts short; notes with no handler, `noted(NDFLT)`,
//! note groups and `kill`, of a sleeping process and a spinning one;
//! timed sleeps and `nsec`; semaphores; attached segments; and the files
//! of `#p`.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

use core::ffi::CStr;
use core::fmt::Write as _;
use core::hint::black_box;
use core::sync::atomic::{AtomicI64, AtomicU8, AtomicU64, AtomicUsize, Ordering};

use r::{Access, NoteHow, OpenFlags, OpenMode, RforkFlags as F, SegAttr};

r::main!(run);

const fn mode(access: Access) -> OpenMode {
    OpenMode {
        access,
        flags: OpenFlags::empty(),
    }
}

const READ: OpenMode = mode(Access::Read);
const WRITE: OpenMode = mode(Access::Write);

const MS: u64 = 1_000_000;

type Check = Result<(), &'static str>;

fn ensure(ok: bool, why: &'static str) -> Check {
    if ok { Ok(()) } else { Err(why) }
}

/// `r` must have failed with `text`.
fn fails<T>(r: Result<T, r::Error>, text: &[u8], why: &'static str) -> Check {
    match r.err() {
        None => Err(why),
        Some(e) => ensure(e.as_bytes() == text, why),
    }
}

/// The last note the handler took, and how many it has taken.
static TEXT: [AtomicU8; r::ERRMAX] = [const { AtomicU8::new(0) }; r::ERRMAX];
static LEN: AtomicUsize = AtomicUsize::new(0);
static COUNT: AtomicUsize = AtomicUsize::new(0);
/// Where the handler resumes code that faulted, if set.
static BAIL: AtomicU64 = AtomicU64::new(0);

fn last() -> r::Buf {
    let mut b = r::Buf::new();
    for t in &TEXT[..LEN.load(Ordering::Acquire)] {
        b.put(&[t.load(Ordering::Relaxed)]);
    }
    b
}

fn count() -> usize {
    COUNT.load(Ordering::Acquire)
}

extern "C" fn handler(ureg: *mut r::Ureg, note: *const u8) {
    // SAFETY: the kernel passes the note's NUL-terminated text.
    let text = unsafe { CStr::from_ptr(note.cast()) }.to_bytes();
    for (t, &b) in TEXT.iter().zip(text) {
        t.store(b, Ordering::Relaxed);
    }
    LEN.store(text.len().min(r::ERRMAX), Ordering::Release);
    COUNT.fetch_add(1, Ordering::AcqRel);
    let bail = BAIL.load(Ordering::Relaxed);
    if text.starts_with(b"sys: trap: ") && bail != 0 {
        // SAFETY: the registers the kernel saved, for `noted` to resume.
        unsafe { (*ureg).set_pc(bail) };
    }
    let how = if text == b"default" {
        NoteHow::Dflt
    } else {
        NoteHow::Cont
    };
    let _ = r::noted(how);
    r::exits(Some(b"noted failed"));
}

/// Where a caught fault resumes: a normal exit.
extern "C" fn bail() -> ! {
    r::exits(None)
}

fn fault() {
    // SAFETY: none; it is meant to fault.
    unsafe { black_box(core::ptr::read_volatile(black_box(8usize) as *const u64)) };
}

/// Runs `f` in a child made by `rfork(RFPROC | flags)`, which exits with
/// its result; returns its pid in the parent only.
fn spawn(flags: F, f: impl FnOnce() -> Check) -> Result<u64, &'static str> {
    match r::rfork(F::PROC | flags) {
        Err(_) => Err("rfork failed"),
        Ok(0) => r::exits(f().err().map(str::as_bytes)),
        Ok(pid) => Ok(pid),
    }
}

fn child(flags: F, f: impl FnOnce() -> Check) -> Check {
    spawn(flags, f).map(drop)
}

/// Awaits a child, whose exit status must hold `status` at its end (or
/// anywhere, if `anywhere`); says on the console what it was if not.
fn reap_where(status: &[u8], anywhere: bool) -> Check {
    let mut buf = [0u8; 256];
    let n = r::await_child(&mut buf).map_err(|_| "await failed")?;
    let text = &buf[..n];
    let ok = if status.is_empty() {
        text.ends_with(b" ''")
    } else if anywhere {
        text.windows(status.len()).any(|w| w == status)
    } else {
        text.strip_suffix(b"'").is_some_and(|t| t.ends_with(status))
    };
    if !ok {
        say(b"tnote: a child exited: ", text);
        return Err("a child exited with the wrong status");
    }
    Ok(())
}

fn reap_with(status: &[u8]) -> Check {
    reap_where(status, false)
}

fn reap() -> Check {
    reap_where(b"", false)
}

fn say(what: &[u8], text: &[u8]) {
    if let Ok(fd) = r::open(c"#c/cons", WRITE) {
        let mut b = r::Buf::new();
        b.put(what);
        b.put(text);
        b.put(b"\n");
        let _ = r::write(fd, b.as_bytes());
        let _ = r::close(fd);
    }
}

/// A NUL-terminated path, formatted.
struct Path([u8; 64]);

impl Path {
    fn new(args: core::fmt::Arguments<'_>) -> Path {
        let mut b = r::Buf::new();
        let _ = b.write_fmt(args);
        let mut p = [0u8; 64];
        let n = b.as_bytes().len().min(63);
        p[..n].copy_from_slice(&b.as_bytes()[..n]);
        Path(p)
    }

    fn get(&self) -> &CStr {
        CStr::from_bytes_until_nul(&self.0).unwrap_or(c"")
    }
}

fn proc_file(pid: u64, file: &str) -> Path {
    Path::new(format_args!("#p/{pid}/{file}"))
}

/// Reads all of `name` (up to the buffer) into `buf`.
fn read_file<'a>(name: &CStr, buf: &'a mut [u8]) -> Result<&'a [u8], &'static str> {
    let fd = r::open(name, READ).map_err(|_| "open failed")?;
    let mut n = 0;
    loop {
        match r::read(fd, &mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(_) => {
                let _ = r::close(fd);
                return Err("read failed");
            }
        }
        if n == buf.len() {
            break;
        }
    }
    r::close(fd).map_err(|_| "close failed")?;
    Ok(&buf[..n])
}

fn write_file(name: &CStr, text: &[u8]) -> Result<(), r::Error> {
    let fd = r::open(name, WRITE)?;
    let r = r::write(fd, text).map(drop);
    let _ = r::close(fd);
    r
}

fn number(text: &[u8]) -> Option<u64> {
    core::str::from_utf8(text).ok()?.trim().parse().ok()
}

/// The caller's pid, from `#c/pid`.
fn mypid() -> Result<u64, &'static str> {
    let mut buf = [0u8; 32];
    number(read_file(c"#c/pid", &mut buf)?).ok_or("#c/pid is not a number")
}

/// Posts `text` to process `pid`.
fn post(pid: u64, text: &[u8]) -> Check {
    write_file(proc_file(pid, "note").get(), text).map_err(|_| "cannot post a note")
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn run(_: r::Args) -> Check {
    r::notify(Some(handler)).map_err(|_| "notify failed")?;
    notes_to_self()?;
    kernel_notes()?;
    interrupted()?;
    alarms()?;
    no_handler()?;
    note_groups()?;
    kill()?;
    semaphores()?;
    segments()?;
    proc_files()
}

/// A note posted to itself arrives before the write returns; `noted`
/// outside a handler fails.
fn notes_to_self() -> Check {
    let before = count();
    post(mypid()?, b"hello")?;
    ensure(count() == before + 1, "a note to itself was not taken")?;
    ensure(last().as_bytes() == b"hello", "the note's text")?;
    fails(
        Err::<(), _>(r::noted(NoteHow::Cont)),
        b"bad arg in system call",
        "noted outside a handler",
    )
}

/// The kernel's notes: a write on a closed pipe, and a fault, which the
/// handler gets past by resuming elsewhere.
fn kernel_notes() -> Check {
    let [a, b] = r::pipe().map_err(|_| "pipe failed")?;
    r::close(b).map_err(|_| "close failed")?;
    let before = count();
    fails(r::write(a, b"x"), b"i/o on hungup channel", "a write on a closed pipe")?;
    ensure(count() == before + 1, "no note for a closed pipe")?;
    ensure(
        last().as_bytes() == b"sys: write on closed pipe",
        "the closed pipe's note",
    )?;
    r::close(a).map_err(|_| "close failed")?;
    // In shared memory, so the child's handler leaves its note here.
    child(F::MEM, || {
        BAIL.store(bail as *const () as u64, Ordering::Relaxed);
        fault();
        Err("the fault did not happen")
    })?;
    reap()?;
    BAIL.store(0, Ordering::Relaxed);
    ensure(last().as_bytes().starts_with(b"sys: trap: "), "a fault's note")
}

/// A note ends a sleep, a pipe read, a rendezvous, an `await` and a
/// semaphore wait with "interrupted"; nothing is lost by it.
fn interrupted() -> Check {
    let t0 = r::nsec();
    r::alarm(100);
    fails(r::sleep(10_000), b"interrupted", "an alarm did not cut a sleep short")?;
    let took = r::nsec() - t0;
    ensure(
        (90 * MS..5_000 * MS).contains(&took),
        "the alarm came at the wrong time",
    )?;
    ensure(last().as_bytes() == b"alarm", "the alarm's note")?;

    let [a, b] = r::pipe().map_err(|_| "pipe failed")?;
    let mut buf = [0u8; 16];
    r::alarm(30);
    fails(
        r::read(a, &mut buf),
        b"interrupted",
        "a note did not cut a pipe read short",
    )?;
    ensure(r::write(b, b"after") == Ok(5), "a pipe write")?;
    ensure(
        r::read(a, &mut buf) == Ok(5) && &buf[..5] == b"after",
        "a read after a note",
    )?;
    r::close(a).and(r::close(b)).map_err(|_| "close failed")?;

    r::alarm(30);
    fails(
        r::rendezvous(0x5151, 1),
        b"interrupted",
        "a note did not cut a rendezvous short",
    )?;
    // The interrupted rendezvous left nothing behind to meet.
    child(F::empty(), || {
        ensure(r::rendezvous(0x5151, 7) == Ok(8), "a rendezvous after a note")
    })?;
    ensure(r::rendezvous(0x5151, 8) == Ok(7), "a rendezvous after a note")?;
    reap()?;

    child(F::empty(), || {
        r::rendezvous(0x6262, 0).map(drop).map_err(|_| "rendezvous failed")
    })?;
    let mut w = [0u8; 128];
    r::alarm(30);
    fails(
        r::await_child(&mut w),
        b"interrupted",
        "a note did not cut an await short",
    )?;
    r::rendezvous(0x6262, 0).map_err(|_| "rendezvous failed")?;
    reap()
}

/// `alarm` gives back what the last one had left; a timed sleep takes at
/// least its time; `nsec` moves on.
fn alarms() -> Check {
    ensure(r::alarm(10_000) == 0, "an alarm was already set")?;
    let left = r::alarm(0);
    ensure(left > 9_000 && left <= 10_000, "alarm did not say what was left")?;
    ensure(r::alarm(0) == 0, "alarm(0) did not cancel")?;
    let t0 = r::nsec();
    r::sleep(50).map_err(|_| "sleep failed")?;
    let t1 = r::nsec();
    ensure(t1 - t0 >= 50 * MS, "sleep(50) was too short")?;
    ensure(t1 - t0 < 5_000 * MS, "sleep(50) was far too long")
}

/// With no handler a note ends the process, with its text as the status;
/// so does `noted(NDFLT)`.
fn no_handler() -> Check {
    child(F::empty(), || {
        r::notify(None).map_err(|_| "notify failed")?;
        post(mypid()?, b"goodbye")?;
        Err("lived on past a note")
    })?;
    reap_with(b"goodbye")?;
    child(F::empty(), || {
        post(mypid()?, b"default")?;
        Err("lived on past noted(NDFLT)")
    })?;
    reap_with(b"default")
}

/// A note written to `notepg` reaches every process of the group but the
/// writer.
fn note_groups() -> Check {
    r::rfork(F::NOTEG).map_err(|_| "rfork(RFNOTEG) failed")?;
    for tag in [0x71, 0x72] {
        child(F::empty(), || {
            r::notify(None).map_err(|_| "notify failed")?;
            r::rendezvous(tag, 0).map_err(|_| "rendezvous failed")?;
            let _ = r::sleep(20_000);
            Err("lived on past a group note")
        })?;
        r::rendezvous(tag, 0).map_err(|_| "rendezvous failed")?;
    }
    write_file(proc_file(mypid()?, "notepg").get(), b"die").map_err(|_| "cannot write notepg")?;
    reap_with(b"die")?;
    reap_with(b"die")
}

/// `kill` ends a process, handler or not: one asleep, and one spinning in
/// user mode, which the clock's interrupt finds.
fn kill() -> Check {
    for spin in [false, true] {
        let pid = spawn(F::empty(), move || {
            r::rendezvous(0x7a11, 0).map_err(|_| "rendezvous failed")?;
            loop {
                if spin {
                    core::hint::spin_loop();
                } else {
                    let _ = r::sleep(1_000);
                }
            }
        })?;
        r::rendezvous(0x7a11, 0).map_err(|_| "rendezvous failed")?;
        write_file(proc_file(pid, "ctl").get(), b"kill").map_err(|_| "cannot write ctl")?;
        reap_with(b"killed")?;
    }
    fails(
        write_file(proc_file(mypid()?, "ctl").get(), b"bogus"),
        b"bad process or channel control request",
        "a bad ctl message",
    )
}

static SEM: AtomicI64 = AtomicI64::new(0);

/// Semaphores count, wait, time out and are interrupted.
fn semaphores() -> Check {
    ensure(r::semacquire(&SEM, false) == Ok(false), "took from an empty semaphore")?;
    ensure(r::semrelease(&SEM, 2) == Ok(2), "semrelease's count")?;
    ensure(r::semacquire(&SEM, false) == Ok(true), "semacquire")?;
    ensure(r::semacquire(&SEM, true) == Ok(true), "semacquire")?;
    ensure(SEM.load(Ordering::Relaxed) == 0, "the count")?;
    let t0 = r::nsec();
    ensure(r::tsemacquire(&SEM, 50) == Ok(false), "tsemacquire took from nothing")?;
    ensure(r::nsec() - t0 >= 50 * MS, "tsemacquire gave up early")?;
    child(F::MEM, || {
        r::sleep(30).map_err(|_| "sleep failed")?;
        r::semrelease(&SEM, 1).map(drop).map_err(|_| "semrelease failed")
    })?;
    ensure(
        r::semacquire(&SEM, true) == Ok(true),
        "a release did not wake the waiter",
    )?;
    reap()?;
    r::alarm(30);
    fails(
        r::semacquire(&SEM, true),
        b"interrupted",
        "a note did not cut semacquire short",
    )
}

/// Shared segments are shared with children, memory segments copied; a
/// semaphore works across them; `segfree` zeroes; read-only and detached
/// segments fault.
fn segments() -> Check {
    let shared = r::segattach(SegAttr::empty(), c"shared", 0, 8192).map_err(|_| "segattach failed")?;
    // SAFETY: the segment is attached, and only words are used.
    let word = |va: u64| unsafe { &*(va as *const AtomicU64) };
    word(shared).store(1, Ordering::Relaxed);
    child(F::empty(), || {
        word(shared).store(2, Ordering::Relaxed);
        Ok(())
    })?;
    reap()?;
    ensure(word(shared).load(Ordering::Relaxed) == 2, "a shared segment was copied")?;

    let memory = r::segattach(SegAttr::empty(), c"memory", 0, 4096).map_err(|_| "segattach failed")?;
    ensure(memory + 4096 <= shared, "segments overlap")?;
    word(memory).store(1, Ordering::Relaxed);
    child(F::empty(), || {
        word(memory).store(2, Ordering::Relaxed);
        Ok(())
    })?;
    reap()?;
    ensure(word(memory).load(Ordering::Relaxed) == 1, "a memory segment was shared")?;

    // SAFETY: as above.
    let sem = unsafe { &*((shared + 8) as *const AtomicI64) };
    child(F::empty(), || {
        r::sleep(30).map_err(|_| "sleep failed")?;
        r::semrelease(sem, 1).map(drop).map_err(|_| "semrelease failed")
    })?;
    ensure(r::semacquire(sem, true) == Ok(true), "a semaphore in a shared segment")?;
    reap()?;

    word(shared + 4096).store(5, Ordering::Relaxed);
    // SAFETY: nothing counts on the page.
    unsafe { r::segfree(shared + 4096, 4096) }.map_err(|_| "segfree failed")?;
    ensure(word(shared + 4096).load(Ordering::Relaxed) == 0, "segfree kept a page")?;
    ensure(word(shared).load(Ordering::Relaxed) == 2, "segfree took the wrong page")?;
    r::segflush(shared, 8192).map_err(|_| "segflush failed")?;

    fails(
        r::segattach(SegAttr::empty(), c"shared", shared, 4096),
        b"segments overlap",
        "attached over a segment",
    )?;
    fails(
        r::segattach(SegAttr::empty(), c"bogus", 0, 4096),
        b"bad segment type",
        "attached a bogus class",
    )?;

    let ro = r::segattach(SegAttr::RONLY, c"shared", 0, 4096).map_err(|_| "segattach failed")?;
    ensure(word(ro).load(Ordering::Relaxed) == 0, "a read-only segment reads")?;
    child(F::empty(), || {
        r::notify(None).map_err(|_| "notify failed")?;
        word(ro).store(1, Ordering::Relaxed);
        Err("wrote a read-only segment")
    })?;
    reap_where(b"sys: trap: ", true)?;

    // SAFETY: nothing here uses it after.
    unsafe { r::segdetach(memory) }.map_err(|_| "segdetach failed")?;
    child(F::empty(), || {
        r::notify(None).map_err(|_| "notify failed")?;
        word(memory).load(Ordering::Relaxed);
        Err("read a detached segment")
    })?;
    reap_where(b"sys: trap: ", true)
}

/// What `#p` shows of this process.
fn proc_files() -> Check {
    let pid = mypid()?;
    let mut buf = [0u8; 2048];
    let status = read_file(proc_file(pid, "status").get(), &mut buf)?;
    ensure(status.starts_with(b"tnote "), "status does not start with the name")?;
    ensure(status.get(56..63) == Some(b"Running"), "status does not say it runs")?;
    ensure(read_file(proc_file(pid, "args").get(), &mut buf)? == b"tnote", "args")?;
    let id = read_file(proc_file(pid, "noteid").get(), &mut buf)?;
    ensure(number(id).is_some(), "noteid is not a number")?;
    let segs = read_file(proc_file(pid, "segment").get(), &mut buf)?;
    for kind in [&b"Text "[..], b"Stack ", b"Shared R"] {
        ensure(contains(segs, kind), "segment misses a segment")?;
    }
    let fds = read_file(proc_file(pid, "fd").get(), &mut buf)?;
    let mut dot = [0u8; 256];
    let d = r::open(c".", READ).map_err(|_| "cannot open .")?;
    let n = r::fd2path(d, &mut dot).map_err(|_| "fd2path of . failed")?;
    r::close(d).map_err(|_| "close failed")?;
    ensure(
        fds.starts_with(&dot[..n]) && fds.get(n) == Some(&b'\n'),
        "fd does not start with the current directory",
    )?;
    let me = proc_file(pid, "fd");
    ensure(contains(fds, me.get().to_bytes()), "fd does not list itself")?;
    let ns = read_file(proc_file(pid, "ns").get(), &mut buf)?;
    // An entry a read, the last `cd` and the current directory.
    let cd_at = ns.len().checked_sub(n + 4).ok_or("ns is too short")?;
    ensure(
        &ns[cd_at..cd_at + 3] == b"cd " && ns[cd_at + 3..ns.len() - 1] == dot[..n] && ns.ends_with(b"\n"),
        "ns does not end with cd",
    )?;
    ensure(ns.starts_with(b"bind "), "ns does not start with a bind")?;

    let fd = r::open(proc_file(pid, "mem").get(), READ).map_err(|_| "cannot open mem")?;
    SEM.store(0x5eed, Ordering::Relaxed);
    let mut w = [0u8; 8];
    let at = SEM.as_ptr() as u64;
    ensure(r::pread(fd, &mut w, at) == Ok(8), "mem read")?;
    ensure(i64::from_ne_bytes(w) == 0x5eed, "mem read the wrong bytes")?;
    r::close(fd).map_err(|_| "close failed")?;

    // A child's exit, read from `wait` instead of `await`.
    child(F::empty(), || Err("waited"))?;
    let fd = r::open(proc_file(pid, "wait").get(), READ).map_err(|_| "cannot open wait")?;
    let n = r::read(fd, &mut buf).map_err(|_| "wait read failed")?;
    r::close(fd).map_err(|_| "close failed")?;
    ensure(buf[..n].ends_with(b"waited'"), "wait gave the wrong message")?;

    let mut stat = [0u8; 256];
    ensure(
        r::stat(Path::new(format_args!("#p/{pid}")).get(), &mut stat).is_ok(),
        "no #p/pid",
    )?;
    fails(
        r::stat(c"#p/999999", &mut stat),
        b"file does not exist",
        "a process that never was",
    )
}
