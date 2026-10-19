//! `#e`, the environment: one directory of variables, each a file whose
//! contents are its value. Each process's environment is its [`Egrp`];
//! the device serves the caller's.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use abi::{DMDIR, OpenFlags, OpenMode, QTDIR, QTFILE, Qid};

use crate::port::chan::Chan;
use crate::port::dev::{self, Dev, Entry};
use crate::port::error::{EBADCHAR, EBADSPEC, EEXIST, ENONEXIST, EPERM, Error, Result};
use crate::port::lock::SpinLock;
use crate::port::pgrp;

/// The largest value.
const VALUE_MAX: u64 = 16 << 20;
/// The longest name.
const NAME_MAX: usize = 127;
/// The most variables.
const VARS_MAX: usize = 1024;

#[derive(Debug, Clone)]
struct Var {
    name: String,
    value: Vec<u8>,
    qid: Qid,
    perm: u32,
}

/// An environment.
#[derive(Debug)]
pub(crate) struct Egrp {
    vars: SpinLock<Vec<Var>>,
    /// The next variable's qid path; 0 is the directory.
    next: AtomicU64,
}

impl Egrp {
    pub(crate) fn new() -> Arc<Egrp> {
        Arc::new(Egrp {
            vars: SpinLock::new(Vec::new()),
            next: AtomicU64::new(1),
        })
    }

    /// A copy, for `rfork(RFENVG)`.
    pub(crate) fn copy(&self) -> Arc<Egrp> {
        Arc::new(Egrp {
            vars: SpinLock::new(self.vars.lock().clone()),
            next: AtomicU64::new(self.next.load(Ordering::Relaxed)),
        })
    }
}

pub(crate) struct Env;

pub(crate) static ENV: Env = Env;

const DIR: Qid = Qid {
    path: 0,
    vers: 0,
    ty: QTDIR,
};

fn entry(v: &Var) -> Entry {
    Entry::new(&v.name, v.qid, v.value.len() as u64, v.perm)
}

fn dirgen(_: &Chan, i: usize) -> Option<Entry> {
    pgrp::egrp().ok()?.vars.lock().get(i).map(entry)
}

/// Runs `f` on the variable `c` names.
fn with_var<T>(c: &Chan, f: impl FnOnce(&mut Var) -> Result<T>) -> Result<T> {
    let e = pgrp::egrp()?;
    let mut vars = e.vars.lock();
    let v = vars.iter_mut().find(|v| v.qid.path == c.qid.path).ok_or(ENONEXIST)?;
    f(v)
}

fn too_big() -> Error {
    "environment variable too large".into()
}

impl Dev for Env {
    fn dc(&self) -> char {
        'e'
    }

    fn name(&self) -> &'static str {
        "env"
    }

    fn attach(&self, spec: &str) -> Result<Chan> {
        if !spec.is_empty() {
            return Err(EBADSPEC.into());
        }
        Ok(Chan::new(&ENV, 0, DIR, String::from("#e")))
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Chan> {
        dev::walk_gen(c, name, &dirgen)
    }

    fn stat(&self, c: &Chan) -> Result<Vec<u8>> {
        if c.qid.is_dir() {
            return Ok(Entry::new("#e", DIR, 0, DMDIR | 0o775).encode(self, c.devno));
        }
        with_var(c, |v| Ok(entry(v).encode(self, c.devno)))
    }

    fn open(&self, c: Chan, mode: OpenMode) -> Result<Arc<Chan>> {
        if c.qid.is_dir() {
            return dev::open_checked(c, mode, DMDIR | 0o775);
        }
        let perm = with_var(&c, |v| Ok(v.perm))?;
        let c = dev::open_checked(c, mode, perm)?;
        if mode.flags.contains(OpenFlags::TRUNC) {
            with_var(&c, |v| {
                v.value.clear();
                v.qid.vers += 1;
                Ok(())
            })?;
        }
        Ok(c)
    }

    fn create(&self, dir: Chan, name: &str, mode: OpenMode, perm: u32) -> Result<Arc<Chan>> {
        if perm & DMDIR != 0 {
            return Err(EPERM.into());
        }
        if name.len() > NAME_MAX {
            return Err("file name too long".into());
        }
        if name.contains('/') {
            return Err(EBADCHAR.into());
        }
        let e = pgrp::egrp()?;
        let qid = {
            let mut vars = e.vars.lock();
            if vars.iter().any(|v| v.name == name) {
                return Err(EEXIST.into());
            }
            if vars.len() >= VARS_MAX {
                return Err("too many environment variables".into());
            }
            let qid = Qid {
                path: e.next.fetch_add(1, Ordering::Relaxed),
                vers: 0,
                ty: QTFILE,
            };
            vars.push(Var {
                name: String::from(name),
                value: Vec::new(),
                qid,
                perm: perm & 0o777,
            });
            qid
        };
        Ok(dir.child(qid, name).opened(mode))
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        if c.qid.is_dir() {
            return dev::dirread_gen(c, buf, offset, &dirgen);
        }
        with_var(c, |v| Ok(dev::read_bytes(&v.value, buf, offset)))
    }

    fn write(&self, c: &Chan, buf: &[u8], offset: u64) -> Result<usize> {
        if c.qid.is_dir() {
            return Err(EPERM.into());
        }
        let end = offset
            .checked_add(buf.len() as u64)
            .filter(|&e| e <= VALUE_MAX)
            .ok_or_else(too_big)?;
        with_var(c, |v| {
            #[allow(clippy::cast_possible_truncation)] // at most VALUE_MAX
            let (start, end) = (offset as usize, end as usize);
            if v.value.len() < end {
                v.value.resize(end, 0);
            }
            v.value[start..end].copy_from_slice(buf);
            v.qid.vers += 1;
            Ok(buf.len())
        })
    }

    fn remove(&self, c: &Chan) -> Result<()> {
        if c.qid.is_dir() {
            return Err(EPERM.into());
        }
        let e = pgrp::egrp()?;
        let gone = {
            let mut vars = e.vars.lock();
            let i = vars.iter().position(|v| v.qid.path == c.qid.path).ok_or(ENONEXIST)?;
            vars.remove(i)
        };
        drop(gone);
        Ok(())
    }
}
