//! The file calls and the name space: binds and unions, every slice-8
//! device, descriptors and `#d`, `chdir` and `..`, stat buffers, and the
//! errors Plan 9 gives. Ends by running `targs` from a copy in `#e`, a
//! program read into memory rather than paged from the boot archive.

#![no_std]
#![no_main]

use core::ffi::CStr;

use r::{Access, BindFlags, Dir, OpenFlags, OpenMode, Whence};

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
const EXEC: OpenMode = mode(Access::Exec);

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

/// Reads all of `name` into `buf`, returning the length.
fn slurp(name: &CStr, buf: &mut [u8]) -> Result<usize, &'static str> {
    let fd = r::open(name, READ).map_err(|_| "cannot open a file to read it")?;
    let mut n = 0;
    loop {
        let got = r::read(fd, &mut buf[n..]).map_err(|_| "read failed")?;
        if got == 0 {
            break;
        }
        n += got;
    }
    r::close(fd).map_err(|_| "close failed")?;
    Ok(n)
}

/// True if directory `name` lists `entry`.
fn lists(name: &CStr, entry: &str) -> Result<bool, &'static str> {
    let fd = r::open(name, READ).map_err(|_| "cannot open a directory")?;
    let mut buf = [0u8; 4096];
    let mut found = false;
    loop {
        let n = r::read(fd, &mut buf).map_err(|_| "directory read failed")?;
        if n == 0 {
            break;
        }
        let mut seen = 0;
        for d in r::dirs(&buf[..n]) {
            seen += d.size();
            found |= d.name == entry;
        }
        ensure(seen == n, "a directory read held a partial entry")?;
    }
    r::close(fd).map_err(|_| "close failed")?;
    Ok(found)
}

/// The number at the start of `b`, after spaces.
fn number(b: &[u8]) -> Option<u64> {
    let s = core::str::from_utf8(b).ok()?.trim_start();
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    s[..end].parse().ok()
}

/// `prefix` then `n` then `suffix`, NUL-terminated, in `buf`.
fn name_of<'a>(buf: &'a mut [u8; 64], prefix: &[u8], n: usize, suffix: &[u8]) -> &'a CStr {
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    let mut n = n;
    loop {
        i -= 1;
        digits[i] = b"0123456789"[n % 10];
        n /= 10;
        if n == 0 {
            break;
        }
    }
    let mut at = 0;
    for part in [prefix, &digits[i..], suffix] {
        buf[at..at + part.len()].copy_from_slice(part);
        at += part.len();
    }
    buf[at] = 0;
    CStr::from_bytes_until_nul(&buf[..=at]).unwrap_or(c"")
}

fn stat_dir<'a>(name: &CStr, buf: &'a mut [u8]) -> Result<Dir<'a>, &'static str> {
    let n = r::stat(name, buf).map_err(|_| "stat failed")?;
    Dir::decode(&buf[..n])
        .map(|(d, _)| d)
        .ok_or("stat gave a malformed entry")
}

fn run(_: r::Args) -> Result<(), &'static str> {
    // A clean name space, so the checks start from no binds whatever the
    // parent made.
    r::rfork(r::RforkFlags::CNAMEG).map_err(|_| "rfork(RFCNAMEG) failed")?;
    devices()?;
    environment()?;
    unions()?;
    descriptors()?;
    directories()?;
    errors()?;
    exec_from_memory()
}

/// `#c`, bound on `/dev`, and `#r` and `#P` beside it.
fn devices() -> Check {
    fails(
        r::open(c"/dev/cons", WRITE),
        b"file does not exist",
        "/dev/cons before any bind",
    )?;
    r::bind(c"#c", c"/dev", BindFlags::AFTER).map_err(|_| "bind #c /dev failed")?;
    let fd = r::open(c"/dev/cons", WRITE).map_err(|_| "cannot open /dev/cons")?;
    let msg = b"tfile: hello from /dev/cons\n";
    ensure(r::write(fd, msg) == Ok(msg.len()), "write to /dev/cons")?;
    let mut path = [0u8; 64];
    let n = r::fd2path(fd, &mut path).map_err(|_| "fd2path failed")?;
    ensure(&path[..n] == b"/dev/cons", "fd2path gave the wrong name")?;
    r::close(fd).map_err(|_| "close failed")?;
    fails(
        r::write(fd, b"x"),
        b"fd out of range or not open",
        "write to a closed fd",
    )?;

    let mut buf = [0xffu8; 64];
    let fd = r::open(c"/dev/zero", READ).map_err(|_| "cannot open /dev/zero")?;
    ensure(r::read(fd, &mut buf[..16]) == Ok(16), "read of /dev/zero")?;
    ensure(buf[..16].iter().all(|&b| b == 0), "/dev/zero gave non-zero bytes")?;
    r::close(fd).map_err(|_| "close failed")?;
    let fd = r::open(c"/dev/null", RDWR).map_err(|_| "cannot open /dev/null")?;
    ensure(r::read(fd, &mut buf) == Ok(0), "/dev/null read not empty")?;
    ensure(r::write(fd, b"gone") == Ok(4), "/dev/null write")?;
    r::close(fd).map_err(|_| "close failed")?;

    let n = slurp(c"/dev/pid", &mut buf)?;
    ensure(n == 12 && number(&buf[..n]).is_some_and(|p| p > 0), "/dev/pid")?;
    let n = slurp(c"/dev/user", &mut buf)?;
    ensure(&buf[..n] == b"eve", "/dev/user")?;
    let mut big = [0u8; 512];
    let n = slurp(c"#c/drivers", &mut big)?;
    let drivers = &big[..n];
    ensure(drivers.starts_with(b"#/ root\n#c cons\n"), "#c/drivers")?;

    let n = slurp(c"#r/rtc", &mut buf)?;
    ensure(number(&buf[..n]).is_some(), "#r/rtc gave no number")?;
    let n = slurp(c"#P/cputype", &mut buf)?;
    ensure(n > 0 && buf[n - 1] == b'\n', "#P/cputype")
}

/// `#e`: create, write, read back, stat, list, remove; creating in a bind
/// needs `CREATE`.
fn environment() -> Check {
    let fd = r::create(c"#e/greeting", RDWR, 0o666).map_err(|_| "create #e/greeting failed")?;
    ensure(r::write(fd, b"hello") == Ok(5), "env write")?;
    ensure(r::seek(fd, 0, Whence::Start) == Ok(0), "seek to 0")?;
    let mut buf = [0u8; 64];
    ensure(r::read(fd, &mut buf) == Ok(5) && &buf[..5] == b"hello", "env read back")?;
    ensure(r::seek(fd, -2, Whence::End) == Ok(3), "seek from the end")?;
    ensure(r::pwrite(fd, b"p!", 3) == Ok(2), "pwrite")?;
    ensure(r::pread(fd, &mut buf, 1) == Ok(4) && &buf[..4] == b"elp!", "pread")?;
    ensure(r::pwrite(fd, b"lo", 3) == Ok(2), "pwrite back")?;
    r::close(fd).map_err(|_| "close failed")?;
    fails(
        r::create(c"#e/greeting", mode_excl(), 0o666),
        b"file already exists",
        "exclusive create of an existing file",
    )?;

    r::bind(c"#e", c"/env", BindFlags::CREATE).map_err(|_| "bind -c #e /env failed")?;
    let n = slurp(c"/env/greeting", &mut buf)?;
    ensure(&buf[..n] == b"hello", "/env/greeting")?;
    let mut sb = [0u8; 128];
    let d = stat_dir(c"/env/greeting", &mut sb)?;
    ensure(
        d.name == "greeting" && d.length == 5 && !d.qid.is_dir(),
        "stat /env/greeting",
    )?;
    ensure(d.uid == "eve" && d.mode & 0o777 == 0o666, "stat owner and mode")?;
    let d = stat_dir(c"/env", &mut sb)?;
    ensure(d.name == "env" && d.qid.is_dir(), "stat /env")?;
    ensure(lists(c"/env", "greeting")?, "/env does not list greeting")?;

    let fd = r::create(c"/env/new", WRITE, 0o600).map_err(|_| "create in a -c bind failed")?;
    r::close(fd).map_err(|_| "close failed")?;
    r::remove(c"/env/new").map_err(|_| "remove failed")?;
    fails(r::open(c"/env/new", READ), b"file does not exist", "open after remove")?;

    r::bind(c"#e", c"/mnt", BindFlags::empty()).map_err(|_| "bind #e /mnt failed")?;
    ensure(lists(c"/mnt", "greeting")?, "/mnt does not list greeting")?;
    fails(
        r::create(c"/mnt/x", WRITE, 0o600),
        b"mounted directory forbids creation",
        "create in a bind without CREATE",
    )?;
    r::unmount(None, c"/mnt").map_err(|_| "unmount failed")?;
    ensure(!lists(c"/mnt", "greeting")?, "unmount left the bind")?;
    fails(r::unmount(None, c"/mnt"), b"not mounted", "unmount of nothing")
}

fn mode_excl() -> OpenMode {
    OpenMode {
        access: Access::Write,
        flags: OpenFlags::EXCL,
    }
}

/// `#r` before `#c` on `/dev`: a union that lists and walks both, and
/// takes one away again.
fn unions() -> Check {
    r::bind(c"#r", c"/dev", BindFlags::BEFORE).map_err(|_| "bind -b #r /dev failed")?;
    ensure(lists(c"/dev", "rtc")?, "/dev lacks rtc")?;
    ensure(lists(c"/dev", "cons")?, "/dev lacks cons")?;
    let mut buf = [0u8; 64];
    let n = slurp(c"/dev/rtc", &mut buf)?;
    ensure(number(&buf[..n]).is_some(), "/dev/rtc")?;
    r::unmount(Some(c"#r"), c"/dev").map_err(|_| "unmount #r /dev failed")?;
    ensure(!lists(c"/dev", "rtc")?, "rtc still in /dev")?;
    ensure(lists(c"/dev", "cons")?, "cons gone from /dev")?;
    // One member from a mount point that is not the first made: the
    // others, and the other mount points, stay.
    r::bind(c"#r", c"/env", BindFlags::BEFORE).map_err(|_| "bind -b #r /env failed")?;
    ensure(lists(c"/env", "rtc")?, "/env lacks rtc")?;
    r::unmount(Some(c"#r"), c"/env").map_err(|_| "unmount #r /env failed")?;
    ensure(!lists(c"/env", "rtc")?, "rtc still in /env")?;
    ensure(lists(c"/env", "greeting")?, "/env lost its own")?;
    ensure(lists(c"/dev", "cons")?, "unmounting from /env changed /dev")
}

/// `dup` shares the offset; `#d` opens to the same file and describes it.
fn descriptors() -> Check {
    let fd = r::open(c"/env/greeting", READ).map_err(|_| "cannot open /env/greeting")?;
    let fd2 = r::dup(fd, None).map_err(|_| "dup failed")?;
    ensure(fd2 != fd, "dup gave the same descriptor")?;
    let mut buf = [0u8; 256];
    ensure(
        r::read(fd2, &mut buf[..2]) == Ok(2) && &buf[..2] == b"he",
        "read through the dup",
    )?;
    ensure(
        r::read(fd, &mut buf[..3]) == Ok(3) && &buf[..3] == b"llo",
        "the offset is not shared",
    )?;
    ensure(r::dup(fd, Some(20)) == Ok(20), "dup to 20")?;
    r::close(20).map_err(|_| "close 20 failed")?;

    r::bind(c"#d", c"/fd", BindFlags::empty()).map_err(|_| "bind #d /fd failed")?;
    let mut nb = [0u8; 64];
    let name = name_of(&mut nb, b"/fd/", fd, b"");
    let fd3 = r::open(name, READ).map_err(|_| "cannot open /fd/n")?;
    ensure(
        r::pread(fd3, &mut buf, 0) == Ok(5) && &buf[..5] == b"hello",
        "/fd/n is not the file",
    )?;
    fails(
        r::open(name, WRITE),
        b"inappropriate use of fd",
        "/fd/n opened for writing",
    )?;
    let mut nb = [0u8; 64];
    let ctl = name_of(&mut nb, b"/fd/", fd, b"ctl");
    let n = slurp(ctl, &mut buf)?;
    let line = &buf[..n];
    ensure(line.ends_with(b" /env/greeting\n"), "/fd/nctl does not name the file")?;
    ensure(lists(c"/fd", "0ctl")?, "/fd does not list 0ctl")?;
    for f in [fd3, fd2, fd] {
        r::close(f).map_err(|_| "close failed")?;
    }
    Ok(())
}

/// `chdir`, relative names, `.` and `..`, in the programs' directory,
/// `/$cputype/bin` bound on `/bin`.
fn directories() -> Check {
    let cpu = r::getenv(b"cputype").ok_or("no $cputype")?;
    let bin = r::cstr(&[&b"/"[..], &cpu, b"/bin"].concat()).map_err(|_| "bad $cputype")?;
    r::bind(&bin, c"/bin", BindFlags::empty()).map_err(|_| "bind /$cputype/bin /bin failed")?;
    r::chdir(c"/bin").map_err(|_| "chdir /bin failed")?;
    let fd = r::open(c"targs", EXEC).map_err(|_| "cannot open targs relative to /bin")?;
    let mut path = [0u8; 64];
    let n = r::fd2path(fd, &mut path).map_err(|_| "fd2path failed")?;
    ensure(&path[..n] == b"/bin/targs", "relative name not made whole")?;
    r::close(fd).map_err(|_| "close failed")?;
    let mut sb = [0u8; 128];
    let d = stat_dir(c".", &mut sb)?;
    ensure(d.name == "bin" && d.qid.is_dir(), "stat .")?;
    r::chdir(c"..").map_err(|_| "chdir .. failed")?;
    let d = stat_dir(c"bin/./targs/../tmem", &mut sb)?;
    ensure(d.name == "tmem" && d.mode & 0o111 != 0, "stat through . and ..")?;
    let d = stat_dir(c"/", &mut sb)?;
    ensure(d.name == "/" && d.qid.is_dir(), "stat /")?;
    fails(r::chdir(c"/bin/targs"), b"not a directory", "chdir to a file")?;
    let fd = r::open(c"/", READ).map_err(|_| "cannot open /")?;
    fails(
        r::seek(fd, 5, Whence::Start),
        b"file is a directory",
        "seek in a directory",
    )?;
    r::close(fd).map_err(|_| "close failed")
}

/// What Plan 9 says when things go wrong.
fn errors() -> Check {
    fails(
        r::open(c"/env/greeting/x", READ),
        b"not a directory",
        "walk through a file",
    )?;
    fails(
        r::open(c"#Z", READ),
        b"unknown device in # filename",
        "an unknown device",
    )?;
    fails(
        r::open(c"/bin/targs", WRITE),
        b"permission denied",
        "write to the archive",
    )?;
    fails(
        r::open(c"/env", WRITE),
        b"file is a directory",
        "a directory opened to write",
    )?;
    let mut b = [0u8; 64];
    let n = Dir::DONT_TOUCH.encode(&mut b).unwrap_or(0);
    fails(r::wstat(c"#c/cons", &b[..n]), b"permission denied", "wstat on #c/cons")?;
    fails(
        r::wstat(c"#c/cons", &b[..n - 1]),
        b"malformed stat buffer",
        "a short wstat buffer",
    )?;
    fails(
        r::stat(c"/env/greeting", &mut b[..1]),
        b"stat buffer too small",
        "stat into 1 byte",
    )?;
    ensure(r::stat(c"/env/greeting", &mut b[..8]) == Ok(2), "stat into 8 bytes")?;
    ensure(
        usize::from(u16::from_le_bytes([b[0], b[1]])) > 6,
        "the size given is wrong",
    )?;
    fails(r::read(99, &mut b), b"fd out of range or not open", "read of fd 99")?;
    let fd = r::open(c"/env/greeting", READ).map_err(|_| "cannot open /env/greeting")?;
    fails(
        r::write(fd, b"x"),
        b"inappropriate use of fd",
        "write on a read-only fd",
    )?;
    r::close(fd).map_err(|_| "close failed")
}

/// Copies `/bin/targs` into `#e` and runs the copy.
fn exec_from_memory() -> Check {
    let mut buf = [0u8; 8192];
    let from = r::open(c"/bin/targs", READ).map_err(|_| "cannot open /bin/targs")?;
    let to = r::create(c"/env/targs", WRITE, 0o755).map_err(|_| "cannot create /env/targs")?;
    let mut total = 0;
    loop {
        let n = r::read(from, &mut buf).map_err(|_| "read of /bin/targs failed")?;
        if n == 0 {
            break;
        }
        ensure(r::write(to, &buf[..n]) == Ok(n), "write of /env/targs")?;
        total += n;
    }
    ensure(total > 4096, "targs is too small")?;
    for f in [from, to] {
        r::close(f).map_err(|_| "close failed")?;
    }
    let _ = r::exec(c"/env/targs", &[c"targs", c"from", c"env"]);
    Err("exec of /env/targs failed")
}
