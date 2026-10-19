//! The system calls on files and name spaces (Plan 9's `sysfile.c`):
//! `open`, `create`, `close`, `read`, `write`, `pread`, `pwrite`, `seek`,
//! `stat`, `fstat`, `wstat`, `fwstat`, `remove`, `fd2path`, `dup`,
//! `pipe`, `chdir`, `bind`, `mount`, `unmount`, `fversion` and `fauth`.
//!
//! Data passes through a kernel buffer: a read gives at most [`IO_MAX`]
//! bytes, and a write goes to the device in pieces that size.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use abi::{Access, BindFlags, Dir, OpenFlags, OpenMode, Whence};

use crate::port::chan::{self, Amode, Chan};
use crate::port::dev::Dev;
use crate::port::devmnt;
use crate::port::devpipe::PIPE;
use crate::port::error::{
    EBADARG, EBADCHAR, EBADSTAT, EBADUSEFD, EGREG, EISDIR, EISMTPT, EISSTREAM, ENOATTACH, ESHORTSTAT, Error, Result,
};
use crate::port::pgrp::{self, NameSpace};
use crate::port::uaccess;

/// The most bytes a read returns, and the size of a write's pieces.
pub(crate) const IO_MAX: usize = 128 * 1024;
/// A file name a call takes is shorter than this, as in Plan 9, so it
/// fits a 9P string.
pub(crate) const NAME_MAX: usize = 1 << 16;

/// A name argument: NUL-terminated UTF-8 (Plan 9 takes any bytes; Plan
/// R's names are text).
pub(crate) fn name_arg(p: u64) -> Result<String> {
    let b = uaccess::copy_in_str(p, NAME_MAX)?;
    if b.len() >= NAME_MAX {
        return Err("name too long".into());
    }
    String::from_utf8(b).map_err(|e| {
        let mut s = alloc::format!(
            "{EBADCHAR}: {}",
            chan::quote(&String::from_utf8_lossy(e.as_bytes()), false)
        );
        chan::truncate(&mut s, 127);
        s.into()
    })
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

/// Plan 9's `pathlast`: the last element of a name, empty for `/`.
fn pathlast(name: &str) -> &str {
    name.rfind('/').map_or(name, |i| &name[i + 1..])
}

/// The chan's stat, named by the last element of its name, as Plan 9's
/// `sysstat` has `dirsetname` do.
fn stat_of(c: &Chan) -> Result<Vec<u8>> {
    let b = c.dev.stat(c)?;
    let (d, _) = Dir::decode(&b).ok_or(EBADSTAT)?;
    let d = Dir {
        name: pathlast(c.name()),
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
        let c = chan::namec(&ns.names(), &name, Amode::Access)?;
        stat_of(&c)?
    };
    stat_out(&s, buf, n)
}

/// `fstat(fd, buf, n) -> n`: the device's own stat, name and all, as
/// Plan 9's.
pub(crate) fn fstat(fd: u64, buf: u64, n: u64) -> Result<u64> {
    let c = chan_of(fd)?;
    let s = c.dev.stat(&c)?;
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

/// Renaming a mount point is refused, as in Plan 9: which would it
/// rename?
fn wstat_chan(c: &Chan, b: &[u8]) -> Result<u64> {
    let (d, _) = Dir::decode(b).ok_or(EBADSTAT)?;
    if c.ismtpt && !d.name.is_empty() {
        let mut e = alloc::format!("{} {EISMTPT}", chan::quote(c.name(), true));
        chan::truncate(&mut e, 127);
        return Err(e.into());
    }
    c.dev.wstat(c, &d)?;
    Ok(0)
}

/// `wstat(name, buf, n)`
pub(crate) fn wstat(name: u64, buf: u64, n: u64) -> Result<u64> {
    let name = name_arg(name)?;
    let b = stat_in(buf, n)?;
    let ns = NameSpace::current()?;
    let c = chan::namec(&ns.names(), &name, Amode::Access)?;
    wstat_chan(&c, &b)
}

/// `fwstat(fd, buf, n)`
pub(crate) fn fwstat(fd: u64, buf: u64, n: u64) -> Result<u64> {
    let b = stat_in(buf, n)?;
    wstat_chan(&*chan_of(fd)?, &b)
}

/// `seek(fd, offset, whence) -> offset`: a directory seeks only to its
/// start, and a pipe not at all.
pub(crate) fn seek(fd: u64, offset: u64, whence: u64) -> Result<u64> {
    let c = chan_of(fd)?;
    if c.dev.dc() == '|' {
        return Err(EISSTREAM.into());
    }
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

/// `remove(name)`: not of a mount point, as in Plan 9 (which would it
/// remove?).
pub(crate) fn remove(name: u64) -> Result<u64> {
    let name = name_arg(name)?;
    let ns = NameSpace::current()?;
    let c = chan::namec(&ns.names(), &name, Amode::Remove)?;
    if c.ismtpt {
        return Err(EISMTPT.into());
    }
    c.dev.remove(&c)?;
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
    let mut s = c.name().as_bytes().to_vec();
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
    let end = |name| PIPE.walk(&dir, &[name])?.clone.ok_or_else(|| Error::from(EGREG));
    let a = PIPE.open(end("data")?, rw)?;
    let b = PIPE.open(end("data1")?, rw)?;
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
        chan::namec(&ns.names(), &name, Amode::Todir)?
    };
    let old = pgrp::with_files(|f| core::mem::replace(&mut f.dot, dot))?;
    drop(old);
    Ok(0)
}

/// The flags `bind` and `mount` take, as Plan 9 checks them.
fn bind_flags(flags: u64) -> Result<BindFlags> {
    let f = BindFlags::from_bits(flags).ok_or(EBADARG)?;
    if f.contains(BindFlags::BEFORE | BindFlags::AFTER) {
        return Err(EBADARG.into());
    }
    Ok(f)
}

/// `bind(name, old, flags) -> id`: the id is the mount's sequence number.
pub(crate) fn bind(name: u64, old: u64, flags: u64) -> Result<u64> {
    let flags = bind_flags(flags)?;
    let (name, old) = (name_arg(name)?, name_arg(old)?);
    let ns = NameSpace::current()?;
    let new = chan::namec(&ns.names(), &name, Amode::Bind)?;
    let old = chan::namec(&ns.names(), &old, Amode::Mount)?;
    ns.pgrp().mount(&new, &old, flags, None)
}

/// `unmount(name, old)`: a null `name` unmounts everything on `old`.
/// `name` is opened, as Plan 9 does, so that a file such as `/srv/x`
/// gives the chan behind it, which a mount is matched by too.
pub(crate) fn unmount(name: u64, old: u64) -> Result<u64> {
    let name = if name == 0 { None } else { Some(name_arg(name)?) };
    let old = name_arg(old)?;
    let ns = NameSpace::current()?;
    let old = chan::namec(&ns.names(), &old, Amode::Mount)?;
    let new = name.map(|n| chan::open(&ns.names(), &n, mode_read())).transpose()?;
    ns.pgrp().unmount(new.as_deref(), &old)?;
    drop(new);
    Ok(0)
}

fn mode_read() -> OpenMode {
    OpenMode {
        access: Access::Read,
        flags: OpenFlags::empty(),
    }
}

/// `mount(fd, afd, old, flags, aname) -> id`: attaches to the 9P server on `fd`
/// and mounts its tree `aname` (null for the default) on `old`, as `bind`
/// would; `afd` is a file from `fauth`, or -1.
pub(crate) fn mount(fd: u64, afd: u64, old: u64, flags: u64, aname: u64) -> Result<u64> {
    let flags = bind_flags(flags)?;
    let aname = if aname == 0 { String::new() } else { name_arg(aname)? };
    chan::validname(&aname, true)?;
    let ns = NameSpace::current()?;
    if ns.pgrp().noattach() {
        return Err(ENOATTACH.into());
    }
    let c = chan_of(fd)?;
    let afile = if afd == u64::MAX { None } else { Some(chan_of(afd)?) };
    let root = devmnt::attach(&c, afile.as_deref(), &aname)?;
    drop(afile);
    let old = name_arg(old)?;
    let old = chan::namec(&ns.names(), &old, Amode::Mount)?;
    let id = ns.pgrp().mount(&root, &old, flags, Some(&aname))?;
    // A mount closes the descriptor it was given, as Plan 9's does.
    drop(c);
    if let Ok(c) = pgrp::fgrp()?.close(fd) {
        drop(c);
    }
    Ok(id)
}

/// `fversion(fd, msize, buf, n) -> n`: sets up the 9P session on `fd`,
/// with messages of at most `msize` bytes (0 for the default) and the
/// version in the `n` bytes at `buf` (empty for 9P2000), unless it has
/// one; the session's version goes back to `buf`, if `n` is not 0.
pub(crate) fn fversion(fd: u64, msize: u64, buf: u64, n: u64) -> Result<u64> {
    let n = usize::try_from(n).map_err(|_| EBADARG)?.min(NAME_MAX);
    let mut v = alloc::vec![0u8; n];
    uaccess::copy_in(&mut v, buf)?;
    if let Some(end) = v.iter().position(|&b| b == 0) {
        v.truncate(end);
    }
    let v = String::from_utf8(v).map_err(|_| EBADARG)?;
    let msize = u32::try_from(msize).map_err(|_| EBADARG)?;
    let got = devmnt::fversion(&chan_of(fd)?, msize, &v)?;
    if n > 0 {
        if got.len() > n {
            return Err("i/o count too small".into());
        }
        uaccess::copy_out(buf, got.as_bytes())?;
    }
    Ok(got.len() as u64)
}

/// `fauth(fd, aname) -> fd`: a file for authenticating to the 9P server
/// on `fd`, for `mount` to pass on.
pub(crate) fn fauth(fd: u64, aname: u64) -> Result<u64> {
    let aname = if aname == 0 { String::new() } else { name_arg(aname)? };
    if NameSpace::current()?.pgrp().noattach() {
        return Err(ENOATTACH.into());
    }
    let a = devmnt::auth(&chan_of(fd)?, &aname)?;
    Ok(pgrp::fgrp()?.add(a, false)? as u64)
}
