//! `#d`, the file descriptors: for each descriptor `n` of the caller, a
//! file `n` that opens to the same chan, so to the same file and offset,
//! and a file `nctl` that describes it, as Plan 9's `devdup` does.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::{Access, DMDIR, OpenFlags, OpenMode, QTDIR, QTFILE, Qid};

use crate::port::chan::Chan;
use crate::port::dev::{self, Dev, Entry};
use crate::port::error::{EBADSPEC, EBADUSEFD, ENONEXIST, EPERM, Result};
use crate::port::pgrp;

pub(crate) struct Dup;

pub(crate) static DUP: Dup = Dup;

const DIR: Qid = Qid {
    path: 0,
    vers: 0,
    ty: QTDIR,
};

/// Descriptor `fd`'s file, and its `ctl` file.
fn qid(fd: usize, ctl: bool) -> Qid {
    Qid {
        path: 2 * fd as u64 + 1 + u64::from(ctl),
        vers: 0,
        ty: QTFILE,
    }
}

/// The descriptor and whether it is the `ctl` file, for qid path `path`.
fn which(path: u64) -> (u64, bool) {
    ((path - 1) / 2, path.is_multiple_of(2))
}

fn entry(fd: usize, ctl: bool, c: &Chan) -> Entry {
    let name = if ctl {
        alloc::format!("{fd}ctl")
    } else {
        alloc::format!("{fd}")
    };
    let perm = if ctl {
        0o400
    } else {
        match c.mode.map(|m| m.access) {
            Some(Access::Write) => 0o200,
            Some(Access::ReadWrite) => 0o600,
            _ => 0o400,
        }
    };
    Entry::new(&name, qid(fd, ctl), 0, perm)
}

fn dirgen(_: &Chan, i: usize) -> Option<Entry> {
    let fds = pgrp::fgrp().ok()?.list();
    let (fd, c) = fds.get(i / 2)?;
    Some(entry(*fd, i % 2 == 1, c))
}

/// Descriptor `fd`'s line, as Plan 9 prints `/proc/n/fd`.
fn describe(fd: u64, c: &Chan) -> String {
    let mode = match c.mode.map(|m| m.access) {
        Some(Access::Write) => "w ",
        Some(Access::ReadWrite) => "rw",
        _ => "r ",
    };
    alloc::format!(
        "{fd:>3} {mode} {} {:>4} ({:016x} {} {:02x}) {:>5} {:>8} {}\n",
        c.dev.dc(),
        c.devno,
        c.qid.path,
        c.qid.vers,
        c.qid.ty,
        0,
        c.offset.load(core::sync::atomic::Ordering::Relaxed),
        c.name
    )
}

impl Dev for Dup {
    fn dc(&self) -> char {
        'd'
    }

    fn name(&self) -> &'static str {
        "dup"
    }

    fn attach(&self, spec: &str) -> Result<Chan> {
        if !spec.is_empty() {
            return Err(EBADSPEC.into());
        }
        Ok(Chan::new(&DUP, 0, DIR, String::from("#d")))
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Chan> {
        let (digits, ctl) = match name.strip_suffix("ctl") {
            Some(d) => (d, true),
            None => (name, false),
        };
        // Only the number as it is listed: no sign, no leading zeros.
        let fd: usize = digits.parse().map_err(|_| ENONEXIST)?;
        if alloc::format!("{fd}") != digits {
            return Err(ENONEXIST.into());
        }
        pgrp::fgrp()?.get(fd as u64).map_err(|_| ENONEXIST)?;
        Ok(c.child(qid(fd, ctl), name))
    }

    fn stat(&self, c: &Chan) -> Result<Vec<u8>> {
        if c.qid.is_dir() {
            return Ok(Entry::new("#d", DIR, 0, DMDIR | 0o555).encode(self, c.devno));
        }
        let (fd, ctl) = which(c.qid.path);
        let f = pgrp::fgrp()?.get(fd).map_err(|_| ENONEXIST)?;
        #[allow(clippy::cast_possible_truncation)] // a descriptor
        Ok(entry(fd as usize, ctl, &f).encode(self, c.devno))
    }

    fn open(&self, c: Chan, mode: OpenMode) -> Result<Arc<Chan>> {
        if c.qid.is_dir() {
            return dev::open_checked(c, mode, DMDIR | 0o555);
        }
        let (fd, ctl) = which(c.qid.path);
        let f = pgrp::fgrp()?.get(fd)?;
        if ctl {
            return dev::open_checked(c, mode, 0o400);
        }
        // The same chan, if the mode is one it was opened for.
        let had = f.mode.map_or(Access::Read, |m| m.access);
        let ok = had == Access::ReadWrite
            || (mode.access == had && !(had == Access::Read && mode.flags.contains(OpenFlags::TRUNC)));
        if !ok {
            return Err(EBADUSEFD.into());
        }
        Ok(f)
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        if c.qid.is_dir() {
            return dev::dirread_gen(c, buf, offset, &dirgen);
        }
        let (fd, ctl) = which(c.qid.path);
        if !ctl {
            return Err(EPERM.into());
        }
        let f = pgrp::fgrp()?.get(fd)?;
        Ok(dev::read_bytes(describe(fd, &f).as_bytes(), buf, offset))
    }
}
