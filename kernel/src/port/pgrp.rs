//! A process's groups: its name space's mount table ([`Pgrp`]), its file
//! descriptors ([`Fgrp`]), and, with its environment (`devenv::Egrp`) and
//! rendezvous group (`proc::Rgrp`), the [`Files`] that hold them for the
//! process.
//!
//! Each group can be shared by several processes, as `rfork` leaves them
//! ([`Files::fork`]), so each locks itself. A lock is a spin lock held only to look or swap:
//! nothing that might sleep, closing a chan included, happens under one.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use abi::{BindFlags, RforkFlags};

use crate::port::chan::{Chan, Names, Path};
use crate::port::dev::Dev;
use crate::port::devenv::Egrp;
use crate::port::devmnt;
use crate::port::devroot;
use crate::port::error::{EBADFD, EMOUNT, ENOFD, ENONS, EUNION, EUNMOUNT, Result};
use crate::port::lock::SpinLock;
use crate::port::proc::{self, Rgrp};

/// A directory in a union: what is mounted, how, and when.
#[derive(Debug, Clone)]
pub(crate) struct Mount {
    pub(crate) to: Arc<Chan>,
    /// Plan 9's `mountid`: one more for each mount made anywhere, so the
    /// table can be listed in the order it was built.
    pub(crate) id: u64,
    /// The flags it was made with (Plan 9's `mflag`): none for the mount
    /// point itself in a union.
    pub(crate) flags: BindFlags,
    /// The tree asked for, for `mount`.
    pub(crate) spec: Option<Arc<str>>,
}

impl Mount {
    fn new(to: Arc<Chan>, flags: BindFlags, spec: Option<Arc<str>>) -> Mount {
        Mount {
            to,
            id: MOUNT_ID.fetch_add(1, Ordering::Relaxed),
            flags,
            spec,
        }
    }

    /// Creates in the union go to it.
    pub(crate) fn create(&self) -> bool {
        self.flags.contains(BindFlags::CREATE)
    }
}

static MOUNT_ID: AtomicU64 = AtomicU64::new(1);

/// An entry of the table, as `#p/n/ns` lists them: a mount and its mount
/// point's name.
#[derive(Debug)]
pub(crate) struct Entry {
    pub(crate) from: String,
    pub(crate) mount: Mount,
}

/// A mount point and the union mounted on it.
#[derive(Debug)]
struct Mhead {
    from: Arc<Chan>,
    members: Arc<[Mount]>,
}

/// A name space's mount table.
#[derive(Debug)]
pub(crate) struct Pgrp {
    mounts: SpinLock<Vec<Mhead>>,
    /// `RFNOMNT`: no attaching devices by `#` name but a few, and no
    /// mounts.
    noattach: AtomicBool,
}

impl Pgrp {
    /// An empty table: nothing but each device's own tree.
    pub(crate) fn new() -> Arc<Pgrp> {
        Arc::new(Pgrp {
            mounts: SpinLock::new(Vec::new()),
            noattach: AtomicBool::new(false),
        })
    }

    /// A copy of the table, for `rfork(RFNAMEG)`.
    fn copy(&self) -> Arc<Pgrp> {
        let mounts = self
            .mounts
            .lock()
            .iter()
            .map(|m| Mhead {
                from: m.from.clone(),
                members: m.members.clone(),
            })
            .collect();
        Arc::new(Pgrp {
            mounts: SpinLock::new(mounts),
            noattach: AtomicBool::new(self.noattach()),
        })
    }

    /// True once `RFNOMNT` has been asked for.
    pub(crate) fn noattach(&self) -> bool {
        self.noattach.load(Ordering::Relaxed)
    }

    /// Plan 9's `findmount`: if file `qid` of instance `devno` of `dev`
    /// is a mount point, the mount point and what is mounted there.
    pub(crate) fn findmount(&self, dev: &dyn Dev, devno: u32, qid: abi::Qid) -> Option<(Arc<Chan>, Arc<[Mount]>)> {
        self.mounts
            .lock()
            .iter()
            .find(|m| m.from.is(dev, devno, qid))
            .map(|m| (m.from.clone(), m.members.clone()))
    }

    /// Mounts `new` on `old` as `flags` say, as Plan 9's `cmount`: in
    /// place of what is there, or before or after it in a union. `new` is
    /// what walking its name gave, so a union there (`new.union`) joins
    /// whole; `old` is the mount point itself. Returns the mount's id.
    pub(crate) fn mount(&self, new: &Chan, old: &Chan, flags: BindFlags, spec: Option<&str>) -> Result<u64> {
        if new.qid.is_dir() != old.qid.is_dir() {
            return Err(EMOUNT.into());
        }
        let (before, after) = (flags.contains(BindFlags::BEFORE), flags.contains(BindFlags::AFTER));
        let union = before || after;
        if !old.qid.is_dir() && union {
            return Err(EMOUNT.into());
        }
        // No creating in a union bound whole, unless it is a lone member
        // that takes creates.
        if flags.contains(BindFlags::CREATE)
            && new
                .union
                .as_ref()
                .is_some_and(|u| !u.is_empty() && (u.len() > 1 || !u[0].create()))
        {
            return Err(EMOUNT.into());
        }
        let mut mounts = self.mounts.lock();
        let at = mounts.iter().position(|m| m.from.same_file(old));
        let old_members = match at {
            Some(i) => mounts[i].members.clone(),
            // A new union starts with the mount point itself in it.
            None if union => Arc::from([Mount::new(Arc::new(old.unopened()), BindFlags::empty(), None)]),
            None => Arc::from([]),
        };
        let first = Mount::new(Arc::new(new.unopened()), flags, spec.map(Arc::from));
        let id = first.id;
        // The rest of a union bound whole come after its first, in order.
        let order = if before { BindFlags::BEFORE } else { BindFlags::AFTER };
        let mut added = alloc::vec![first];
        if let Some(u) = &new.union {
            added.extend(
                u.iter()
                    .skip(1)
                    .map(|m| Mount::new(m.to.clone(), order, m.spec.clone())),
            );
        }
        let members: Arc<[Mount]> = if after {
            old_members.iter().cloned().chain(added).collect()
        } else if before {
            added.into_iter().chain(old_members.iter().cloned()).collect()
        } else {
            added.into()
        };
        let old_head = if let Some(i) = at {
            Some(core::mem::replace(&mut mounts[i].members, members))
        } else {
            mounts.push(Mhead {
                from: Arc::new(old.unopened()),
                members,
            });
            None
        };
        drop(mounts);
        drop((old_head, old_members));
        Ok(id)
    }

    /// The entry after the one with id `last` (from the start if 0), in
    /// the order the mounts were made: what `#p/n/ns` gives a read.
    pub(crate) fn next_entry(&self, last: u64) -> Option<Entry> {
        self.mounts
            .lock()
            .iter()
            .flat_map(|h| h.members.iter().map(move |m| (h, m)))
            .filter(|(_, m)| m.id > last)
            .min_by_key(|(_, m)| m.id)
            .map(|(h, m)| Entry {
                from: String::from(h.from.name()),
                mount: m.clone(),
            })
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
                // The first that is `new`, or is mounted from it.
                let at = members
                    .iter()
                    .position(|m| m.to.same_file(new) || devmnt::transport(&m.to).is_some_and(|t| t.same_file(new)))
                    .ok_or(EUNION)?;
                let left: Arc<[Mount]> = members
                    .iter()
                    .enumerate()
                    .filter(|&(j, _)| j != at)
                    .map(|(_, m)| m.clone())
                    .collect();
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

    /// The chan open on `fd`, and whether it is closed on exec.
    pub(crate) fn get_fd(&self, fd: u64) -> Result<(Arc<Chan>, bool)> {
        let fds = self.fds.lock();
        usize::try_from(fd)
            .ok()
            .and_then(|i| fds.get(i))
            .and_then(Option::as_ref)
            .map(|f| (f.chan.clone(), f.cexec))
            .ok_or_else(|| EBADFD.into())
    }

    /// A copy of the table, its descriptors holding the same chans, for
    /// `rfork(RFFDG)`.
    fn copy(&self) -> Arc<Fgrp> {
        Arc::new(Fgrp {
            fds: SpinLock::new(self.fds.lock().clone()),
        })
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
    pub(crate) rgrp: Arc<Rgrp>,
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
        let mut slash = devroot::attach_root()?;
        slash.path = Path::new(String::from("/"));
        Ok(Files {
            pgrp: Pgrp::new(),
            fgrp: Fgrp::new(),
            egrp: Egrp::new(),
            rgrp: Arc::new(Rgrp::new()),
            dot: slash.unopened(),
            slash,
        })
    }

    /// The files of a child of `rfork`, or the caller's own after an
    /// `rfork` without `RFPROC`: each group shared, copied (`RFNAMEG`,
    /// `RFFDG`, `RFENVG`) or fresh (`RFCNAMEG`, `RFCFDG`, `RFCENVG`,
    /// `RFREND`), as `flags` say. A fresh name space keeps `/` and the
    /// current directory, and whether attaches are allowed. Never drops a
    /// chan, so it may run under the files' lock.
    pub(crate) fn fork(&self, flags: RforkFlags) -> Files {
        let pgrp = if flags.contains(RforkFlags::CNAMEG) {
            let p = Pgrp::new();
            p.noattach.store(self.pgrp.noattach(), Ordering::Relaxed);
            p
        } else if flags.contains(RforkFlags::NAMEG) {
            self.pgrp.copy()
        } else {
            self.pgrp.clone()
        };
        if flags.contains(RforkFlags::NOMNT) {
            pgrp.noattach.store(true, Ordering::Relaxed);
        }
        let fgrp = if flags.contains(RforkFlags::CFDG) {
            Fgrp::new()
        } else if flags.contains(RforkFlags::FDG) {
            self.fgrp.copy()
        } else {
            self.fgrp.clone()
        };
        let egrp = if flags.contains(RforkFlags::CENVG) {
            Egrp::new()
        } else if flags.contains(RforkFlags::ENVG) {
            self.egrp.copy()
        } else {
            self.egrp.clone()
        };
        let rgrp = if flags.contains(RforkFlags::REND) {
            Arc::new(Rgrp::new())
        } else {
            self.rgrp.clone()
        };
        Files {
            pgrp,
            fgrp,
            egrp,
            rgrp,
            slash: self.slash.unopened(),
            dot: self.dot.unopened(),
        }
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

/// The calling process's rendezvous group.
pub(crate) fn rgrp() -> Result<Arc<Rgrp>> {
    with_files(|f| f.rgrp.clone())
}
