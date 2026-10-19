//! The system calls on files and name spaces (Plan 9's `sysfile.c`):
//! `open`, `create`, `close`, `read`, `write`, `pread`, `pwrite`, `seek`,
//! `stat`, `fstat`, `wstat`, `fwstat`, `remove`, `fd2path`, `dup`,
//! `pipe`, `chdir`, `bind` and `unmount`.
//!
//! Data passes through a kernel buffer: a read gives at most [`IO_MAX`]
//! bytes, and a write goes to the device in pieces that size.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use abi::{Access, BindFlags, Dir, OpenFlags, OpenMode, Whence};

use crate::port::chan::{self, Chan, last_elem};
use crate::port::dev::Dev;
use crate::port::devpipe::PIPE;
use crate::port::error::{EBADARG, EBADCHAR, EBADSTAT, EBADUSEFD, EISDIR, ENOTDIR, ESHORTSTAT, Result};
use crate::port::pgrp::{self, NameSpace};
use crate::port::uaccess;

/// The most bytes a read returns, and the size of a write's pieces.
pub(crate) const IO_MAX: usize = 128 * 1024;
/// The longest file name a call takes.
pub(crate) const NAME_MAX: usize = 1024;

/// A name argument: NUL-terminated UTF-8.
pub(crate) fn name_arg(p: u64) -> Result<String> {
    let b = uaccess::copy_in_str(p, NAME_MAX)?;
    String::from_utf8(b).map_err(|_| EBADCHAR.into())
}

fn mode_arg(raw: u64) -> Result<OpenMode> {
    OpenMode::from_raw(raw).ok_or_else(|| EBADARG.into())
}

/// An offset argument: a non-negative `i64`.
fn offset_arg(raw: u64) -> Result<u64> {
    i64::try_from(raw)
        .map(|_| raw)
        .map_err(|_| "negative i/o offset".into())
}

fn chan_of(fd: u64) -> Result<Arc<Chan>> {
    pgrp::fgrp()?.get(fd)
}

/// `open(name, mode) -> fd`
pub(crate) fn open(name: u64, mode: u64) -> Result<u64> {
    let name = name_arg(name)?;
    let mode = mode_arg(mode)?;
    let c = {
        let ns = NameSpace::current()?;
        chan::open(&ns.names(), &name, mode)?
    };
    let fd = pgrp::fgrp()?.add(c, mode.flags.contains(OpenFlags::CEXEC))?;
    Ok(fd as u64)
}

/// `create(name, mode, perm) -> fd`
pub(crate) fn create(name: u64, mode: u64, perm: u64) -> Result<u64> {
    let name = name_arg(name)?;
    let mode = mode_arg(mode)?;
    let perm = u32::try_from(perm).map_err(|_| EBADARG)?;
    let c = {
        let ns = NameSpace::current()?;
        chan::create(&ns.names(), &name, mode, perm)?
    };
    let fd = pgrp::fgrp()?.add(c, mode.flags.contains(OpenFlags::CEXEC))?;
    Ok(fd as u64)
}

/// `close(fd)`
pub(crate) fn close(fd: u64) -> Result<u64> {
    let c = pgrp::fgrp()?.close(fd)?;
    drop(c);
    Ok(0)
}

/// `read(fd, buf, n)` at the descriptor's offset, or `pread` at `offset`.
pub(crate) fn read(fd: u64, buf: u64, n: u64, offset: Option<u64>) -> Result<u64> {
    let offset = offset.map(offset_arg).transpose()?;
    let c = chan_of(fd)?;
    if !c.readable() {
        return Err(EBADUSEFD.into());
    }
    let n = usize::try_from(n).unwrap_or(usize::MAX).min(IO_MAX);
    uaccess::check(buf, n)?;
    let mut k = alloc::vec![0u8; n];
    let at = offset.unwrap_or_else(|| c.offset.load(Ordering::Relaxed));
    let got = c.read(&mut k, at)?.min(n);
    uaccess::copy_out(buf, &k[..got])?;
    if offset.is_none() {
        c.offset.fetch_add(got as u64, Ordering::Relaxed);
    }
    Ok(got as u64)
}

/// `write(fd, buf, n)` at the descriptor's offset, or `pwrite` at
/// `offset`.
pub(crate) fn write(fd: u64, buf: u64, n: u64, offset: Option<u64>) -> Result<u64> {
    let offset = offset.map(offset_arg).transpose()?;
    let c = chan_of(fd)?;
    if !c.writable() {
        return Err(EBADUSEFD.into());
    }
    if c.qid.is_dir() {
        return Err(EISDIR.into());
    }
    let n = usize::try_from(n).map_err(|_| EBADARG)?;
    uaccess::check(buf, n)?;
    let at = offset.unwrap_or_else(|| c.offset.load(Ordering::Relaxed));
    let mut k = alloc::vec![0u8; n.min(IO_MAX)];
    let mut done = 0;
    while done < n {
        let len = (n - done).min(IO_MAX);
        uaccess::copy_in(&mut k[..len], buf + done as u64)?;
        let w = c.dev.write(&c, &k[..len], at + done as u64)?.min(len);
        done += w;
        if w < len {
            break;
        }
    }
    if offset.is_none() {
        c.offset.fetch_add(done as u64, Ordering::Relaxed);
    }
    Ok(done as u64)
}

/// The chan's stat, named by the last element of its name, as Plan 9's
/// `dirsetname` does.
fn stat_of(c: &Chan) -> Result<Vec<u8>> {
    let b = c.dev.stat(c)?;
    let (d, _) = Dir::decode(&b).ok_or(EBADSTAT)?;
    let d = Dir {
        name: last_elem(&c.name),
        ..d
    };
    let mut out = alloc::vec![0u8; d.size()];
    d.encode(&mut out).ok_or(EBADSTAT)?;
    Ok(out)
}

/// Copies stat `s` out to a buffer of `n` bytes; one too small gets just
/// the size, so the caller can try again with room for it.
fn stat_out(s: &[u8], buf: u64, n: u64) -> Result<u64> {
    let n = usize::try_from(n).unwrap_or(usize::MAX);
    if n < 2 {
        return Err(ESHORTSTAT.into());
    }
    let len = if n < s.len() { 2 } else { s.len() };
    uaccess::copy_out(buf, &s[..len])?;
    Ok(len as u64)
}

/// `stat(name, buf, n) -> n`
pub(crate) fn stat(name: u64, buf: u64, n: u64) -> Result<u64> {
    let name = name_arg(name)?;
    let s = {
        let ns = NameSpace::current()?;
        let w = chan::walk(&ns.names(), &name, true)?;
        stat_of(&w.chan)?
    };
    stat_out(&s, buf, n)
}

/// `fstat(fd, buf, n) -> n`
pub(crate) fn fstat(fd: u64, buf: u64, n: u64) -> Result<u64> {
    let s = stat_of(&*chan_of(fd)?)?;
    stat_out(&s, buf, n)
}

/// The `Dir` a `wstat` call passes, in `n` bytes at `buf`.
fn stat_in(buf: u64, n: u64) -> Result<Vec<u8>> {
    let n = usize::try_from(n)
        .ok()
        .filter(|&n| n <= 2 + usize::from(u16::MAX))
        .ok_or(EBADSTAT)?;
    let mut b = alloc::vec![0u8; n];
    uaccess::copy_in(&mut b, buf)?;
    match Dir::decode(&b) {
        Some((_, used)) if used == n => Ok(b),
        _ => Err(EBADSTAT.into()),
    }
}

fn wstat_chan(c: &Chan, b: &[u8]) -> Result<u64> {
    let (d, _) = Dir::decode(b).ok_or(EBADSTAT)?;
    c.dev.wstat(c, &d)?;
    Ok(0)
}

/// `wstat(name, buf, n)`
pub(crate) fn wstat(name: u64, buf: u64, n: u64) -> Result<u64> {
    let name = name_arg(name)?;
    let b = stat_in(buf, n)?;
    let ns = NameSpace::current()?;
    let w = chan::walk(&ns.names(), &name, true)?;
    wstat_chan(&w.chan, &b)
}

/// `fwstat(fd, buf, n)`
pub(crate) fn fwstat(fd: u64, buf: u64, n: u64) -> Result<u64> {
    let b = stat_in(buf, n)?;
    wstat_chan(&*chan_of(fd)?, &b)
}

/// `seek(fd, offset, whence) -> offset`: a directory seeks only to its
/// start.
pub(crate) fn seek(fd: u64, offset: u64, whence: u64) -> Result<u64> {
    let c = chan_of(fd)?;
    let whence = Whence::from_raw(whence).ok_or(EBADARG)?;
    #[allow(clippy::cast_possible_wrap)] // the offset is an i64
    let off = offset as i64;
    if c.qid.is_dir() {
        if whence != Whence::Start || off != 0 {
            return Err(EISDIR.into());
        }
        c.offset.store(0, Ordering::Relaxed);
        return Ok(0);
    }
    let base = match whence {
        Whence::Start => 0,
        Whence::Current => c.offset.load(Ordering::Relaxed),
        Whence::End => Dir::decode(&c.dev.stat(&c)?).ok_or(EBADSTAT)?.0.length,
    };
    let new = i64::try_from(base)
        .ok()
        .and_then(|b| b.checked_add(off))
        .and_then(|n| u64::try_from(n).ok())
        .ok_or("negative seek offset")?;
    c.offset.store(new, Ordering::Relaxed);
    Ok(new)
}

/// `remove(name)`
pub(crate) fn remove(name: u64) -> Result<u64> {
    let name = name_arg(name)?;
    let ns = NameSpace::current()?;
    let w = chan::walk(&ns.names(), &name, true)?;
    w.chan.dev.remove(&w.chan)?;
    Ok(0)
}

/// `fd2path(fd, buf, n)`: the name the descriptor's file was opened by,
/// with a NUL, cut to fit.
pub(crate) fn fd2path(fd: u64, buf: u64, n: u64) -> Result<u64> {
    let c = chan_of(fd)?;
    let n = usize::try_from(n).map_err(|_| EBADARG)?;
    if n == 0 {
        return Err(EBADARG.into());
    }
    let mut s = c.name.as_bytes().to_vec();
    s.truncate(n - 1);
    s.push(0);
    uaccess::copy_out(buf, &s)?;
    Ok(0)
}

/// `dup(old, new) -> fd`; `new` = `u64::MAX` picks the lowest free.
pub(crate) fn dup(old: u64, new: u64) -> Result<u64> {
    let new = (new != u64::MAX).then_some(new);
    Ok(pgrp::fgrp()?.dup(old, new)? as u64)
}

/// `pipe(fds)`: a new pipe, its two ends open for reading and writing on
/// two new descriptors, which go to `fds` as two words.
pub(crate) fn pipe(fds: u64) -> Result<u64> {
    uaccess::check(fds, 16)?;
    let rw = OpenMode {
        access: Access::ReadWrite,
        flags: OpenFlags::empty(),
    };
    let dir = PIPE.attach("")?;
    let a = PIPE.open(PIPE.walk(&dir, "data")?, rw)?;
    let b = PIPE.open(PIPE.walk(&dir, "data1")?, rw)?;
    let fgrp = pgrp::fgrp()?;
    let fa = fgrp.add(a, false)?;
    let fb = match fgrp.add(b, false) {
        Ok(fb) => fb,
        Err(e) => {
            drop(fgrp.close(fa as u64));
            return Err(e);
        }
    };
    let mut words = [0u8; 16];
    words[..8].copy_from_slice(&(fa as u64).to_ne_bytes());
    words[8..].copy_from_slice(&(fb as u64).to_ne_bytes());
    if let Err(e) = uaccess::copy_out(fds, &words) {
        drop((fgrp.close(fa as u64), fgrp.close(fb as u64)));
        return Err(e.into());
    }
    Ok(0)
}

/// `chdir(name)`
pub(crate) fn chdir(name: u64) -> Result<u64> {
    let name = name_arg(name)?;
    let dot = {
        let ns = NameSpace::current()?;
        chan::walk(&ns.names(), &name, false)?.chan
    };
    if !dot.qid.is_dir() {
        return Err(ENOTDIR.into());
    }
    let old = pgrp::with_files(|f| core::mem::replace(&mut f.dot, dot))?;
    drop(old);
    Ok(0)
}

/// `bind(name, old, flags)`
pub(crate) fn bind(name: u64, old: u64, flags: u64) -> Result<u64> {
    let (name, old) = (name_arg(name)?, name_arg(old)?);
    let flags = BindFlags::from_bits(flags).ok_or(EBADARG)?;
    let ns = NameSpace::current()?;
    let new = chan::walk(&ns.names(), &name, true)?;
    let old = chan::walk(&ns.names(), &old, false)?.chan;
    ns.pgrp().mount(new, &old, flags)?;
    Ok(0)
}

/// `unmount(name, old)`: a null `name` unmounts everything on `old`.
pub(crate) fn unmount(name: u64, old: u64) -> Result<u64> {
    let name = if name == 0 { None } else { Some(name_arg(name)?) };
    let old = name_arg(old)?;
    let ns = NameSpace::current()?;
    let new = name.map(|n| chan::walk(&ns.names(), &n, true)).transpose()?;
    let old = chan::walk(&ns.names(), &old, false)?.chan;
    ns.pgrp().unmount(new.as_ref().map(|w| &w.chan), &old)?;
    Ok(0)
}
