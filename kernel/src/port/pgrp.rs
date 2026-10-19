//! A process's groups: its name space's mount table ([`Pgrp`]), its file
//! descriptors ([`Fgrp`]), and, with its environment (`devenv::Egrp`),
//! the [`Files`] that hold them for the process.
//!
//! Each group can be shared by several processes (`rfork`, from slice 9),
//! so each locks itself. A lock is a spin lock held only to look or swap:
//! nothing that might sleep, closing a chan included, happens under one.

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::BindFlags;

use crate::port::chan::{Chan, Names, Walked};
use crate::port::devenv::Egrp;
use crate::port::devroot;
use crate::port::error::{EBADFD, EMOUNT, ENOFD, ENONS, EUNMOUNT, Result};
use crate::port::lock::SpinLock;
use crate::port::proc;

/// A directory in a union: what is mounted, and whether creates in the
/// union go to it.
#[derive(Debug, Clone)]
pub(crate) struct Mount {
    pub(crate) to: Arc<Chan>,
    pub(crate) create: bool,
}

/// A mount point and the union mounted on it.
#[derive(Debug)]
struct Mhead {
    from: Chan,
    members: Arc<[Mount]>,
}

/// A name space's mount table.
#[derive(Debug)]
pub(crate) struct Pgrp {
    mounts: SpinLock<Vec<Mhead>>,
}

impl Pgrp {
    /// An empty table: nothing but each device's own tree.
    pub(crate) fn new() -> Arc<Pgrp> {
        Arc::new(Pgrp {
            mounts: SpinLock::new(Vec::new()),
        })
    }

    /// What is mounted on `c`, if it is a mount point.
    pub(crate) fn lookup(&self, c: &Chan) -> Option<Arc<[Mount]>> {
        self.mounts
            .lock()
            .iter()
            .find(|m| m.from.same_file(c))
            .map(|m| m.members.clone())
    }

    /// Mounts `new` on `old` as `flags` say: in place of what is there, or
    /// before or after it in a union. `new` is what walking its name gave,
    /// so a union there joins whole; `old` is the mount point itself.
    pub(crate) fn mount(&self, new: Walked, old: &Chan, flags: BindFlags) -> Result<()> {
        if new.chan.qid.is_dir() != old.qid.is_dir() {
            return Err(EMOUNT.into());
        }
        let (before, after) = (flags.contains(BindFlags::BEFORE), flags.contains(BindFlags::AFTER));
        let union = before || after;
        if before && after || union && !old.qid.is_dir() {
            return Err(EMOUNT.into());
        }
        let create = flags.contains(BindFlags::CREATE);
        let added: Vec<Mount> = match new.union {
            Some(members) if union => members
                .iter()
                .map(|m| Mount {
                    to: m.to.clone(),
                    create,
                })
                .collect(),
            _ => alloc::vec![Mount {
                to: Arc::new(new.chan.unopened()),
                create,
            }],
        };
        let itself = Mount {
            to: Arc::new(old.unopened()),
            create: false,
        };
        let from = old.unopened();
        let mut mounts = self.mounts.lock();
        let at = mounts.iter().position(|m| m.from.same_file(old));
        let old_members = match at {
            Some(i) => mounts[i].members.clone(),
            // A new union starts with the mount point itself in it.
            None if union => Arc::from([itself]),
            None => Arc::from([]),
        };
        let members: Arc<[Mount]> = if before {
            added.iter().chain(old_members.iter()).cloned().collect()
        } else if after {
            old_members.iter().chain(added.iter()).cloned().collect()
        } else {
            added.into()
        };
        let old_head = if let Some(i) = at {
            Some(core::mem::replace(&mut mounts[i].members, members))
        } else {
            mounts.push(Mhead { from, members });
            None
        };
        drop(mounts);
        drop((old_head, old_members));
        Ok(())
    }

    /// Unmounts `new` from `old`, or everything mounted there if `new` is
    /// `None`.
    pub(crate) fn unmount(&self, new: Option<&Chan>, old: &Chan) -> Result<()> {
        let mut mounts = self.mounts.lock();
        let i = mounts.iter().position(|m| m.from.same_file(old)).ok_or(EUNMOUNT)?;
        let gone = match new {
            None => mounts.remove(i).members,
            Some(new) => {
                let members = &mounts[i].members;
                if !members.iter().any(|m| m.to.same_file(new)) {
                    return Err(EUNMOUNT.into());
                }
                let left: Arc<[Mount]> = members.iter().filter(|m| !m.to.same_file(new)).cloned().collect();
                if left.is_empty() {
                    mounts.remove(i).members
                } else {
                    core::mem::replace(&mut mounts[i].members, left)
                }
            }
        };
        drop(mounts);
        drop(gone);
        Ok(())
    }
}

/// The most file descriptors a process may have open.
pub(crate) const NFD: usize = 1024;

#[derive(Debug, Clone)]
struct Fd {
    chan: Arc<Chan>,
    /// Closed by `exec`.
    cexec: bool,
}

/// File descriptors: small numbers for open chans.
#[derive(Debug)]
pub(crate) struct Fgrp {
    fds: SpinLock<Vec<Option<Fd>>>,
}

impl Fgrp {
    /// No descriptors open.
    pub(crate) fn new() -> Arc<Fgrp> {
        Arc::new(Fgrp {
            fds: SpinLock::new(Vec::new()),
        })
    }

    /// The lowest free descriptor, now holding `chan`.
    pub(crate) fn add(&self, chan: Arc<Chan>, cexec: bool) -> Result<usize> {
        let mut fds = self.fds.lock();
        let fd = fds.iter().position(Option::is_none).unwrap_or(fds.len());
        if fd >= NFD {
            drop(fds);
            drop(chan);
            return Err(ENOFD.into());
        }
        if fd == fds.len() {
            fds.push(None);
        }
        fds[fd] = Some(Fd { chan, cexec });
        Ok(fd)
    }

    /// The chan open on `fd`.
    pub(crate) fn get(&self, fd: u64) -> Result<Arc<Chan>> {
        let fds = self.fds.lock();
        usize::try_from(fd)
            .ok()
            .and_then(|i| fds.get(i))
            .and_then(Option::as_ref)
            .map(|f| f.chan.clone())
            .ok_or_else(|| EBADFD.into())
    }

    /// Closes `fd`, returning its chan for the caller to drop, outside the
    /// lock.
    pub(crate) fn close(&self, fd: u64) -> Result<Arc<Chan>> {
        let mut fds = self.fds.lock();
        usize::try_from(fd)
            .ok()
            .and_then(|i| fds.get_mut(i))
            .and_then(Option::take)
            .map(|f| f.chan)
            .ok_or_else(|| EBADFD.into())
    }

    /// Makes `new` (the lowest free descriptor if `None`) another
    /// descriptor for `old`'s chan, closing what `new` held.
    pub(crate) fn dup(&self, old: u64, new: Option<u64>) -> Result<usize> {
        let chan = self.get(old)?;
        let Some(new) = new else {
            return self.add(chan, false);
        };
        let new = usize::try_from(new).ok().filter(|&n| n < NFD).ok_or(EBADFD)?;
        let mut fds = self.fds.lock();
        if new >= fds.len() {
            fds.resize(new + 1, None);
        }
        let gone = fds[new].replace(Fd { chan, cexec: false });
        drop(fds);
        drop(gone);
        Ok(new)
    }

    /// Closes every descriptor marked close-on-exec; returns their chans.
    pub(crate) fn close_on_exec(&self) -> Vec<Arc<Chan>> {
        let mut fds = self.fds.lock();
        fds.iter_mut()
            .filter(|f| f.as_ref().is_some_and(|f| f.cexec))
            .filter_map(|f| f.take().map(|f| f.chan))
            .collect()
    }

    /// The open descriptors and their chans.
    pub(crate) fn list(&self) -> Vec<(usize, Arc<Chan>)> {
        self.fds
            .lock()
            .iter()
            .enumerate()
            .filter_map(|(i, f)| Some((i, f.as_ref()?.chan.clone())))
            .collect()
    }
}

/// What a user process's names and files are: its groups, root and
/// current directory.
#[derive(Debug)]
pub(crate) struct Files {
    pub(crate) pgrp: Arc<Pgrp>,
    pub(crate) fgrp: Arc<Fgrp>,
    pub(crate) egrp: Arc<Egrp>,
    /// `/`, the root device's root, and the current directory, both
    /// unopened and as walked, before the mount table is followed.
    pub(crate) slash: Chan,
    pub(crate) dot: Chan,
}

impl Files {
    /// A fresh start, as the first process has: nothing mounted, `#/` at
    /// `/`, there as the current directory too, no descriptors and an
    /// empty environment.
    pub(crate) fn boot() -> Result<Files> {
        let slash = devroot::attach_root()?.renamed("/");
        Ok(Files {
            pgrp: Pgrp::new(),
            fgrp: Fgrp::new(),
            egrp: Egrp::new(),
            dot: slash.renamed("/"),
            slash,
        })
    }
}

/// The parts of the calling process's [`Files`] a name needs, held while
/// it is resolved.
pub(crate) struct NameSpace {
    pgrp: Arc<Pgrp>,
    slash: Chan,
    dot: Chan,
}

impl NameSpace {
    /// The calling process's.
    pub(crate) fn current() -> Result<NameSpace> {
        with_files(|f| NameSpace {
            pgrp: f.pgrp.clone(),
            slash: f.slash.unopened(),
            dot: f.dot.unopened(),
        })
    }

    /// `files`'.
    pub(crate) fn of(f: &Files) -> NameSpace {
        NameSpace {
            pgrp: f.pgrp.clone(),
            slash: f.slash.unopened(),
            dot: f.dot.unopened(),
        }
    }

    pub(crate) fn names(&self) -> Names<'_> {
        Names {
            pgrp: &self.pgrp,
            slash: &self.slash,
            dot: &self.dot,
        }
    }

    pub(crate) fn pgrp(&self) -> &Pgrp {
        &self.pgrp
    }
}

/// Runs `f` on the calling process's files, under their lock: `f` must
/// not sleep, nor drop a chan that was open.
pub(crate) fn with_files<T>(f: impl FnOnce(&mut Files) -> T) -> Result<T> {
    let me = proc::current().ok_or(ENONS)?;
    let mut files = me.files().lock();
    Ok(f(files.as_mut().ok_or(ENONS)?))
}

/// The calling process's file descriptors.
pub(crate) fn fgrp() -> Result<Arc<Fgrp>> {
    with_files(|f| f.fgrp.clone())
}

/// The calling process's environment.
pub(crate) fn egrp() -> Result<Arc<Egrp>> {
    with_files(|f| f.egrp.clone())
}
