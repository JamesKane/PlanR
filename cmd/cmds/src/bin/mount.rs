//! `mount [-a|-b] [-cnq] [-k keypattern] /srv/service dir [spec]`: mounts
//! the 9P server on the file `/srv/service` (opened for reading and
//! writing) on `dir`, as Plan 9's `mount`: its tree `spec`, in place of
//! `dir` or before (`-b`) or after (`-a`) it in a union; `-c` allows
//! creating there, `-C` caches. A server that asks for authentication is
//! answered through factotum unless `-n`; there is no factotum yet, so
//! that fails, and the mount goes ahead as Plan 9's does. `-q` keeps
//! quiet about failing, and exits normally.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use core::ffi::CStr;

use r::{Access, BindFlags, Error, OpenFlags, OpenMode, Rerr, Text};

r::main!(run);

fn usage() -> ! {
    r::eprintln!("usage: mount [-a|-b] [-cnq] [-k keypattern] /srv/service dir [spec]");
    r::exits(Some(b"usage"))
}

const RDWR: OpenMode = OpenMode {
    access: Access::ReadWrite,
    flags: OpenFlags::empty(),
};

/// libauth's `auth_proxy`, as far as it goes without a factotum: it
/// opens factotum's `rpc` file, and there is none.
fn auth_proxy(_afd: r::Fd, _params: &[u8]) -> Result<(), Error> {
    match r::open(c"/mnt/factotum/rpc", RDWR) {
        Ok(fd) => {
            let _ = r::close(fd);
            Err(Error::from_text(b"factotum's rpc is not yet supported"))
        }
        Err(e) => Err(Error::from_text(
            &[&b"opening /mnt/factotum/rpc: "[..], e.as_bytes()].concat(),
        )),
    }
}

/// libauth's `amount0`: authenticates, if the server asks, then mounts.
fn amount0(argv0: &[u8], fd: r::Fd, mntpt: &CStr, flags: BindFlags, aname: &CStr, keyspec: &[u8]) -> Result<(), Error> {
    let afd = r::fauth(fd, aname).ok();
    if let Some(a) = afd {
        let params = [&b"proto=p9any role=client "[..], keyspec].concat();
        if let Err(e) = auth_proxy(a, &params) {
            r::eprintln!("{}: auth_proxy: {e}", Text(argv0));
        }
    }
    let rv = r::mount(fd, afd, mntpt, flags, aname);
    if let Some(a) = afd {
        let _ = r::close(a);
    }
    rv
}

extern "C" fn catch(_: *mut r::Ureg, note: *const u8) {
    // SAFETY: the kernel passes the note's NUL-terminated text.
    let m = unsafe { CStr::from_ptr(note.cast()) }.to_bytes();
    r::eprintln!("mount: {}", Text(m));
    r::exits(Some(m))
}

fn run(cmdline: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(cmdline.iter());
    let mut flags = BindFlags::empty();
    let (mut quiet, mut noauth) = (false, false);
    let mut keyspec: &[u8] = b"";
    while let Some(c) = o.next() {
        match c {
            'a' => flags = flags | BindFlags::AFTER,
            'b' => flags = flags | BindFlags::BEFORE,
            'c' => flags = flags | BindFlags::CREATE,
            'C' => flags = flags | BindFlags::CACHE,
            'k' => keyspec = o.arg().unwrap_or_else(|| usage()),
            'n' => noauth = true,
            'q' => quiet = true,
            _ => usage(),
        }
    }
    let argv0 = o.argv0();
    let argv = o.rest();
    let spec: &[u8] = match argv.len() {
        2 => b"",
        3 => argv[2],
        _ => usage(),
    };
    if flags.contains(BindFlags::AFTER | BindFlags::BEFORE) {
        usage();
    }
    let fd = match r::cstr(argv[0]).and_then(|c| r::open(&c, RDWR)) {
        Ok(fd) => fd,
        Err(_) if quiet => return Ok(()),
        Err(_) => {
            r::eprintln!("{}: can't open {}: {Rerr}", Text(argv0), Text(argv[0]));
            return Err("open");
        }
    };
    let _ = r::notify(Some(catch));
    let rv = match (r::cstr(argv[1]), r::cstr(spec)) {
        (Ok(mntpt), Ok(spec)) if noauth => r::mount(fd, None, &mntpt, flags, &spec),
        (Ok(mntpt), Ok(spec)) => amount0(argv0, fd, &mntpt, flags, &spec, keyspec),
        (Err(e), _) | (_, Err(e)) => Err(e),
    };
    match rv {
        Ok(()) => Ok(()),
        Err(_) if quiet => Ok(()),
        Err(_) => {
            r::eprintln!("{}: mount {}: {Rerr}", Text(argv0), Text(argv[1]));
            Err("mount")
        }
    }
}
