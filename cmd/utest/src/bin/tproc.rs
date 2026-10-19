//! Processes and pipes: what `rfork` shares and copies, copy-on-write
//! both ways, `RFMEM` (data and bss shared, the stack not), pipes and
//! their write boundaries, end of file and flow control, `#s`,
//! `rendezvous`, the name space, environment and descriptor groups,
//! `RFNOMNT`, `RFNOWAIT`, and `exec` in a forked child.

#![no_std]
#![no_main]

use core::ffi::CStr;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicU64, Ordering};

use r::{Access, BindFlags, OpenFlags, OpenMode, RforkFlags as F};

r::main!(run);

const fn mode(access: Access) -> OpenMode {
    OpenMode {
        access,
        flags: OpenFlags::empty(),
    }
}

const READ: OpenMode = mode(Access::Read);
const WRITE: OpenMode = mode(Access::Write);
const RDWR: OpenMode = mode(Access::ReadWrite);

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

/// In the data segment.
static DATA: AtomicU64 = AtomicU64::new(0x1234);
/// In the bss, over more than one page.
static BSS: [AtomicU64; 1024] = [const { AtomicU64::new(0) }; 1024];

fn get(a: &AtomicU64) -> u64 {
    a.load(Ordering::Relaxed)
}

fn set(a: &AtomicU64, v: u64) {
    a.store(v, Ordering::Relaxed);
}

/// Runs `f` in a child made by `rfork(RFPROC | flags)`, which exits with
/// its result; returns in the parent only.
fn child(flags: F, f: impl FnOnce() -> Check) -> Check {
    match r::rfork(F::PROC | flags) {
        Err(_) => Err("rfork failed"),
        Ok(0) => r::exits(f().err().map(str::as_bytes)),
        Ok(_) => Ok(()),
    }
}

/// Awaits a child, which must have exited with `status` (empty for a
/// normal exit); says on the console what it exited with if not.
fn reap_with(status: &[u8]) -> Check {
    let mut buf = [0u8; 256];
    let n = r::await_child(&mut buf).map_err(|_| "await failed")?;
    let text = &buf[..n];
    let ok = if status.is_empty() {
        text.ends_with(b" ''")
    } else {
        text.strip_suffix(b"'").is_some_and(|t| t.ends_with(status))
    };
    if !ok {
        if let Ok(fd) = r::open(c"#c/cons", WRITE) {
            let mut b = r::Buf::new();
            b.put(b"tproc: a child exited: ");
            b.put(text);
            b.put(b"\n");
            let _ = r::write(fd, b.as_bytes());
            let _ = r::close(fd);
        }
        return Err("a child failed");
    }
    Ok(())
}

fn reap() -> Check {
    reap_with(b"")
}

fn read_exact(fd: r::Fd, want: &[u8], why: &'static str) -> Check {
    let mut buf = [0u8; 64];
    let n = r::read(fd, &mut buf).map_err(|_| why)?;
    ensure(&buf[..n] == want, why)
}

fn write_all(fd: r::Fd, b: &[u8]) -> Check {
    ensure(r::write(fd, b) == Ok(b.len()), "pipe write failed")
}

fn close(fd: r::Fd) -> Check {
    r::close(fd).map_err(|_| "close failed")
}

fn run(_: r::Args) -> Check {
    copy_on_write()?;
    shared_memory()?;
    pipes()?;
    flow_control()?;
    srv()?;
    rendezvous()?;
    groups()?;
    no_mount()?;
    no_wait()?;
    fork_exec()
}

/// A child sees the parent's data, bss and stack as they were, and its
/// writes stay its own; the parent's writes after the fork stay the
/// parent's too.
fn copy_on_write() -> Check {
    set(&DATA, 7);
    set(&BSS[600], 9);
    let local = AtomicU64::new(11);
    child(F::empty(), || {
        ensure(get(&DATA) == 7 && get(&BSS[600]) == 9, "the child's data or bss differ")?;
        ensure(get(&local) == 11, "the child's stack differs")?;
        set(&DATA, 70);
        set(&BSS[600], 90);
        set(&local, 110);
        ensure(
            get(&DATA) == 70 && get(&local) == 110,
            "the child cannot write its copy",
        )
    })?;
    reap()?;
    ensure(
        get(&DATA) == 7 && get(&BSS[600]) == 9,
        "a child's write reached the parent",
    )?;
    ensure(get(&local) == 11, "a child's write reached the parent's stack")?;

    // The other way: the child waits until the parent has written.
    let [a, b] = r::pipe().map_err(|_| "pipe failed")?;
    child(F::empty(), || {
        read_exact(a, b"go", "the child's go")?;
        ensure(
            get(&DATA) == 7 && get(&BSS[1000]) == 0,
            "a parent's write reached the child",
        )
    })?;
    set(&DATA, 8);
    set(&BSS[1000], 1);
    write_all(b, b"go")?;
    reap()?;
    close(a)?;
    close(b)
}

/// `RFMEM` shares data and bss, but not the stack; a child of the child
/// without it still gets copies of the shared segments.
fn shared_memory() -> Check {
    set(&DATA, 1);
    let local = AtomicU64::new(1);
    child(F::MEM, || {
        set(&DATA, 2);
        set(&BSS[0], 3);
        set(&local, 4);
        child(F::empty(), || {
            set(&DATA, 99);
            set(&BSS[0], 99);
            Ok(())
        })?;
        reap()?;
        ensure(
            get(&DATA) == 2 && get(&BSS[0]) == 3,
            "a grandchild's write reached shared memory",
        )
    })?;
    reap()?;
    ensure(get(&DATA) == 2 && get(&BSS[0]) == 3, "RFMEM did not share data and bss")?;
    ensure(get(&local) == 1, "RFMEM shared the stack")
}

/// Pipes keep write boundaries and partial reads, give end of file once
/// the other end is closed, and refuse writes nothing can read, with a
/// note that ends the writer.
fn pipes() -> Check {
    let [a, b] = r::pipe().map_err(|_| "pipe failed")?;
    write_all(a, b"hello")?;
    read_exact(b, b"hello", "a pipe lost a write")?;
    write_all(b, b"ab")?;
    write_all(b, b"cd")?;
    read_exact(a, b"ab", "a read crossed a write boundary")?;
    read_exact(a, b"cd", "the second write")?;
    write_all(a, b"12345")?;
    let mut two = [0u8; 2];
    ensure(r::read(b, &mut two) == Ok(2) && &two == b"12", "a short read")?;
    read_exact(b, b"345", "the rest of a write")?;
    let mut path = [0u8; 64];
    let n = r::fd2path(a, &mut path).map_err(|_| "fd2path failed")?;
    ensure(&path[..n] == b"#|/data", "a pipe's name")?;

    child(F::FDG, || write_all(b, b"from a child"))?;
    read_exact(a, b"from a child", "a child's write")?;
    reap()?;
    close(b)?;
    let mut buf = [0u8; 8];
    ensure(
        r::read(a, &mut buf) == Ok(0),
        "no end of file once the other end closed",
    )?;
    // The write fails, and the note that says so ends a writer with no
    // handler.
    child(F::FDG, || {
        fails(r::write(a, b"x"), b"i/o on hungup channel", "a write nothing can read")?;
        Err("lived on past a write on a closed pipe")
    })?;
    reap_with(b"sys: write on closed pipe")?;
    close(a)
}

/// A writer far ahead of its reader waits, and nothing is lost.
fn flow_control() -> Check {
    const TOTAL: usize = 1 << 20;
    let [a, b] = r::pipe().map_err(|_| "pipe failed")?;
    child(F::FDG, || {
        let chunk = [0x5au8; 16 * 1024];
        for _ in 0..TOTAL / chunk.len() {
            write_all(b, &chunk)?;
        }
        Ok(())
    })?;
    close(b)?;
    let mut buf = [0u8; 16 * 1024];
    let mut got = 0;
    loop {
        let n = r::read(a, &mut buf).map_err(|_| "read failed")?;
        if n == 0 {
            break;
        }
        ensure(buf[..n].iter().all(|&x| x == 0x5a), "a pipe garbled its data")?;
        got += n;
    }
    ensure(got == TOTAL, "a pipe lost data")?;
    reap()?;
    close(a)
}

/// A descriptor posted in `#s` opens to the same chan.
fn srv() -> Check {
    let [mine, other] = r::pipe().map_err(|_| "pipe failed")?;
    let post = r::create(c"#s/tproc", WRITE, 0o600).map_err(|_| "cannot create #s/tproc")?;
    let mut num = r::Buf::new();
    let _ = write!(num, "{mine}");
    ensure(
        r::write(post, num.as_bytes()) == Ok(num.as_bytes().len()),
        "posting a descriptor",
    )?;
    fails(
        r::write(post, num.as_bytes()),
        b"inappropriate use of fd",
        "posting twice",
    )?;
    close(post)?;
    close(mine)?;
    let opened = r::open(c"#s/tproc", RDWR).map_err(|_| "cannot open #s/tproc")?;
    write_all(opened, b"via srv")?;
    read_exact(other, b"via srv", "the posted chan")?;
    let mut path = [0u8; 64];
    let len = r::fd2path(opened, &mut path).map_err(|_| "fd2path failed")?;
    ensure(&path[..len] == b"#|/data", "a srv open gives the posted chan")?;
    // As in Plan 9, creating a file that exists opens it truncated, which
    // a posted srv refuses.
    fails(
        r::create(c"#s/tproc", WRITE, 0o600),
        b"srv file already exists",
        "a second #s/tproc",
    )?;
    r::remove(c"#s/tproc").map_err(|_| "cannot remove #s/tproc")?;
    fails(r::open(c"#s/tproc", RDWR), b"file does not exist", "a removed srv")?;
    close(opened)?;
    close(other)
}

/// Two processes meet at a tag and swap values.
fn rendezvous() -> Check {
    child(F::empty(), || {
        ensure(r::rendezvous(0x7a9, 1) == Ok(2), "the child's rendezvous")
    })?;
    ensure(r::rendezvous(0x7a9, 2) == Ok(1), "the parent's rendezvous")?;
    reap()
}

fn lists(dir: &CStr, entry: &str) -> Result<bool, &'static str> {
    let fd = r::open(dir, READ).map_err(|_| "cannot open a directory")?;
    let mut buf = [0u8; 4096];
    let mut found = false;
    loop {
        let n = r::read(fd, &mut buf).map_err(|_| "directory read failed")?;
        if n == 0 {
            break;
        }
        found |= r::dirs(&buf[..n]).any(|d| d.name == entry);
    }
    close(fd)?;
    Ok(found)
}

/// Each group is shared by default, copied with its flag, and fresh with
/// its `C` flag.
fn groups() -> Check {
    child(F::NAMEG, || {
        r::bind(c"#c", c"/n", BindFlags::empty()).map_err(|_| "bind failed")?;
        ensure(lists(c"/n", "cons")?, "the child's bind")
    })?;
    reap()?;
    ensure(!lists(c"/n", "cons")?, "an RFNAMEG child's bind reached the parent")?;
    child(F::empty(), || {
        r::bind(c"#c", c"/n", BindFlags::empty()).map_err(|_| "bind failed")
    })?;
    reap()?;
    ensure(lists(c"/n", "cons")?, "a child's bind in a shared name space")?;
    child(F::CNAMEG, || {
        ensure(!lists(c"/n", "cons")?, "an RFCNAMEG child kept a bind")
    })?;
    reap()?;
    r::unmount(None, c"/n").map_err(|_| "unmount failed")?;

    let var = c"#e/tprocvar";
    child(F::ENVG, || {
        r::create(var, WRITE, 0o666).map(drop).map_err(|_| "create failed")
    })?;
    reap()?;
    fails(r::open(var, READ), b"file does not exist", "an RFENVG child's variable")?;
    child(F::empty(), || {
        r::create(var, WRITE, 0o666).map(drop).map_err(|_| "create failed")
    })?;
    reap()?;
    r::remove(var).map_err(|_| "a child's variable in a shared environment")?;
    child(F::CENVG, || {
        ensure(!lists(c"#e", "tprocvar")?, "an RFCENVG child's environment")
    })?;
    reap()?;

    let [a, b] = r::pipe().map_err(|_| "pipe failed")?;
    child(F::CFDG, || {
        fails(
            r::close(a),
            b"fd out of range or not open",
            "an RFCFDG child's descriptors",
        )
    })?;
    reap()?;
    child(F::FDG, || close(a))?;
    reap()?;
    write_all(b, b"still open")?;
    read_exact(a, b"still open", "an RFFDG child closed the parent's descriptor")?;
    close(a)?;
    close(b)?;

    // Without RFPROC the caller's own groups change.
    r::rfork(F::ENVG).map_err(|_| "rfork without RFPROC failed")?;
    fails(
        r::rfork(F::FDG | F::CFDG),
        b"bad arg in system call",
        "RFFDG with RFCFDG",
    )?;
    fails(r::rfork(F::MEM), b"bad arg in system call", "RFMEM without RFPROC")
}

/// After `RFNOMNT` only `#|`, `#d`, `#e`, `#c` and `#p` attach.
fn no_mount() -> Check {
    child(F::empty(), || {
        r::rfork(F::NAMEG | F::NOMNT).map_err(|_| "rfork(RFNOMNT) failed")?;
        fails(r::open(c"#r/rtc", READ), b"mount/attach disallowed", "#r after RFNOMNT")?;
        let fd = r::open(c"#c/pid", READ).map_err(|_| "#c after RFNOMNT")?;
        close(fd)
    })?;
    reap()?;
    let fd = r::open(c"#r/rtc", READ).map_err(|_| "RFNOMNT in a copied name space reached the parent")?;
    close(fd)
}

/// A child made with `RFNOWAIT` is not awaited.
fn no_wait() -> Check {
    let [a, b] = r::pipe().map_err(|_| "pipe failed")?;
    child(F::NOWAIT | F::FDG, || write_all(b, b"done"))?;
    let mut buf = [0u8; 64];
    fails(
        r::await_child(&mut buf),
        b"no living children",
        "an RFNOWAIT child was awaited",
    )?;
    read_exact(a, b"done", "the RFNOWAIT child")?;
    close(a)?;
    close(b)
}

/// A forked child can exec, in memory whose pages it shared until then.
fn fork_exec() -> Check {
    set(&DATA, 5);
    child(F::empty(), || {
        let _ = r::exec(c"/bin/targs", &[c"targs", c"forked"]);
        Err("exec failed")
    })?;
    reap_with(b"targs forked")?;
    ensure(get(&DATA) == 5, "the parent's data after a child's exec")
}
