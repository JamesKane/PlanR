//! The boot archive: the files the kernel has before it has any file
//! system. `xtask` builds it, 9boot loads it from `\planr\boot.arc`, and
//! the kernel serves it (`#/`, from which `/boot` comes) straight from the
//! memory 9boot left it in.
//!
//! The layout, all little-endian:
//!
//! | offset | size | |
//! |---|---|---|
//! | 0 | 8 | magic, `PLANRARC` |
//! | 8 | 4 | version ([`VERSION`]) |
//! | 12 | 4 | number of files |
//! | 16 | 32 each | the file table, sorted by name |
//! | | | names, then the files' contents |
//!
//! A table entry is the name's offset and length (`u32` each), the file's
//! mode (`u32`, Plan 9 permission bits), a reserved `u32`, then the
//! contents' offset and length (`u64` each). Contents start on an
//! [`ALIGN`] boundary, so with the archive loaded page aligned a page of a
//! file is a page of the archive.
//!
//! Names are paths relative to the root (`boot/init`), with no empty, `.`
//! or `..` elements. Directories are implied by the names.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::vec::Vec;
use core::fmt;

/// `PLANRARC`.
pub const MAGIC: u64 = u64::from_le_bytes(*b"PLANRARC");
pub const VERSION: u32 = 1;
/// The alignment of each file's contents.
pub const ALIGN: usize = 4096;
const HEADER: usize = 16;
const ENTRY: usize = 32;

/// Why an archive was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    TooShort,
    BadMagic,
    Version(u32),
    /// The file table runs past the end.
    BadTable,
    /// Entry `n`'s name or contents lie outside the archive.
    OutOfBounds(usize),
    /// Entry `n`'s name is not a clean relative UTF-8 path.
    BadName(usize),
    /// Entry `n` is not after entry `n - 1` in name order.
    Unsorted(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooShort => f.write_str("archive too short"),
            Error::BadMagic => f.write_str("not a boot archive"),
            Error::Version(v) => write!(f, "archive version {v}, want {VERSION}"),
            Error::BadTable => f.write_str("file table runs past the end"),
            Error::OutOfBounds(i) => write!(f, "entry {i} lies outside the archive"),
            Error::BadName(i) => write!(f, "entry {i} has a bad name"),
            Error::Unsorted(i) => write!(f, "entry {i} is out of order or repeated"),
        }
    }
}

/// A file in the archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct File<'a> {
    pub name: &'a str,
    pub mode: u32,
    pub data: &'a [u8],
}

/// A validated archive.
#[derive(Debug, Clone, Copy)]
pub struct Archive<'a> {
    data: &'a [u8],
    count: usize,
}

fn u32_at(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(d[o..o + 4].try_into().expect("four bytes"))
}

fn u64_at(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().expect("eight bytes"))
}

/// True if `name` is a relative path with no empty, `.` or `..` elements.
fn clean(name: &str) -> bool {
    !name.is_empty() && name.split('/').all(|e| !e.is_empty() && e != "." && e != "..")
}

impl<'a> Archive<'a> {
    /// Checks the header and every entry.
    pub fn parse(data: &'a [u8]) -> Result<Archive<'a>, Error> {
        if data.len() < HEADER {
            return Err(Error::TooShort);
        }
        if u64_at(data, 0) != MAGIC {
            return Err(Error::BadMagic);
        }
        let version = u32_at(data, 8);
        if version != VERSION {
            return Err(Error::Version(version));
        }
        let count = u32_at(data, 12) as usize;
        if count.checked_mul(ENTRY).is_none_or(|n| HEADER + n > data.len()) {
            return Err(Error::BadTable);
        }
        let a = Archive { data, count };
        let mut prev: Option<&str> = None;
        for i in 0..count {
            let f = a.entry(i).ok_or(Error::OutOfBounds(i))?;
            let name = core::str::from_utf8(f.0).map_err(|_| Error::BadName(i))?;
            if !clean(name) {
                return Err(Error::BadName(i));
            }
            if prev.is_some_and(|p| p >= name) {
                return Err(Error::Unsorted(i));
            }
            prev = Some(name);
        }
        Ok(a)
    }

    /// Entry `i`'s name and contents, if they lie inside the archive.
    fn entry(&self, i: usize) -> Option<(&'a [u8], u32, &'a [u8])> {
        let d = self.data;
        let o = HEADER + i * ENTRY;
        let (name_off, name_len) = (u32_at(d, o) as usize, u32_at(d, o + 4) as usize);
        let mode = u32_at(d, o + 8);
        let data_off = usize::try_from(u64_at(d, o + 16)).ok()?;
        let data_len = usize::try_from(u64_at(d, o + 24)).ok()?;
        let name = d.get(name_off..name_off.checked_add(name_len)?)?;
        let data = d.get(data_off..data_off.checked_add(data_len)?)?;
        Some((name, mode, data))
    }

    /// File `i`, in name order.
    fn file(&self, i: usize) -> File<'a> {
        let (name, mode, data) = self.entry(i).expect("checked in parse");
        File {
            name: core::str::from_utf8(name).expect("checked in parse"),
            mode,
            data,
        }
    }

    /// The number of files.
    #[must_use]
    pub fn len(&self) -> usize {
        self.count
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The files, in name order.
    pub fn files(&self) -> impl Iterator<Item = File<'a>> + '_ {
        (0..self.count).map(|i| self.file(i))
    }

    /// The file called `name`.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<File<'a>> {
        let mut lo = 0;
        let mut hi = self.count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let f = self.file(mid);
            match f.name.cmp(name) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Some(f),
            }
        }
        None
    }
}

/// Why [`build`] refused its input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    BadName(alloc::string::String),
    Repeated(alloc::string::String),
    TooBig,
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::BadName(n) => write!(f, "bad archive name {n:?}"),
            BuildError::Repeated(n) => write!(f, "{n:?} is in the archive twice"),
            BuildError::TooBig => f.write_str("archive too big"),
        }
    }
}

/// Builds an archive holding `files`, in any order.
pub fn build(files: &[File<'_>]) -> Result<Vec<u8>, BuildError> {
    let mut files = files.to_vec();
    files.sort_by(|a, b| a.name.cmp(b.name));
    for w in files.windows(2) {
        if w[0].name == w[1].name {
            return Err(BuildError::Repeated(w[0].name.into()));
        }
    }
    if let Some(f) = files.iter().find(|f| !clean(f.name)) {
        return Err(BuildError::BadName(f.name.into()));
    }
    let count = u32::try_from(files.len()).map_err(|_| BuildError::TooBig)?;
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.resize(HEADER + files.len() * ENTRY, 0);
    let mut names = Vec::new();
    for f in &files {
        names.push(out.len());
        out.extend_from_slice(f.name.as_bytes());
    }
    let too_big = |n: usize| u32::try_from(n).map_err(|_| BuildError::TooBig);
    for (i, f) in files.iter().enumerate() {
        out.resize(out.len().next_multiple_of(ALIGN), 0);
        let o = HEADER + i * ENTRY;
        let entry = [
            too_big(names[i])?.to_le_bytes(),
            too_big(f.name.len())?.to_le_bytes(),
            f.mode.to_le_bytes(),
            [0; 4],
        ];
        out[o..o + 16].copy_from_slice(entry.as_flattened());
        let at = out.len() as u64;
        out[o + 16..o + 24].copy_from_slice(&at.to_le_bytes());
        out[o + 24..o + 32].copy_from_slice(&(f.data.len() as u64).to_le_bytes());
        out.extend_from_slice(f.data);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file<'a>(name: &'a str, data: &'a [u8]) -> File<'a> {
        File {
            name,
            mode: 0o755,
            data,
        }
    }

    #[test]
    fn round_trip() {
        let files = [
            file("boot/tmem", b"memory"),
            file("boot/init", b"init"),
            file("README", b""),
        ];
        let a = build(&files).unwrap();
        let arc = Archive::parse(&a).unwrap();
        assert_eq!(arc.len(), 3);
        let names: Vec<&str> = arc.files().map(|f| f.name).collect();
        assert_eq!(names, ["README", "boot/init", "boot/tmem"]);
        assert_eq!(arc.find("boot/tmem").unwrap().data, b"memory");
        assert_eq!(arc.find("boot/init").unwrap().mode, 0o755);
        assert!(arc.find("boot").is_none());
        assert!(arc.find("boot/x").is_none());
        for f in arc.files().filter(|f| !f.data.is_empty()) {
            assert_eq!((f.data.as_ptr() as usize - a.as_ptr() as usize) % ALIGN, 0);
        }
    }

    #[test]
    fn empty_archive() {
        let a = build(&[]).unwrap();
        let arc = Archive::parse(&a).unwrap();
        assert!(arc.is_empty());
        assert!(arc.find("x").is_none());
    }

    #[test]
    fn build_refuses() {
        for bad in ["", "/boot/init", "boot//init", "boot/./init", "../x", "boot/"] {
            assert_eq!(
                build(&[file(bad, b"")]),
                Err(BuildError::BadName(bad.into())),
                "{bad:?}"
            );
        }
        assert_eq!(
            build(&[file("a", b"1"), file("a", b"2")]),
            Err(BuildError::Repeated("a".into()))
        );
    }

    #[test]
    fn parse_refuses() {
        let good = build(&[file("a", b"1"), file("b", b"2")]).unwrap();
        assert_eq!(Archive::parse(&good[..8]).unwrap_err(), Error::TooShort);
        let mut bad = good.clone();
        bad[0] ^= 1;
        assert_eq!(Archive::parse(&bad).unwrap_err(), Error::BadMagic);
        let mut bad = good.clone();
        bad[8] = 2;
        assert_eq!(Archive::parse(&bad).unwrap_err(), Error::Version(2));
        let mut bad = good.clone();
        bad[15] = 0xff;
        assert_eq!(Archive::parse(&bad).unwrap_err(), Error::BadTable);
        // Contents past the end.
        let mut bad = good.clone();
        bad[HEADER + ENTRY + 24] = 0xff;
        assert_eq!(Archive::parse(&bad).unwrap_err(), Error::OutOfBounds(1));
        // Swap the names: out of order.
        let mut bad = good.clone();
        let (a, b) = (u32_at(&good, HEADER) as usize, u32_at(&good, HEADER + ENTRY) as usize);
        bad[a] = b'b';
        bad[b] = b'a';
        assert_eq!(Archive::parse(&bad).unwrap_err(), Error::Unsorted(1));
        // A name with a slash at the start.
        let mut bad = good;
        bad[a] = b'/';
        assert_eq!(Archive::parse(&bad).unwrap_err(), Error::BadName(0));
    }
}
