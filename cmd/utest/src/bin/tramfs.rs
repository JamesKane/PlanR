//! `ramfs` through the kernel: the one `/sys/test/run` mounted on `/tmp`,
//! and a second one posted in `/srv` and mounted twice. Files written, read,
//! truncated and renamed (`dirwstat`), permissions, directories read
//! across several messages, a megabyte file, `ORCLOSE`, a removed file
//! still open (gone, as on Plan 9), two processes writing at once, and
//! the server's errors, which are Plan 9's `ramfs.c`'s.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::vec::Vec;
use core::ffi::CStr;

use abi::DMDIR;
use r::{Access, BindFlags, OpenFlags, OpenMode, RforkFlags as F};

r::main!(run);

type Check = Result<(), &'static str>;

fn ensure(ok: bool, why: &'static str) -> Check {
    if ok { Ok(()) } else { Err(why) }
}

const fn mode(access: Access, flags: OpenFlags) -> OpenMode {
    OpenMode { access, flags }
}

const READ: OpenMode = mode(Access::Read, OpenFlags::empty());
const WRITE: OpenMode = mode(Access::Write, OpenFlags::empty());
const RDWR: OpenMode = mode(Access::ReadWrite, OpenFlags::empty());

/// `r` must have failed with `text`.
fn fails<T>(r: Result<T, r::Error>, text: &[u8], why: &'static str) -> Check {
    match r.err() {
        None => Err(why),
        Some(e) if e.as_bytes() == text => Ok(()),
        Some(e) => {
            r::eprintln!("tramfs: wrong error: {e}");
            Err(why)
        }
    }
}

fn put(name: &CStr, data: &[u8]) -> Check {
    let fd = r::create(name, WRITE, 0o664).map_err(|_| "create failed")?;
    let r = r::io::Write::write_all(&mut r::File(fd), data).map_err(|_| "write failed");
    r::close(fd).map_err(|_| "close failed")?;
    r
}

fn get(name: &CStr) -> Result<Vec<u8>, &'static str> {
    r::read_all(name).map_err(|_| "read failed")
}

fn run(_: r::Args) -> Check {
    files()?;
    wstat()?;
    directories()?;
    big()?;
    removed()?;
    concurrent()?;
    second()
}

fn files() -> Check {
    put(c"/tmp/a", b"hello, world")?;
    ensure(get(c"/tmp/a")? == b"hello, world", "contents")?;
    let fd = r::open(c"/tmp/a", RDWR).map_err(|_| "open failed")?;
    r::pwrite(fd, b"there", 7).map_err(|_| "pwrite failed")?;
    let mut b = [0u8; 32];
    let n = r::pread(fd, &mut b, 0).map_err(|_| "pread failed")?;
    ensure(&b[..n] == b"hello, there", "pwrite then pread")?;
    let d = r::dirfstat(fd).map_err(|_| "dirfstat failed")?;
    r::close(fd).map_err(|_| "close failed")?;
    ensure(
        d.name == "a" && d.length == 12 && d.mode == 0o664 && d.uid == "eve" && d.ty == u16::from(b'M'),
        "the stat of a file",
    )?;
    // create on a file that exists truncates it.
    put(c"/tmp/a", b"new")?;
    ensure(get(c"/tmp/a")? == b"new", "create truncates")?;
    fails(
        r::open(c"/tmp/nonesuch", READ),
        b"file does not exist",
        "open of nothing",
    )?;
    let excl = mode(Access::Write, OpenFlags::EXCL);
    fails(
        r::create(c"/tmp/a", excl, 0o664),
        b"file already exists",
        "exclusive create",
    )?;
    // The owner's bits decide, for the owner too.
    let mut n = r::DirBuf::null();
    n.mode = 0o200;
    r::dirwstat(c"/tmp/a", &n).map_err(|_| "chmod failed")?;
    fails(
        r::open(c"/tmp/a", READ),
        b"permission denied",
        "open without permission",
    )?;
    r::remove(c"/tmp/a").map_err(|_| "remove failed")
}

fn wstat() -> Check {
    put(c"/tmp/old", b"0123456789")?;
    put(c"/tmp/other", b"")?;
    let mut d = r::DirBuf::null();
    d.name = "other".into();
    fails(r::dirwstat(c"/tmp/old", &d), b"file exists", "rename onto a file")?;
    d.name = "new".into();
    d.length = 4;
    r::dirwstat(c"/tmp/old", &d).map_err(|_| "rename failed")?;
    fails(r::dirstat(c"/tmp/old"), b"file does not exist", "the old name stays")?;
    ensure(get(c"/tmp/new")? == b"0123", "rename and truncate")?;
    let s = r::dirstat(c"/tmp/new").map_err(|_| "dirstat failed")?;
    ensure(s.name == "new" && s.length == 4, "the stat after wstat")?;
    // A file cannot become a directory: Plan 9's ramfs keeps the bit
    // as it was, and takes the rest of the mode.
    let mut d = r::DirBuf::null();
    d.mode = DMDIR | 0o640;
    r::dirwstat(c"/tmp/new", &d).map_err(|_| "chmod with DMDIR failed")?;
    ensure(
        r::dirstat(c"/tmp/new").is_ok_and(|d| d.mode == 0o640),
        "a file made a directory",
    )?;
    r::remove(c"/tmp/new").map_err(|_| "remove failed")?;
    r::remove(c"/tmp/other").map_err(|_| "remove failed")
}

/// Enough entries that a directory read takes several messages.
const ENTRIES: usize = 600;

fn entry(i: usize) -> alloc::ffi::CString {
    r::cstr(format!("/tmp/d/entry-with-a-long-name-{i:04}").as_bytes()).expect("no NUL")
}

fn directories() -> Check {
    let fd = r::create(c"/tmp/d", READ, DMDIR | 0o775).map_err(|_| "mkdir failed")?;
    r::close(fd).map_err(|_| "close failed")?;
    ensure(
        r::dirstat(c"/tmp/d").is_ok_and(|d| d.is_dir()),
        "/tmp/d is not a directory",
    )?;
    fails(
        r::open(c"/tmp/d", WRITE),
        b"permission denied",
        "a directory opened to write",
    )?;
    for i in 0..ENTRIES {
        put(&entry(i), b"x")?;
    }
    let fd = r::open(c"/tmp/d", READ).map_err(|_| "open failed")?;
    let all = r::dirreadall(fd).map_err(|_| "dirreadall failed")?;
    r::close(fd).map_err(|_| "close failed")?;
    ensure(all.len() == ENTRIES, "dirreadall's count")?;
    ensure(
        all.iter()
            .enumerate()
            .all(|(i, d)| d.name == format!("entry-with-a-long-name-{i:04}") && d.length == 1),
        "the entries",
    )?;
    fails(
        r::remove(c"/tmp/d"),
        b"directory not empty",
        "rmdir of a full directory",
    )?;
    for i in 0..ENTRIES {
        r::remove(&entry(i)).map_err(|_| "remove failed")?;
    }
    r::remove(c"/tmp/d").map_err(|_| "rmdir failed")
}

/// A megabyte, written and read in pieces of every size.
fn big() -> Check {
    let data: Vec<u8> = (0..1u32 << 20).map(|i| (i * 7 + i / 251).to_le_bytes()[0]).collect();
    let fd = r::create(c"/tmp/big", RDWR, 0o664).map_err(|_| "create failed")?;
    let mut at = 0;
    for k in (1..).map(|i| i * 4099 % 70_000 + 1) {
        if at == data.len() {
            break;
        }
        let k = k.min(data.len() - at);
        let n = r::write(fd, &data[at..at + k]).map_err(|_| "write failed")?;
        ensure(n == k, "a short write")?;
        at += k;
    }
    r::close(fd).map_err(|_| "close failed")?;
    ensure(get(c"/tmp/big")? == data, "a megabyte back")?;
    ensure(r::dirstat(c"/tmp/big").is_ok_and(|d| d.length == 1 << 20), "its length")?;
    r::remove(c"/tmp/big").map_err(|_| "remove failed")
}

fn removed() -> Check {
    put(c"/tmp/gone", b"still here")?;
    let fd = r::open(c"/tmp/gone", READ).map_err(|_| "open failed")?;
    r::remove(c"/tmp/gone").map_err(|_| "remove failed")?;
    // Plan 9's ramfs frees a removed file at once, open or not.
    let mut b = [0u8; 32];
    fails(r::read(fd, &mut b), b"file does not exist", "read of a removed file")?;
    r::close(fd).map_err(|_| "close failed")?;
    // ORCLOSE.
    let rclose = mode(Access::Write, OpenFlags::RCLOSE);
    let fd = r::create(c"/tmp/rclose", rclose, 0o664).map_err(|_| "create failed")?;
    ensure(r::dirstat(c"/tmp/rclose").is_ok(), "the ORCLOSE file is not there")?;
    r::close(fd).map_err(|_| "close failed")?;
    fails(r::dirstat(c"/tmp/rclose"), b"file does not exist", "ORCLOSE")
}

/// Two processes writing their own files, line by line, at once.
fn concurrent() -> Check {
    let names = [c"/tmp/one", c"/tmp/two"];
    for (i, &name) in names.iter().enumerate() {
        match r::rfork(F::PROC | F::FDG) {
            Err(_) => return Err("rfork failed"),
            Ok(0) => {
                let ok = (|| {
                    let fd = r::create(name, WRITE, 0o664).ok()?;
                    for n in 0..200 {
                        r::fprint!(fd, "{i} {n}\n").ok()?;
                    }
                    r::close(fd).ok()
                })();
                r::exits(if ok.is_some() { None } else { Some(b"writer failed") })
            }
            Ok(_) => {}
        }
    }
    for _ in 0..2 {
        let w = r::wait().map_err(|_| "wait failed")?;
        ensure(w.msg.is_empty(), "a writer failed")?;
    }
    for (i, &name) in names.iter().enumerate() {
        let want: Vec<u8> = (0..200).flat_map(|n| format!("{i} {n}\n").into_bytes()).collect();
        ensure(get(name)? == want, "a writer's file")?;
        r::remove(name).map_err(|_| "remove failed")?;
    }
    Ok(())
}

/// Runs `ramfs` with `argv` in this name space; it must exit normally.
fn ramfs(argv: &[&CStr]) -> Check {
    match r::rfork(F::PROC | F::FDG) {
        Err(_) => Err("rfork failed"),
        Ok(0) => {
            let e = r::exec(c"/bin/ramfs", argv);
            r::exits(Some(e.as_bytes()))
        }
        Ok(pid) => {
            let w = r::wait().map_err(|_| "wait failed")?;
            ensure(w.pid == pid && w.msg.is_empty(), "ramfs failed")
        }
    }
}

/// A second `ramfs`, posted in `/srv` (with `-S` alone: as on Plan 9,
/// `-m` would mount it instead), then mounted from there on `/n` and on
/// `/mnt`: one tree, apart from `/tmp`'s.
fn second() -> Check {
    ramfs(&[c"ramfs", c"-S", c"tramfs"])?;
    let fd = r::open(c"/srv/tramfs", RDWR).map_err(|_| "open /srv/tramfs failed")?;
    r::mount(fd, None, c"/n", BindFlags::CREATE, c"").map_err(|_| "mount from /srv failed")?;
    // The mount closed it, as Plan 9's does.
    fails(r::close(fd), b"fd out of range or not open", "mount left its fd open")?;
    put(c"/n/file", b"in the second")?;
    fails(
        r::dirstat(c"/tmp/file"),
        b"file does not exist",
        "the second is not /tmp",
    )?;
    let fd = r::open(c"/srv/tramfs", RDWR).map_err(|_| "open /srv/tramfs failed")?;
    r::mount(fd, None, c"/mnt", BindFlags::empty(), c"").map_err(|_| "mount from /srv failed")?;
    // The mount closed it, as Plan 9's does.
    fails(r::close(fd), b"fd out of range or not open", "mount left its fd open")?;
    ensure(get(c"/mnt/file")? == b"in the second", "the same tree twice")?;
    fails(
        r::create(c"/mnt/x", WRITE, 0o664),
        b"mounted directory forbids creation",
        "a mount without -c",
    )?;
    let fd = r::open(c"/mnt", READ).map_err(|_| "open failed")?;
    let all = r::dirreadall(fd).map_err(|_| "dirreadall failed")?;
    r::close(fd).map_err(|_| "close failed")?;
    ensure(all.len() == 1 && all[0].name == "file", "the second tree's root")?;
    // Its users gone, the server ends.
    r::unmount(None, c"/mnt").map_err(|_| "unmount failed")?;
    r::unmount(None, c"/n").map_err(|_| "unmount failed")?;
    r::remove(c"/srv/tramfs").map_err(|_| "remove from /srv failed")
}
