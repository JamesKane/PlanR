//! 9P through `#M`: a toy file server, in a child on one end of a pipe,
//! mounted by the parent. It checks `fversion`, `fauth`, `mount`; walks,
//! opens, reads (several messages' worth too), writes, creates, removes,
//! stats and `wstat`s through the mount; that a server's error text comes
//! back unchanged; that requests from two processes are outstanding at
//! once; that a note flushes a request (`Tflush`); `RFNOMNT`; a session
//! shared through `#s`; that every fid is clunked; and that a server that
//! goes away fails what is outstanding without a note.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

use core::ffi::CStr;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicUsize, Ordering};

use abi::{DMDIR, QTDIR, QTFILE};
use ninep::{NOFID, Qid, Rmsg, Tmsg};
use r::{Access, BindFlags, Dir, NoteHow, OpenFlags, OpenMode, RforkFlags as F};

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
        Some(e) if e.as_bytes() == text => Ok(()),
        Some(e) => {
            say(b"t9p: wrong error: ", e.as_bytes());
            Err(why)
        }
    }
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

/// Awaits a child, which must have exited normally.
fn reap() -> Check {
    let mut buf = [0u8; 256];
    let n = r::await_child(&mut buf).map_err(|_| "await failed")?;
    if !buf[..n].ends_with(b" ''") {
        say(b"t9p: a child exited: ", &buf[..n]);
        return Err("a child failed");
    }
    Ok(())
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

fn read_file(name: &CStr, buf: &mut [u8]) -> Result<usize, r::Error> {
    let fd = r::open(name, READ)?;
    let n = r::read(fd, buf);
    let _ = r::close(fd);
    n
}

fn write_file(name: &CStr, data: &[u8]) -> Result<usize, r::Error> {
    let fd = r::open(name, WRITE)?;
    let n = r::write(fd, data);
    let _ = r::close(fd);
    n
}

/// The notes the handler has taken; it continues after `alarm` and dies
/// of anything else.
static NOTES: AtomicUsize = AtomicUsize::new(0);

extern "C" fn handler(_: *mut r::Ureg, note: *const u8) {
    // SAFETY: the kernel passes the note's NUL-terminated text.
    let text = unsafe { CStr::from_ptr(note.cast()) }.to_bytes();
    NOTES.fetch_add(1, Ordering::AcqRel);
    let how = if text == b"alarm" { NoteHow::Cont } else { NoteHow::Dflt };
    let _ = r::noted(how);
}

fn run(_: r::Args) -> Check {
    r::notify(Some(handler)).map_err(|_| "notify failed")?;
    let [c, s] = r::pipe().map_err(|_| "pipe failed")?;
    child(F::FDG, || {
        r::close(c).map_err(|_| "close failed")?;
        Server::new().serve(s)
    })?;
    r::close(s).map_err(|_| "close failed")?;
    session(c)?;
    mount(c)?;
    files()?;
    concurrent()?;
    flush()?;
    no_mount(c)?;
    shared(c)?;
    clunked(c)?;
    reap()?;
    hangup()?;
    ensure(NOTES.load(Ordering::Acquire) == 1, "a note besides the alarm")
}

/// `fversion` sets up the session (the server makes the messages
/// smaller); asking again gives the same; `fauth` passes on the server's
/// refusal.
fn session(c: r::Fd) -> Check {
    let mut v = [0u8; 16];
    let n = r::fversion(c, 0, &mut v).map_err(|_| "fversion failed")?;
    ensure(&v[..n] == b"9P2000", "fversion's version")?;
    let mut v = *b"9P2000";
    ensure(r::fversion(c, 0, &mut v).is_ok_and(|n| n == 6), "fversion again")?;
    let mut v = *b"9P1999";
    fails(
        r::fversion(c, 0, &mut v),
        b"bad 9P version specification",
        "fversion 9P1999",
    )?;
    fails(r::fauth(c, c""), b"authentication not required", "fauth")?;
    // A descriptor that is not open for both.
    let [pa, pb] = r::pipe().map_err(|_| "pipe failed")?;
    let ro = r::open(c"#c/cons", READ).map_err(|_| "open #c/cons failed")?;
    fails(
        r::fversion(ro, 0, &mut []),
        b"inappropriate use of fd",
        "fversion on a read-only fd",
    )?;
    for fd in [pa, pb, ro] {
        r::close(fd).map_err(|_| "close failed")?;
    }
    Ok(())
}

fn mount(c: r::Fd) -> Check {
    // A mount closes the descriptor it is given, as Plan 9's does, so it
    // gets a copy; creating in it needs `CREATE`, as in Plan 9.
    let m = r::dup(c, None).map_err(|_| "dup failed")?;
    r::mount(m, None, c"/n", BindFlags::CREATE, c"").map_err(|_| "mount failed")?;
    fails(r::close(m), b"fd out of range or not open", "mount left its fd open")?;
    fails(r::open(c"#M", READ), b"mount/attach disallowed", "#M attached by name")?;
    let mut buf = [0u8; 64];
    let n = read_file(c"/n/hello", &mut buf).map_err(|_| "read /n/hello failed")?;
    ensure(&buf[..n] == HELLO, "/n/hello's contents")
}

/// The usual operations, through the mount.
fn files() -> Check {
    // Stat: the name, length, and the mount's device.
    let mut st = [0u8; 128];
    let n = r::stat(c"/n/hello", &mut st).map_err(|_| "stat failed")?;
    let (d, _) = Dir::decode(&st[..n]).ok_or("bad stat")?;
    ensure(d.name == "hello" && d.length == HELLO.len() as u64, "stat of /n/hello")?;
    ensure(d.ty == u16::from(b'M') && d.uid == "toy", "stat's device and owner")?;
    // A directory read, every entry the mount's.
    let fd = r::open(c"/n", READ).map_err(|_| "open /n failed")?;
    let mut buf = [0u8; 1024];
    let mut names = 0;
    loop {
        let n = r::read(fd, &mut buf).map_err(|_| "read /n failed")?;
        if n == 0 {
            break;
        }
        for d in r::dirs(&buf[..n]) {
            ensure(d.ty == u16::from(b'M'), "a directory entry's device")?;
            names += usize::from(matches!(d.name, "hello" | "block" | "ctl" | "stats" | "sub" | "big"));
        }
    }
    ensure(names == 6, "the entries of /n")?;
    r::close(fd).map_err(|_| "close failed")?;
    // Several elements, and the server's own error text.
    let n = read_file(c"/n/sub/deep", &mut buf).map_err(|_| "read /n/sub/deep failed")?;
    ensure(&buf[..n] == b"deep\n", "/n/sub/deep's contents")?;
    fails(r::open(c"/n/sub/nothing", READ), b"no such toy file", "a missing file")?;
    fails(
        r::open(c"/n/sub/deep/x", READ),
        b"not a directory",
        "a walk through a file",
    )?;
    // More than one message's worth.
    let mut big = [0u8; BIG + 100];
    let fd = r::open(c"/n/big", READ).map_err(|_| "open /n/big failed")?;
    let n = r::read(fd, &mut big).map_err(|_| "read /n/big failed")?;
    r::close(fd).map_err(|_| "close failed")?;
    ensure(n == BIG, "/n/big's length")?;
    ensure(
        big[..n].iter().enumerate().all(|(i, &b)| b == big_byte(i)),
        "/n/big's contents",
    )?;
    // Create, write, read back, rename, remove.
    let fd = r::create(c"/n/new", RDWR, 0o644).map_err(|_| "create failed")?;
    ensure(r::write(fd, b"abc").is_ok_and(|n| n == 3), "write to a new file")?;
    let n = r::pread(fd, &mut buf, 0).map_err(|_| "pread failed")?;
    ensure(&buf[..n] == b"abc", "a new file's contents")?;
    let mut st = [0u8; 128];
    let d = Dir {
        name: "renamed",
        ..Dir::DONT_TOUCH
    };
    let n = d.encode(&mut st).ok_or("encode failed")?;
    r::fwstat(fd, &st[..n]).map_err(|_| "fwstat failed")?;
    r::close(fd).map_err(|_| "close failed")?;
    fails(r::open(c"/n/new", READ), b"no such toy file", "the old name")?;
    let n = read_file(c"/n/renamed", &mut buf).map_err(|_| "read /n/renamed failed")?;
    ensure(&buf[..n] == b"abc", "the renamed file")?;
    fails(
        r::create(c"/n/renamed", mode_excl(), 0o644),
        b"file already exists",
        "create over a file",
    )?;
    r::remove(c"/n/renamed").map_err(|_| "remove failed")?;
    fails(r::open(c"/n/renamed", READ), b"no such toy file", "a removed file")?;
    // Remove on close happens at the server, at the clunk.
    let rclose = OpenMode {
        access: Access::Write,
        flags: OpenFlags::RCLOSE,
    };
    let fd = r::create(c"/n/tmp", rclose, 0o644).map_err(|_| "create /n/tmp failed")?;
    ensure(r::stat(c"/n/tmp", &mut st).is_ok(), "/n/tmp before its close")?;
    r::close(fd).map_err(|_| "close failed")?;
    fails(
        r::stat(c"/n/tmp", &mut st),
        b"no such toy file",
        "/n/tmp after its close",
    )?;
    // Truncation by open.
    write_file(c"/n/sub/deep", b"x").map_err(|_| "write /n/sub/deep failed")?;
    let n = read_file(c"/n/sub/deep", &mut buf).map_err(|_| "read /n/sub/deep failed")?;
    ensure(&buf[..n] == b"xeep\n", "a write in place")?;
    let trunc = OpenMode {
        access: Access::Write,
        flags: OpenFlags::TRUNC,
    };
    let fd = r::open(c"/n/sub/deep", trunc).map_err(|_| "open with OTRUNC failed")?;
    r::write(fd, b"deep\n").map_err(|_| "write failed")?;
    r::close(fd).map_err(|_| "close failed")
}

fn mode_excl() -> OpenMode {
    OpenMode {
        access: Access::Write,
        flags: OpenFlags::EXCL,
    }
}

/// A read the server holds is outstanding while another process's
/// requests come and go; a write to `ctl` lets it finish.
fn concurrent() -> Check {
    child(F::empty(), || {
        let mut buf = [0u8; 32];
        let n = read_file(c"/n/block", &mut buf).map_err(|_| "read /n/block failed")?;
        ensure(&buf[..n] == b"released", "the held read's reply")
    })?;
    r::sleep(50).map_err(|_| "sleep failed")?;
    let mut buf = [0u8; 64];
    for _ in 0..3 {
        let n = read_file(c"/n/hello", &mut buf).map_err(|_| "read beside a held one failed")?;
        ensure(&buf[..n] == HELLO, "/n/hello beside a held read")?;
    }
    ensure(stats(&mut buf)?.starts_with(b"held 1 "), "the server holds the read")?;
    write_file(c"/n/ctl", b"release").map_err(|_| "write /n/ctl failed")?;
    reap()
}

/// A note ends a held read: the kernel flushes it, and the read fails
/// with "interrupted".
fn flush() -> Check {
    let fd = r::open(c"/n/block", READ).map_err(|_| "open /n/block failed")?;
    let mut buf = [0u8; 64];
    r::alarm(50);
    fails(r::read(fd, &mut buf), b"interrupted", "a read a note cut short")?;
    ensure(NOTES.load(Ordering::Acquire) == 1, "the alarm was not taken")?;
    r::close(fd).map_err(|_| "close failed")?;
    ensure(
        stats(&mut buf)?.starts_with(b"held 0 flushes 1 "),
        "the server saw the flush",
    )
}

/// No mounting, nor authenticating, after `RFNOMNT`.
fn no_mount(c: r::Fd) -> Check {
    child(F::NAMEG | F::NOMNT, || {
        fails(
            r::mount(c, None, c"/mnt", BindFlags::empty(), c""),
            b"mount/attach disallowed",
            "mount after RFNOMNT",
        )?;
        fails(r::fauth(c, c""), b"mount/attach disallowed", "fauth after RFNOMNT")
    })?;
    reap()
}

/// A second mount over the same transport, through `#s`, shares the
/// session.
fn shared(c: r::Fd) -> Check {
    let srv = r::create(c"#s/t9p", WRITE, 0o600).map_err(|_| "create #s/t9p failed")?;
    let mut num = r::Buf::new();
    let _ = write!(num, "{c}");
    r::write(srv, num.as_bytes()).map_err(|_| "post failed")?;
    let fd = r::open(c"#s/t9p", RDWR).map_err(|_| "open #s/t9p failed")?;
    r::mount(fd, None, c"/mnt", BindFlags::empty(), c"other").map_err(|_| "second mount failed")?;
    let mut buf = [0u8; 64];
    let n = read_file(c"/mnt/hello", &mut buf).map_err(|_| "read /mnt/hello failed")?;
    ensure(&buf[..n] == HELLO, "/mnt/hello")?;
    let st = stats(&mut buf)?;
    if !st.ends_with(b" versions 1 attaches 2\n") {
        say(b"t9p: stats: ", st);
        return Err("one session");
    }
    r::unmount(None, c"/mnt").map_err(|_| "unmount /mnt failed")?;
    r::remove(c"#s/t9p").map_err(|_| "remove #s/t9p failed")?;
    fails(r::close(fd), b"fd out of range or not open", "mount left its fd open")?;
    r::close(srv).map_err(|_| "close failed")
}

/// Once unmounted and closed, nothing holds a fid but what is open; when
/// that goes too, the session ends and the server sees end of file.
fn clunked(c: r::Fd) -> Check {
    let fd = r::open(c"/n/stats", READ).map_err(|_| "open /n/stats failed")?;
    r::unmount(None, c"/n").map_err(|_| "unmount /n failed")?;
    fails(
        r::stat(c"/n/hello", &mut [0; 128]),
        b"file does not exist",
        "/n/hello unmounted",
    )?;
    r::close(c).map_err(|_| "close failed")?;
    let mut buf = [0u8; 64];
    let n = r::read(fd, &mut buf).map_err(|_| "read stats failed")?;
    if !buf[..n].windows(7).any(|w| w == b"fids 1 ") {
        say(b"t9p: stats: ", &buf[..n]);
        return Err("fids left at the server");
    }
    r::close(fd).map_err(|_| "close failed")
}

/// A server that exits after the attach: what follows fails, with no
/// note for the write on the closed pipe.
fn hangup() -> Check {
    let [c, s] = r::pipe().map_err(|_| "pipe failed")?;
    child(F::FDG, || {
        r::close(c).map_err(|_| "close failed")?;
        Server::new().serve(s)
    })?;
    r::close(s).map_err(|_| "close failed")?;
    r::mount(c, None, c"/mnt", BindFlags::empty(), c"die").map_err(|_| "mount of the dying server failed")?;
    reap()?;
    fails(
        r::open(c"/mnt/hello", READ),
        b"i/o on hungup channel",
        "a request to a dead server",
    )?;
    r::unmount(None, c"/mnt").map_err(|_| "unmount failed")?;
    fails(r::close(c), b"fd out of range or not open", "mount left its fd open")
}

/// Reads `/n/stats`.
fn stats(buf: &mut [u8]) -> Result<&[u8], &'static str> {
    let n = read_file(c"/n/stats", buf).map_err(|_| "read /n/stats failed")?;
    Ok(&buf[..n])
}

const HELLO: &[u8] = b"hello, world\n";
const BIG: usize = 12_000;

fn big_byte(i: usize) -> u8 {
    #[allow(clippy::cast_possible_truncation)] // below 251
    let b = (i % 251) as u8;
    b
}

// The toy server.

/// The largest message it takes.
const SMSIZE: usize = 8192 + ninep::IOHDRSZ;
const NFILES: usize = 16;
const NFIDS: usize = 64;
const DATA: usize = 256;

const ROOT: u64 = 0;
const QHELLO: u64 = 1;
const QBLOCK: u64 = 2;
const QCTL: u64 = 3;
const QSTATS: u64 = 4;
const QSUB: u64 = 5;
const QBIG: u64 = 7;

#[derive(Clone, Copy)]
struct File {
    live: bool,
    path: u64,
    parent: u64,
    dir: bool,
    perm: u32,
    name: [u8; 32],
    nlen: usize,
    data: [u8; DATA],
    len: usize,
}

impl File {
    const NONE: File = File {
        live: false,
        path: 0,
        parent: 0,
        dir: false,
        perm: 0,
        name: [0; 32],
        nlen: 0,
        data: [0; DATA],
        len: 0,
    };

    fn new(path: u64, parent: u64, name: &str, dir: bool, data: &[u8]) -> File {
        let mut f = File {
            live: true,
            path,
            parent,
            dir,
            perm: if dir { 0o755 } else { 0o644 },
            ..File::NONE
        };
        f.set_name(name);
        f.data[..data.len()].copy_from_slice(data);
        f.len = data.len();
        f
    }

    fn set_name(&mut self, name: &str) {
        self.nlen = name.len().min(32);
        self.name[..self.nlen].copy_from_slice(&name.as_bytes()[..self.nlen]);
    }

    fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.nlen]).unwrap_or("?")
    }

    fn qid(&self) -> Qid {
        Qid {
            path: self.path,
            vers: 0,
            ty: if self.dir { QTDIR } else { QTFILE },
        }
    }

    fn length(&self) -> u64 {
        if self.path == QBIG { BIG as u64 } else { self.len as u64 }
    }

    /// Its stat, encoded into `out`.
    fn stat(&self, out: &mut [u8]) -> usize {
        let d = Dir {
            ty: 0,
            dev: 0,
            qid: self.qid(),
            mode: self.perm | if self.dir { DMDIR } else { 0 },
            atime: 0,
            mtime: 0,
            length: self.length(),
            name: self.name(),
            uid: "toy",
            gid: "toy",
            muid: "toy",
        };
        d.encode(out).unwrap_or(0)
    }
}

#[derive(Clone, Copy)]
struct Fid {
    used: bool,
    num: u32,
    path: u64,
    /// The open mode, once opened.
    open: Option<u8>,
}

struct Server {
    files: [File; NFILES],
    fids: [Fid; NFIDS],
    next_path: u64,
    /// The tag of the read of `block` it holds.
    held: Option<u16>,
    flushes: u32,
    versions: u32,
    attaches: u32,
    /// Exit after the attach.
    die: bool,
}

/// What a request gets.
#[allow(clippy::large_enum_variant)] // as for `Rmsg`
enum Answer<'a> {
    Reply(Rmsg<'a>),
    Error(&'static str),
    /// None yet.
    Hold,
}

use Answer::{Error as Fail, Reply};

impl Server {
    fn new() -> Server {
        let mut files = [File::NONE; NFILES];
        files[0] = File::new(ROOT, ROOT, "/", true, b"");
        files[1] = File::new(QHELLO, ROOT, "hello", false, HELLO);
        files[2] = File::new(QBLOCK, ROOT, "block", false, b"");
        files[3] = File::new(QCTL, ROOT, "ctl", false, b"");
        files[4] = File::new(QSTATS, ROOT, "stats", false, b"");
        files[5] = File::new(QSUB, ROOT, "sub", true, b"");
        files[6] = File::new(6, QSUB, "deep", false, b"deep\n");
        files[7] = File::new(QBIG, ROOT, "big", false, b"");
        Server {
            files,
            fids: [Fid {
                used: false,
                num: 0,
                path: 0,
                open: None,
            }; NFIDS],
            next_path: 8,
            held: None,
            flushes: 0,
            versions: 0,
            attaches: 0,
            die: false,
        }
    }

    /// Serves on `fd` until end of file.
    fn serve(mut self, fd: r::Fd) -> Check {
        let mut msg = [0u8; SMSIZE];
        let mut out = [0u8; SMSIZE];
        loop {
            let Some(n) = read_msg(fd, &mut msg)? else {
                return Ok(());
            };
            let (tag, t) = Tmsg::decode(&msg[..n]).map_err(|_| "the server got a bad message")?;
            let mut scratch = [0u8; SMSIZE];
            let answer = self.answer(tag, &t, &mut scratch, fd, &mut out)?;
            let reply = match answer {
                Reply(m) => m,
                Fail(ename) => Rmsg::Error { ename },
                Answer::Hold => continue,
            };
            send(fd, tag, &reply, &mut out)?;
            if self.die && matches!(t, Tmsg::Attach { .. }) {
                return Ok(());
            }
        }
    }

    fn file(&self, path: u64) -> Option<usize> {
        self.files.iter().position(|f| f.live && f.path == path)
    }

    fn fid(&self, fid: u32) -> Option<usize> {
        self.fids.iter().position(|f| f.used && f.num == fid)
    }

    fn child(&self, dir: u64, name: &str) -> Option<usize> {
        self.files
            .iter()
            .position(|f| f.live && f.path != ROOT && f.parent == dir && f.name() == name)
    }

    fn new_fid(&mut self, fid: u32, path: u64) -> bool {
        match self.fids.iter().position(|f| !f.used) {
            Some(i) => {
                self.fids[i] = Fid {
                    used: true,
                    num: fid,
                    path,
                    open: None,
                };
                true
            }
            None => false,
        }
    }

    fn remove_file(&mut self, path: u64) {
        if let Some(i) = self.file(path) {
            self.files[i].live = false;
        }
    }

    #[allow(clippy::too_many_lines)] // one arm per message
    fn answer<'a>(
        &mut self,
        tag: u16,
        req: &Tmsg<'_>,
        scratch: &'a mut [u8],
        fd: r::Fd,
        out: &mut [u8],
    ) -> Result<Answer<'a>, &'static str> {
        let ans = match *req {
            Tmsg::Version { msize, .. } => {
                self.versions += 1;
                self.fids.iter_mut().for_each(|f| f.used = false);
                #[allow(clippy::cast_possible_truncation)] // a small constant
                let msize = msize.min(SMSIZE as u32);
                Reply(Rmsg::Version {
                    msize,
                    version: ninep::VERSION,
                })
            }
            Tmsg::Auth { .. } => Fail("authentication not required"),
            Tmsg::Attach { fid, afid, aname, .. } => {
                if afid != NOFID {
                    Fail("no authentication")
                } else if self.fid(fid).is_some() || !self.new_fid(fid, ROOT) {
                    Fail("fid in use")
                } else {
                    self.attaches += 1;
                    self.die = aname == "die";
                    Reply(Rmsg::Attach {
                        qid: self.files[0].qid(),
                    })
                }
            }
            Tmsg::Flush { oldtag } => {
                if self.held == Some(oldtag) {
                    self.held = None;
                }
                self.flushes += 1;
                Reply(Rmsg::Flush)
            }
            Tmsg::Walk { fid, newfid, wnames } => {
                let Some(i) = self.fid(fid) else {
                    return Ok(Fail("unknown fid"));
                };
                if self.fids[i].open.is_some() {
                    return Ok(Fail("walk of an open fid"));
                }
                if newfid != fid && self.fid(newfid).is_some() {
                    return Ok(Fail("fid in use"));
                }
                let mut path = self.fids[i].path;
                let mut qids = [Qid::default(); ninep::MAXWELEM];
                let mut n = 0;
                for w in wnames.as_slice() {
                    let Some(f) = self.file(path) else { break };
                    if !self.files[f].dir {
                        if n == 0 {
                            return Ok(Fail("not a directory"));
                        }
                        break;
                    }
                    let Some(c) = self.child(path, w) else {
                        if n == 0 {
                            return Ok(Fail("no such toy file"));
                        }
                        break;
                    };
                    path = self.files[c].path;
                    qids[n] = self.files[c].qid();
                    n += 1;
                }
                if n == wnames.len() {
                    if newfid == fid {
                        self.fids[i].path = path;
                    } else if !self.new_fid(newfid, path) {
                        return Ok(Fail("too many fids"));
                    }
                }
                Reply(Rmsg::Walk {
                    wqids: ninep::Elems::new(&qids[..n]).ok_or("too many qids")?,
                })
            }
            Tmsg::Open { fid, mode } => {
                let Some(i) = self.fid(fid) else {
                    return Ok(Fail("unknown fid"));
                };
                let Some(f) = self.file(self.fids[i].path) else {
                    return Ok(Fail("file removed"));
                };
                if self.fids[i].open.is_some() {
                    return Ok(Fail("already open"));
                }
                if self.files[f].dir && mode & 3 != ninep::OREAD {
                    return Ok(Fail("is a directory"));
                }
                if mode & ninep::OTRUNC != 0 {
                    self.files[f].len = 0;
                }
                self.fids[i].open = Some(mode);
                Reply(Rmsg::Open {
                    qid: self.files[f].qid(),
                    iounit: 0,
                })
            }
            Tmsg::Create { fid, name, perm, mode } => {
                let Some(i) = self.fid(fid) else {
                    return Ok(Fail("unknown fid"));
                };
                let dir = self.fids[i].path;
                if self.child(dir, name).is_some() {
                    return Ok(Fail("file exists"));
                }
                let Some(slot) = self.files.iter().position(|f| !f.live) else {
                    return Ok(Fail("toy server full"));
                };
                let path = self.next_path;
                self.next_path += 1;
                self.files[slot] = File::new(path, dir, name, perm & DMDIR != 0, b"");
                self.files[slot].perm = perm & 0o777;
                self.fids[i].path = path;
                self.fids[i].open = Some(mode);
                Reply(Rmsg::Create {
                    qid: self.files[slot].qid(),
                    iounit: 0,
                })
            }
            Tmsg::Read { fid, offset, count } => {
                let Some(i) = self.fid(fid) else {
                    return Ok(Fail("unknown fid"));
                };
                let Some(f) = self.file(self.fids[i].path) else {
                    return Ok(Fail("file removed"));
                };
                let count = (count as usize).min(scratch.len());
                let offset = usize::try_from(offset).unwrap_or(usize::MAX);
                let file = self.files[f];
                let n = if file.dir {
                    self.dirread(file.path, offset, &mut scratch[..count])
                } else {
                    match file.path {
                        QBLOCK => {
                            self.held = Some(tag);
                            return Ok(Answer::Hold);
                        }
                        QSTATS => {
                            let mut b = r::Buf::new();
                            let used = self.fids.iter().filter(|f| f.used).count();
                            let _ = writeln!(
                                b,
                                "held {} flushes {} fids {used} versions {} attaches {}",
                                usize::from(self.held.is_some()),
                                self.flushes,
                                self.versions,
                                self.attaches
                            );
                            copy_at(b.as_bytes(), offset, &mut scratch[..count])
                        }
                        QBIG => {
                            let end = BIG.min(offset.saturating_add(count));
                            let n = end.saturating_sub(offset);
                            for (j, b) in scratch[..n].iter_mut().enumerate() {
                                *b = big_byte(offset + j);
                            }
                            n
                        }
                        _ => copy_at(&file.data[..file.len], offset, &mut scratch[..count]),
                    }
                };
                Reply(Rmsg::Read { data: &scratch[..n] })
            }
            Tmsg::Write { fid, offset, data } => {
                let Some(i) = self.fid(fid) else {
                    return Ok(Fail("unknown fid"));
                };
                let Some(f) = self.file(self.fids[i].path) else {
                    return Ok(Fail("file removed"));
                };
                if self.files[f].path == QCTL {
                    if data == b"release"
                        && let Some(held) = self.held.take()
                    {
                        send(fd, held, &Rmsg::Read { data: b"released" }, out)?;
                    }
                } else {
                    let offset = usize::try_from(offset).unwrap_or(usize::MAX);
                    let end = offset.saturating_add(data.len());
                    if end > DATA {
                        return Ok(Fail("toy file full"));
                    }
                    let file = &mut self.files[f];
                    file.data[offset..end].copy_from_slice(data);
                    file.len = file.len.max(end);
                }
                #[allow(clippy::cast_possible_truncation)] // within a message
                let count = data.len() as u32;
                Reply(Rmsg::Write { count })
            }
            Tmsg::Clunk { fid } => {
                let Some(i) = self.fid(fid) else {
                    return Ok(Fail("unknown fid"));
                };
                self.fids[i].used = false;
                if self.fids[i].open.is_some_and(|m| m & ninep::ORCLOSE != 0) {
                    self.remove_file(self.fids[i].path);
                }
                Reply(Rmsg::Clunk)
            }
            Tmsg::Remove { fid } => {
                let Some(i) = self.fid(fid) else {
                    return Ok(Fail("unknown fid"));
                };
                self.fids[i].used = false;
                let path = self.fids[i].path;
                if path == ROOT || self.files.iter().any(|f| f.live && f.path != ROOT && f.parent == path) {
                    Fail("cannot remove")
                } else {
                    self.remove_file(path);
                    Reply(Rmsg::Remove)
                }
            }
            Tmsg::Stat { fid } => {
                let Some(i) = self.fid(fid) else {
                    return Ok(Fail("unknown fid"));
                };
                let Some(f) = self.file(self.fids[i].path) else {
                    return Ok(Fail("file removed"));
                };
                let n = self.files[f].stat(scratch);
                Reply(Rmsg::Stat { stat: &scratch[..n] })
            }
            Tmsg::Wstat { fid, stat } => {
                let Some(i) = self.fid(fid) else {
                    return Ok(Fail("unknown fid"));
                };
                let Some(f) = self.file(self.fids[i].path) else {
                    return Ok(Fail("file removed"));
                };
                let Some((d, _)) = Dir::decode(stat) else {
                    return Ok(Fail("bad stat"));
                };
                if !d.name.is_empty() {
                    if self.child(self.files[f].parent, d.name).is_some() {
                        return Ok(Fail("file exists"));
                    }
                    self.files[f].set_name(d.name);
                }
                Reply(Rmsg::Wstat)
            }
        };
        Ok(ans)
    }

    /// Reads directory `dir` at byte `offset`: whole entries that fit.
    fn dirread(&self, dir: u64, offset: usize, out: &mut [u8]) -> usize {
        let (mut at, mut n) = (0, 0);
        let mut one = [0u8; 128];
        for f in self
            .files
            .iter()
            .filter(|f| f.live && f.path != ROOT && f.parent == dir)
        {
            let len = f.stat(&mut one);
            if at >= offset {
                if n + len > out.len() {
                    break;
                }
                out[n..n + len].copy_from_slice(&one[..len]);
                n += len;
            }
            at += len;
        }
        n
    }
}

/// Copies what lies at `offset` in `s` into `out`.
fn copy_at(s: &[u8], offset: usize, out: &mut [u8]) -> usize {
    let rest = s.get(offset..).unwrap_or(&[]);
    let n = rest.len().min(out.len());
    out[..n].copy_from_slice(&rest[..n]);
    n
}

/// Sends reply `m`, tagged `tag`.
fn send(fd: r::Fd, tag: u16, m: &Rmsg<'_>, out: &mut [u8]) -> Check {
    let n = m.encode(tag, out).map_err(|_| "the server cannot encode a reply")?;
    let w = r::write(fd, &out[..n]).map_err(|_| "the server's write failed")?;
    ensure(w == n, "the server's short write")
}

/// Reads one whole message into `buf`; `None` at end of file.
fn read_msg(fd: r::Fd, buf: &mut [u8]) -> Result<Option<usize>, &'static str> {
    let mut have = 0;
    loop {
        let need = if have < 4 {
            4
        } else {
            let n = ninep::msg_size(&buf[..have]).unwrap_or(0);
            if n < ninep::HDRSZ || n > buf.len() {
                return Err("the server got a bad size");
            }
            if have == n {
                return Ok(Some(n));
            }
            n
        };
        let got = r::read(fd, &mut buf[have..need]).map_err(|_| "the server's read failed")?;
        if got == 0 {
            return if have == 0 {
                Ok(None)
            } else {
                Err("end of file in a message")
            };
        }
        have += got;
    }
}
