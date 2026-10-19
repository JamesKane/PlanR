//! `ramfs`: a file system held in memory, as Plan 9's `ramfs(4)`.
//!
//! ```text
//! ramfs [-Di] [-m mtpt] [-s | -S srvname]
//! ```
//!
//! It starts a server process and mounts the empty tree it serves on
//! `mtpt` (`/tmp` by default), replacing what is there and taking
//! creates, then exits; the server lives until the last reference to the
//! mount goes. `-s` posts the channel as `/srv/ramfs`, `-S` as
//! `/srv/srvname`; either mounts it only if `-m` is given too. `-i`
//! serves 9P on descriptors 0 and 1 instead, without forking. `-D` traces
//! every message on standard error.
//!
//! The files belong to whoever runs `ramfs` (`#c/user`) and vanish with
//! the server.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use core::ffi::CStr;
use core::fmt::Write as _;

use ninep::ram::RamFs;
use ninep::srv::{Ename, Srv, Transport};
use r::{Access, BindFlags, OpenFlags, OpenMode, RforkFlags as F};

r::main!(run);

/// The largest message the server takes: 64 KiB of data.
#[allow(clippy::cast_possible_truncation)] // 24
const MSIZE: u32 = 64 * 1024 + ninep::IOHDRSZ as u32;

fn usage() -> ! {
    r::eprintln!("usage: ramfs [-Di] [-m mtpt] [-s | -S srvname]");
    r::exits(Some(b"usage"))
}

/// Prints `ramfs: what: error` and exits with it.
fn fatal(what: &str, e: &dyn core::fmt::Display) -> ! {
    let mut msg = String::new();
    let _ = write!(msg, "ramfs: {what}: {e}");
    r::eprintln!("{msg}");
    r::exits(Some(msg.as_bytes()))
}

fn now() -> u32 {
    u32::try_from(r::time::time()).unwrap_or(u32::MAX)
}

/// 9P read from one descriptor and written to another.
struct Fds {
    rfd: r::Fd,
    wfd: r::Fd,
}

fn ename(e: &r::Error) -> Ename {
    String::from_utf8_lossy(e.as_bytes()).into_owned().into()
}

impl Transport for Fds {
    fn read(&mut self, buf: &mut [u8]) -> ninep::srv::Result<usize> {
        r::read(self.rfd, buf).map_err(|e| ename(&e))
    }

    fn write(&mut self, buf: &[u8]) -> ninep::srv::Result<usize> {
        r::write(self.wfd, buf).map_err(|e| ename(&e))
    }
}

fn serve(srv: &mut Srv<RamFs>, rfd: r::Fd, wfd: r::Fd) -> ! {
    match srv.serve(&mut Fds { rfd, wfd }) {
        Ok(()) => r::exits(None),
        Err(e) => fatal("serving", &e),
    }
}

#[allow(clippy::unnecessary_wraps)] // the signature `main!` wants
fn run(args: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(args.iter());
    let (mut chatty, mut stdio) = (false, false);
    let (mut mtpt, mut srvname) = (None, None);
    while let Some(c) = o.next() {
        match c {
            'D' => chatty = true,
            'i' => stdio = true,
            'm' => mtpt = Some(o.arg().unwrap_or_else(|| usage())),
            's' => srvname = Some(&b"ramfs"[..]),
            'S' => srvname = Some(o.arg().unwrap_or_else(|| usage())),
            _ => usage(),
        }
    }
    if !o.rest().is_empty() {
        usage();
    }
    let uid = r::read_all(c"#c/user").unwrap_or_else(|_| b"none".to_vec());
    let uid = String::from_utf8_lossy(&uid);
    let mut srv = Srv::new(RamFs::new(uid.trim(), now), MSIZE);
    if chatty {
        srv.set_chatty(Box::new(|m| r::eprintln!("ramfs: {m}")));
    }
    if stdio {
        serve(&mut srv, 0, 1);
    }
    let mtpt = match mtpt {
        Some(m) => Some(r::cstr(m).unwrap_or_else(|e| fatal("bad mount point", &e))),
        None if srvname.is_none() => Some(c"/tmp".into()),
        None => None,
    };

    let [c, s] = r::pipe().unwrap_or_else(|e| fatal("pipe", &e));
    // The server: its own name space, so it does not see its own mount,
    // and nobody's child, so nobody need wait for it.
    match r::rfork(F::PROC | F::FDG | F::NAMEG | F::NOTEG | F::NOWAIT) {
        Err(e) => fatal("rfork", &e),
        Ok(0) => {
            let _ = r::close(c);
            serve(&mut srv, s, s);
        }
        Ok(_) => {}
    }
    let _ = r::close(s);
    if let Some(name) = srvname {
        post(name, c);
    }
    if let Some(m) = &mtpt {
        mount(c, m);
    }
    let _ = r::close(c);
    Ok(())
}

/// Posts descriptor `fd` as `/srv/name`.
fn post(name: &[u8], fd: r::Fd) {
    let mut path = b"/srv/".to_vec();
    path.extend_from_slice(name);
    let path = r::cstr(&path).unwrap_or_else(|e| fatal("bad service name", &e));
    let mode = OpenMode {
        access: Access::Write,
        flags: OpenFlags::empty(),
    };
    let f = r::create(&path, mode, 0o600).unwrap_or_else(|e| fatal("create /srv", &e));
    if let Err(e) = r::fprint!(f, "{fd}") {
        fatal("post", &e);
    }
    let _ = r::close(f);
}

fn mount(fd: r::Fd, mtpt: &CStr) {
    if let Err(e) = r::mount(fd, None, mtpt, BindFlags::CREATE, c"") {
        fatal("mount", &e);
    }
}
