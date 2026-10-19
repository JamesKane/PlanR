//! Directory entries that own their strings ([`DirBuf`], Plan 9's `Dir`
//! as `dirstat` allocates it), and the calls that give and take them:
//! `dirstat`, `dirfstat`, `dirwstat`, `dirfwstat`, `dirread`,
//! `dirreadall`.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::CStr;

use abi::{Dir, Qid};

use crate::{Error, Fd};

/// A directory entry; see [`abi::Dir`] for the fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirBuf {
    pub ty: u16,
    pub dev: u32,
    pub qid: Qid,
    pub mode: u32,
    pub atime: u32,
    pub mtime: u32,
    pub length: u64,
    pub name: String,
    pub uid: String,
    pub gid: String,
    pub muid: String,
}

impl DirBuf {
    #[must_use]
    pub fn from_dir(d: &Dir<'_>) -> DirBuf {
        DirBuf {
            ty: d.ty,
            dev: d.dev,
            qid: d.qid,
            mode: d.mode,
            atime: d.atime,
            mtime: d.mtime,
            length: d.length,
            name: String::from(d.name),
            uid: String::from(d.uid),
            gid: String::from(d.gid),
            muid: String::from(d.muid),
        }
    }

    /// This entry, borrowing its strings.
    #[must_use]
    pub fn as_dir(&self) -> Dir<'_> {
        Dir {
            ty: self.ty,
            dev: self.dev,
            qid: self.qid,
            mode: self.mode,
            atime: self.atime,
            mtime: self.mtime,
            length: self.length,
            name: &self.name,
            uid: &self.uid,
            gid: &self.gid,
            muid: &self.muid,
        }
    }

    /// A `wstat` that changes nothing: every field "don't touch" (Plan
    /// 9's `nulldir`). Set the fields to change.
    #[must_use]
    pub fn null() -> DirBuf {
        DirBuf::from_dir(&Dir::DONT_TOUCH)
    }

    /// Whether this is a directory.
    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.qid.is_dir()
    }

    fn encode(&self) -> Vec<u8> {
        let d = self.as_dir();
        let mut b = vec![0; d.size()];
        let _ = d.encode(&mut b);
        b
    }
}

/// Decodes the one entry a stat call put in `buf`.
fn decode_one(buf: &[u8]) -> Result<DirBuf, Error> {
    Dir::decode(buf)
        .map(|(d, _)| DirBuf::from_dir(&d))
        .ok_or_else(|| Error::from_text(b"malformed stat buffer"))
}

/// Runs a stat call into a buffer that grows to fit.
fn stat_with(f: impl Fn(&mut [u8]) -> Result<usize, Error>) -> Result<DirBuf, Error> {
    let mut buf = vec![0u8; 256];
    loop {
        let n = f(&mut buf)?;
        if n > 2 {
            return decode_one(&buf[..n]);
        }
        // Only the size fit: it says how much is needed.
        let need = usize::from(u16::from_le_bytes([buf[0], buf[1]])) + 2;
        if need <= buf.len() {
            return Err(Error::from_text(b"malformed stat buffer"));
        }
        buf.resize(need, 0);
    }
}

pub fn dirstat(name: &CStr) -> Result<DirBuf, Error> {
    stat_with(|b| crate::stat(name, b))
}

pub fn dirfstat(fd: Fd) -> Result<DirBuf, Error> {
    stat_with(|b| crate::fstat(fd, b))
}

pub fn dirwstat(name: &CStr, d: &DirBuf) -> Result<(), Error> {
    crate::wstat(name, &d.encode())
}

pub fn dirfwstat(fd: Fd, d: &DirBuf) -> Result<(), Error> {
    crate::fwstat(fd, &d.encode())
}

/// The most a directory read asks for at once, as Plan 9's `DIRMAX`
/// times a few.
const DIRREAD: usize = 16 * 1024;

/// The entries one read of directory `fd` gives; empty at the end.
pub fn dirread(fd: Fd) -> Result<Vec<DirBuf>, Error> {
    let mut buf = vec![0u8; DIRREAD];
    let n = crate::read(fd, &mut buf)?;
    Ok(abi::dirs(&buf[..n]).map(|d| DirBuf::from_dir(&d)).collect())
}

/// Every entry left in directory `fd`.
pub fn dirreadall(fd: Fd) -> Result<Vec<DirBuf>, Error> {
    let mut all = Vec::new();
    loop {
        let more = dirread(fd)?;
        if more.is_empty() {
            return Ok(all);
        }
        all.extend(more);
    }
}
