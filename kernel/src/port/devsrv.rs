//! `#s`, the service registry, as Plan 9's `devsrv`: one directory, the
//! same for every process, where a file stands for an open chan posted
//! under a name. Creating `#s/name` makes the entry; writing a descriptor
//! number to it posts that descriptor's chan; opening it afterwards gives
//! that chan, so a server can hand its end of a pipe to clients that do
//! not share its descriptors. Removing the file lets the chan go.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use abi::{Access, DMDIR, OpenFlags, OpenMode, QTDIR, QTFILE, Qid};

use crate::port::chan::{Chan, Walkqid};
use crate::port::dev::{self, Dev, Entry};
use crate::port::error::{EBADARG, EBADCHAR, EBADSPEC, EBADUSEFD, EEXIST, EISDIR, ENONEXIST, EPERM, ESHUTDOWN, Result};
use crate::port::lock::SpinLock;
use crate::port::pgrp;

/// A posted service.
#[derive(Debug)]
struct Srv {
    name: String,
    path: u64,
    perm: u32,
    /// What opening it gives, once a descriptor is written.
    chan: Option<Arc<Chan>>,
}

static SRVS: SpinLock<Vec<Srv>> = SpinLock::new(Vec::new());
static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

pub(crate) struct SrvDev;

pub(crate) static SRV: SrvDev = SrvDev;

const DIR: Qid = Qid {
    path: 0,
    vers: 0,
    ty: QTDIR,
};

const DIRPERM: u32 = DMDIR | 0o777;

fn entry(s: &Srv) -> Entry {
    let qid = Qid {
        path: s.path,
        vers: 0,
        ty: QTFILE,
    };
    Entry::new(&s.name, qid, 0, s.perm)
}

fn dirgen(_: &Chan, i: usize) -> Option<Entry> {
    SRVS.lock().get(i).map(entry)
}

/// The name `#s/name` that `c` is posted under, if it is: Plan 9's
/// `srvname`.
pub(crate) fn name_of(c: &Arc<Chan>) -> Option<String> {
    SRVS.lock()
        .iter()
        .find(|s| s.chan.as_ref().is_some_and(|p| Arc::ptr_eq(p, c)))
        .map(|s| alloc::format!("#s/{}", s.name))
}

impl Dev for SrvDev {
    fn dc(&self) -> char {
        's'
    }

    fn name(&self) -> &'static str {
        "srv"
    }

    fn attach(&self, spec: &str) -> Result<Chan> {
        if !spec.is_empty() {
            return Err(EBADSPEC.into());
        }
        Ok(Chan::new(&SRV, 0, DIR, String::from("#s")))
    }

    fn walk(&self, c: &Chan, names: &[&str]) -> Result<Walkqid> {
        dev::walk_gen(c, names, &dirgen, &dev::flat(DIR))
    }

    fn stat(&self, c: &Chan) -> Result<Vec<u8>> {
        if c.qid.is_dir() {
            return Ok(Entry::new("#s", DIR, 0, DIRPERM).encode(self, c.devno));
        }
        let srvs = SRVS.lock();
        let s = srvs.iter().find(|s| s.path == c.qid.path).ok_or(ENONEXIST)?;
        Ok(entry(s).encode(self, c.devno))
    }

    fn open(&self, c: Chan, mode: OpenMode) -> Result<Arc<Chan>> {
        if c.qid.is_dir() {
            if mode.flags.contains(OpenFlags::RCLOSE) {
                return Err(EPERM.into());
            }
            return dev::open_checked(c, mode, DIRPERM);
        }
        if mode.flags.contains(OpenFlags::TRUNC) {
            return Err("srv file already exists".into());
        }
        let srvs = SRVS.lock();
        let s = srvs.iter().find(|s| s.path == c.qid.path).ok_or(ESHUTDOWN)?;
        let posted = s.chan.as_ref().ok_or(ESHUTDOWN)?;
        let posted_access = posted.mode.map(|m| m.access);
        if Some(mode.access) != posted_access && posted_access != Some(Access::ReadWrite) {
            return Err(EPERM.into());
        }
        // The open gives the posted chan in place of `c`.
        dev::check_open(&c, mode, s.perm)?;
        Ok(posted.clone())
    }

    fn create(&self, dir: Chan, name: &str, mode: OpenMode, perm: u32) -> Result<Arc<Chan>> {
        if mode.access != Access::Write || perm & DMDIR != 0 {
            return Err(EPERM.into());
        }
        if name.contains('/') {
            return Err(EBADCHAR.into());
        }
        let mut srvs = SRVS.lock();
        if srvs.iter().any(|s| s.name == name) {
            return Err(EEXIST.into());
        }
        let path = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        let s = Srv {
            name: String::from(name),
            path,
            perm: perm & 0o777,
            chan: None,
        };
        let qid = entry(&s).qid;
        srvs.push(s);
        drop(srvs);
        Ok(dir.child(qid, name).opened(mode))
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        if !c.qid.is_dir() {
            return Err(EISDIR.into());
        }
        dev::dirread_gen(c, buf, offset, &dirgen)
    }

    /// Posts the descriptor whose number is written.
    fn write(&self, c: &Chan, buf: &[u8], _offset: u64) -> Result<usize> {
        if buf.len() >= 32 {
            return Err(EBADARG.into());
        }
        let fd = core::str::from_utf8(buf)
            .ok()
            .and_then(|t| parse_number(t.trim_end_matches(['\n', '\0'])))
            .ok_or(EBADARG)?;
        let (chan, cexec) = pgrp::fgrp()?.get_fd(fd)?;
        if cexec || chan.mode.is_some_and(|m| m.flags.contains(OpenFlags::RCLOSE)) {
            return Err("posted fd has remove-on-close or close-on-exec".into());
        }
        let mut srvs = SRVS.lock();
        let Some(s) = srvs.iter_mut().find(|s| s.path == c.qid.path) else {
            drop(srvs);
            drop(chan);
            return Err(ENONEXIST.into());
        };
        if s.chan.is_some() {
            drop(srvs);
            drop(chan);
            return Err(EBADUSEFD.into());
        }
        s.chan = Some(chan);
        Ok(buf.len())
    }

    fn remove(&self, c: &Chan) -> Result<()> {
        if c.qid.is_dir() {
            return Err(EPERM.into());
        }
        let gone = {
            let mut srvs = SRVS.lock();
            let i = srvs.iter().position(|s| s.path == c.qid.path).ok_or(ENONEXIST)?;
            if srvs[i].name == "boot" {
                return Err(EPERM.into());
            }
            srvs.remove(i)
        };
        drop(gone);
        Ok(())
    }
}

/// A number as C's `strtoul(s, 0, 0)` reads it: decimal, `0x` hex or
/// `0` octal.
fn parse_number(t: &str) -> Option<u64> {
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(h, 16).ok()
    } else if t.len() > 1 && t.starts_with('0') {
        u64::from_str_radix(&t[1..], 8).ok()
    } else {
        t.parse().ok()
    }
}
