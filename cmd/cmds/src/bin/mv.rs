//! `mv fromfile tofile`, `mv fromfile ... todir` (Plan 9's `mv.c`):
//! moves files, to a new name or into a directory: by renaming them where
//! they are if they stay in the same directory, else by copying them and
//! removing them. A file already at the new name is removed first. A
//! directory can be renamed but not moved.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::vec::Vec;

use r::{Access, DirBuf, OpenFlags, OpenMode, Text};

r::main!(run);

fn stat(name: &[u8]) -> Option<DirBuf> {
    r::cstr(name).ok().and_then(|c| r::dirstat(&c).ok())
}

fn is_dir(name: &[u8]) -> bool {
    stat(name).is_some_and(|d| d.is_dir())
}

/// `name`'s directory and last element, as Plan 9's `split`: `/x` is
/// in the directory with the empty name, and `..` is `.` in `..`.
fn split(name: &[u8]) -> (Vec<u8>, Vec<u8>) {
    match name.iter().rposition(|&b| b == b'/') {
        Some(i) => (name[..i].to_vec(), name[i + 1..].to_vec()),
        None if name == b".." => (b"..".to_vec(), b".".to_vec()),
        None => (b".".to_vec(), name.to_vec()),
    }
}

fn samefile(a: &[u8], b: &[u8]) -> bool {
    if a == b {
        return true;
    }
    match (stat(a), stat(b)) {
        (Some(a), Some(b)) => a.qid == b.qid && a.dev == b.dev && a.ty == b.ty,
        _ => false,
    }
}

/// Removes `name`, and whatever a union shows under it after; exits if
/// it cannot.
fn hardremove(name: &[u8]) {
    let Ok(c) = r::cstr(name) else { return };
    if let Err(e) = r::remove(&c) {
        r::eprintln!("mv: can't remove {}: {e}", Text(name));
        r::exits(Some(b"mv"));
    }
    while r::remove(&c).is_ok() {}
}

fn copy1(fdf: r::Fd, fdt: r::Fd, from: &[u8], to: &[u8]) -> bool {
    let mut buf = alloc::vec![0u8; 8192];
    loop {
        match r::read(fdf, &mut buf) {
            Ok(0) => return true,
            Ok(n) => {
                let e = match r::write(fdt, &buf[..n]) {
                    Ok(m) if m == n => continue,
                    Ok(_) => r::Error::from_text(b"short write"),
                    Err(e) => e,
                };
                r::eprintln!("mv: error writing {}: {e}", Text(to));
                return false;
            }
            Err(e) => {
                r::eprintln!("mv: error reading {}: {e}", Text(from));
                return false;
            }
        }
    }
}

/// Moves `from` to `toelem` (or its own last element) in `todir`; false
/// (having said why) if it cannot.
fn mv(from: &[u8], todir: &[u8], toelem: Option<&[u8]>) -> bool {
    let ft = Text(from);
    let dirb = match r::cstr(from).and_then(|c| r::dirstat(&c)) {
        Ok(d) => d,
        Err(e) => {
            r::eprintln!("mv: can't stat {ft}: {e}");
            return false;
        }
    };
    let (fromdir, fromelem) = split(from);
    let toelem = toelem.unwrap_or(&fromelem);
    if toelem.is_empty() {
        r::eprintln!("mv: null last name element moving {ft}");
        return false;
    }
    let toname = [todir, b"/", toelem].concat();
    let tt = Text(&toname);
    if samefile(&fromdir, todir) {
        if samefile(from, &toname) {
            r::eprintln!("mv: {ft} and {tt} are the same");
            return false;
        }
        if stat(&toname).is_some() {
            hardremove(&toname);
        }
        let mut n = DirBuf::null();
        n.name = alloc::string::String::from_utf8_lossy(toelem).into_owned();
        let renamed = r::cstr(from).and_then(|c| r::dirwstat(&c, &n));
        match renamed {
            Ok(()) => return true,
            Err(e) if dirb.is_dir() => {
                r::eprintln!("mv: can't rename directory {ft}: {e}");
                return false;
            }
            Err(_) => {}
        }
    }
    // Renaming won't do: copy.
    if dirb.is_dir() {
        r::eprintln!("mv: {ft} is a directory, not copied to {tt}");
        return false;
    }
    let read = OpenMode {
        access: Access::Read,
        flags: OpenFlags::empty(),
    };
    let fdf = match r::cstr(from).and_then(|c| r::open(&c, read)) {
        Ok(fd) => fd,
        Err(e) => {
            r::eprintln!("mv: can't open {ft}: {e}");
            return false;
        }
    };
    // create does not truncate an append-only file.
    if stat(&toname).is_some_and(|d| d.mode & abi::DMAPPEND != 0) {
        hardremove(&toname);
    }
    let write = OpenMode {
        access: Access::Write,
        flags: OpenFlags::empty(),
    };
    let fdt = match r::cstr(&toname).and_then(|c| r::create(&c, write, dirb.mode)) {
        Ok(fd) => fd,
        Err(e) => {
            r::eprintln!("mv: can't create {tt}: {e}");
            let _ = r::close(fdf);
            return false;
        }
    };
    let mut ok = copy1(fdf, fdt, from, &toname);
    let _ = r::close(fdf);
    if ok {
        let mut n = DirBuf::null();
        n.mtime = dirb.mtime;
        n.mode = dirb.mode;
        // Errors are ignored: user none, for one, always fails.
        let _ = r::dirfwstat(fdt, &n);
        if let Err(e) = r::cstr(from).and_then(|c| r::remove(&c)) {
            r::eprintln!("mv: can't remove {ft}: {e}");
            ok = false;
        }
    }
    let _ = r::close(fdt);
    ok
}

fn run(args: r::Args) -> Result<(), &'static str> {
    if args.len() < 3 {
        r::eprintln!("usage: mv fromfile tofile");
        r::eprintln!("\t  mv fromfile ... todir");
        return Err("bad usage");
    }
    let names: Vec<Vec<u8>> = args.iter().skip(1).map(r::cleanname).collect();
    let (to, from) = names.split_last().unwrap_or((&names[0], &[]));
    let (todir, toelem) = if is_dir(to) {
        if from.len() == 1 && is_dir(&from[0]) {
            // mv dir1 dir2
            let (d, e) = split(to);
            (d, Some(e))
        } else {
            // mv file ... dir
            (to.clone(), None)
        }
    } else {
        // mv file1 file2
        let (d, e) = split(to);
        (d, Some(e))
    };
    if from.len() > 1 && toelem.is_some() {
        r::eprintln!("mv: {} not a directory", Text(to));
        return Err("bad usage");
    }
    let mut failed = false;
    for f in from {
        failed |= !mv(f, &todir, toelem.as_deref());
    }
    if failed { Err("failure") } else { Ok(()) }
}
