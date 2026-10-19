//! Kernel devices: Plan 9's `Dev` table as a trait, the table of devices,
//! and the helpers simple devices share.
//!
//! A device serves a tree of files, reached by attaching to it (`#c`, or
//! `#c` with a specifier after the letter) and walking from there one name
//! at a time. Files are named by [`Chan`]s, and each operation gets the
//! chan it applies to. Most devices are a fixed table of files, or a
//! table they make as they go; [`Entry`] and the `*_gen` helpers serve
//! those from a function that lists a directory.
//!
//! **Permissions.** There are no users yet but the host owner, [`EVE`],
//! who owns every file, so a file's owner bits decide what an open may do.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::{Access, Dir, OpenFlags, OpenMode, Qid};

use crate::port::chan::Chan;
use crate::port::error::{EISDIR, ENONEXIST, EPERM, ESHORTSTAT, Result};
use crate::port::{devarch, devcons, devdup, devenv, devpipe, devproc, devroot, devrtc, devsrv};

/// The host owner, who owns everything until there are other users.
pub(crate) const EVE: &str = "eve";

/// A kernel device.
pub(crate) trait Dev: Sync {
    /// The letter that names it after `#`.
    fn dc(&self) -> char;
    /// Its name, for `#c/drivers`.
    fn name(&self) -> &'static str;
    /// The root of the device's tree, for `#` with `spec` after the
    /// letter.
    fn attach(&self, spec: &str) -> Result<Chan>;
    /// The file called `name` in directory `c`, unopened.
    fn walk(&self, c: &Chan, name: &str) -> Result<Chan>;
    /// The file's [`Dir`], encoded.
    fn stat(&self, c: &Chan) -> Result<Vec<u8>>;
    /// Opens the file; most devices check the mode and mark `c` open
    /// ([`open_checked`]).
    fn open(&self, c: Chan, mode: OpenMode) -> Result<Arc<Chan>>;
    /// Makes file `name` in directory `dir` and opens it.
    fn create(&self, _dir: Chan, _name: &str, _mode: OpenMode, _perm: u32) -> Result<Arc<Chan>> {
        Err(EPERM.into())
    }
    /// Reads at `offset`; a directory gives whole encoded entries.
    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize>;
    fn write(&self, _c: &Chan, _buf: &[u8], _offset: u64) -> Result<usize> {
        Err(EPERM.into())
    }
    /// The last reference to an open chan is gone.
    fn close(&self, _c: &Chan) {}
    fn remove(&self, _c: &Chan) -> Result<()> {
        Err(EPERM.into())
    }
    fn wstat(&self, _c: &Chan, _dir: &Dir<'_>) -> Result<()> {
        Err(EPERM.into())
    }
    /// The file's contents, if they stay in memory for good (the boot
    /// archive's), so `exec` can page from them without a copy.
    fn mapped(&self, _c: &Chan) -> Option<&'static [u8]> {
        None
    }
}

/// Every device, in the order `#c/drivers` lists them.
pub(crate) fn all() -> [&'static dyn Dev; 9] {
    [
        &devroot::ROOT,
        &devcons::CONS,
        &devenv::ENV,
        &devproc::PROC,
        &devpipe::PIPE,
        &devdup::DUP,
        &devsrv::SRV,
        &devrtc::RTC,
        &devarch::ARCH,
    ]
}

/// The device named by letter `dc`.
pub(crate) fn find(dc: char) -> Option<&'static dyn Dev> {
    all().into_iter().find(|d| d.dc() == dc)
}

/// A file in a device's table.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) name: String,
    pub(crate) qid: Qid,
    pub(crate) length: u64,
    /// Permissions, with `DMDIR` for a directory.
    pub(crate) perm: u32,
}

impl Entry {
    pub(crate) fn new(name: &str, qid: Qid, length: u64, perm: u32) -> Entry {
        Entry {
            name: String::from(name),
            qid,
            length,
            perm,
        }
    }

    /// The entry, encoded as `stat` gives it, for a file of `dev`.
    pub(crate) fn encode(&self, dev: &dyn Dev, devno: u32) -> Vec<u8> {
        #[allow(clippy::cast_possible_truncation)] // the 32-bit stat field
        let now = devrtc::seconds() as u32;
        let d = Dir {
            ty: u16::try_from(u32::from(dev.dc())).unwrap_or(0),
            dev: devno,
            qid: self.qid,
            mode: self.perm,
            atime: now,
            mtime: now,
            length: self.length,
            name: &self.name,
            uid: EVE,
            gid: EVE,
            muid: EVE,
        };
        let mut b = alloc::vec![0; d.size()];
        // A name too long to encode is cut to nothing rather than fail.
        if d.encode(&mut b).is_none() {
            let d = Dir { name: "", ..d };
            b.truncate(d.size());
            let _ = d.encode(&mut b);
        }
        b
    }
}

/// Lists a directory: entry `i` of directory `c`, or `None` past the end.
pub(crate) type Gen<'a> = &'a dyn Fn(&Chan, usize) -> Option<Entry>;

/// Walks to `name` in `c` by looking through the directory's entries.
pub(crate) fn walk_gen(c: &Chan, name: &str, list: Gen<'_>) -> Result<Chan> {
    (0..)
        .map_while(|i| list(c, i))
        .find(|e| e.name == name)
        .map(|e| c.child(e.qid, name))
        .ok_or_else(|| ENONEXIST.into())
}

/// Reads directory `c` at byte `offset`: as many whole entries as fit.
/// An entry too big for the buffer, when none has gone in yet, fails, so
/// a read is never empty before the end.
pub(crate) fn dirread_gen(c: &Chan, buf: &mut [u8], offset: u64, list: Gen<'_>) -> Result<usize> {
    let (mut at, mut n) = (0u64, 0usize);
    for e in (0..).map_while(|i| list(c, i)) {
        let d = e.encode(c.dev, c.devno);
        let start = at;
        at += d.len() as u64;
        if start < offset {
            continue;
        }
        if n + d.len() > buf.len() {
            if n == 0 {
                return Err(ESHORTSTAT.into());
            }
            break;
        }
        buf[n..n + d.len()].copy_from_slice(&d);
        n += d.len();
    }
    Ok(n)
}

/// The permission bits `mode` needs (read 4, write 2, execute 1).
fn needs(mode: OpenMode) -> u32 {
    let mut need = match mode.access {
        Access::Read => 4,
        Access::Write => 2,
        Access::ReadWrite => 6,
        Access::Exec => 1,
    };
    if mode.flags.contains(OpenFlags::TRUNC) {
        need |= 2;
    }
    need
}

/// Checks that the host owner may open a file with permissions `perm` in
/// `mode`, and that a directory is only read.
pub(crate) fn check_open(c: &Chan, mode: OpenMode, perm: u32) -> Result<()> {
    if c.qid.is_dir() && (mode.access != Access::Read || mode.flags.contains(OpenFlags::TRUNC)) {
        return Err(EISDIR.into());
    }
    let need = needs(mode);
    if (perm >> 6) & need != need {
        return Err(EPERM.into());
    }
    Ok(())
}

/// [`check_open`], then marks `c` open.
pub(crate) fn open_checked(c: Chan, mode: OpenMode, perm: u32) -> Result<Arc<Chan>> {
    check_open(&c, mode, perm)?;
    Ok(c.opened(mode))
}

/// Copies what lies at `offset` in `s` into `buf`.
pub(crate) fn read_bytes(s: &[u8], buf: &mut [u8], offset: u64) -> usize {
    let Ok(start) = usize::try_from(offset) else {
        return 0;
    };
    let Some(rest) = s.get(start..) else {
        return 0;
    };
    let n = rest.len().min(buf.len());
    buf[..n].copy_from_slice(&rest[..n]);
    n
}
