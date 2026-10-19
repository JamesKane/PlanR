//! Files' identities and descriptions: [`Qid`] and [`Dir`], with Plan 9's
//! machine-independent encoding of a `Dir` (`stat(5)`), which `stat`,
//! `wstat` and directory reads use, and 9P carries unchanged.
//!
//! An encoded `Dir`, all little-endian; a string is a `u16` length and
//! that many UTF-8 bytes, with no NUL:
//!
//! | size | |
//! |---|---|
//! | 2 | the size of the rest |
//! | 2 | type: the kernel device |
//! | 4 | dev: the device's instance |
//! | 13 | qid: type (1), version (4), path (8) |
//! | 4 | mode: permissions, and `DM` bits |
//! | 4 | atime, seconds since the epoch |
//! | 4 | mtime |
//! | 8 | length in bytes |
//! | s | name: the last element of the path |
//! | s | uid: the owner |
//! | s | gid: the group |
//! | s | muid: who last changed it |
//!
//! In a `wstat`, an all-ones number or an empty string leaves that field
//! as it is ([`Dir::DONT_TOUCH`]).

/// The size of an encoded [`Qid`].
pub const QIDSZ: usize = 13;
/// The size of an encoded [`Dir`] with empty strings (Plan 9's
/// `STATFIXLEN`).
pub const STATFIXLEN: usize = 2 + 2 + 4 + QIDSZ + 4 + 4 + 4 + 8 + 4 * 2;

/// Qid types: what kind of file a qid names.
pub const QTDIR: u8 = 0x80;
pub const QTAPPEND: u8 = 0x40;
pub const QTEXCL: u8 = 0x20;
pub const QTMOUNT: u8 = 0x10;
pub const QTAUTH: u8 = 0x08;
pub const QTTMP: u8 = 0x04;
pub const QTFILE: u8 = 0x00;

/// Mode bits above the permissions: the qid type bits, moved up.
pub const DMDIR: u32 = 0x8000_0000;
pub const DMAPPEND: u32 = 0x4000_0000;
pub const DMEXCL: u32 = 0x2000_0000;
pub const DMMOUNT: u32 = 0x1000_0000;
pub const DMAUTH: u32 = 0x0800_0000;
pub const DMTMP: u32 = 0x0400_0000;
/// Read, write and execute permission, in each of the owner, group and
/// other triples.
pub const DMREAD: u32 = 4;
pub const DMWRITE: u32 = 2;
pub const DMEXEC: u32 = 1;

/// A file's identity on its server: the same file always has the same
/// `path`; `vers` changes when it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Qid {
    pub path: u64,
    pub vers: u32,
    pub ty: u8,
}

impl Qid {
    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.ty & QTDIR != 0
    }

    /// Encodes the qid into the first [`QIDSZ`] bytes of `out`.
    pub fn encode(&self, out: &mut [u8]) {
        out[0] = self.ty;
        out[1..5].copy_from_slice(&self.vers.to_le_bytes());
        out[5..13].copy_from_slice(&self.path.to_le_bytes());
    }

    /// The qid in the first [`QIDSZ`] bytes of `b`.
    #[must_use]
    pub fn decode(b: &[u8]) -> Option<Qid> {
        let b = b.get(..QIDSZ)?;
        Some(Qid {
            ty: b[0],
            vers: u32::from_le_bytes(b[1..5].try_into().ok()?),
            path: u64::from_le_bytes(b[5..13].try_into().ok()?),
        })
    }
}

/// What `stat` says of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dir<'a> {
    pub ty: u16,
    pub dev: u32,
    pub qid: Qid,
    pub mode: u32,
    pub atime: u32,
    pub mtime: u32,
    pub length: u64,
    pub name: &'a str,
    pub uid: &'a str,
    pub gid: &'a str,
    pub muid: &'a str,
}

impl<'a> Dir<'a> {
    /// A `wstat` that changes nothing: every field "don't touch".
    pub const DONT_TOUCH: Dir<'static> = Dir {
        ty: u16::MAX,
        dev: u32::MAX,
        qid: Qid {
            path: u64::MAX,
            vers: u32::MAX,
            ty: u8::MAX,
        },
        mode: u32::MAX,
        atime: u32::MAX,
        mtime: u32::MAX,
        length: u64::MAX,
        name: "",
        uid: "",
        gid: "",
        muid: "",
    };

    /// The size of the encoding, its own size field included.
    #[must_use]
    pub fn size(&self) -> usize {
        STATFIXLEN + self.name.len() + self.uid.len() + self.gid.len() + self.muid.len()
    }

    /// Encodes the entry into `out`, returning its size, or `None` if it
    /// does not fit, or a string or the whole is too long to encode.
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        let size = self.size();
        let rest = u16::try_from(size - 2).ok()?;
        let out = out.get_mut(..size)?;
        out[0..2].copy_from_slice(&rest.to_le_bytes());
        out[2..4].copy_from_slice(&self.ty.to_le_bytes());
        out[4..8].copy_from_slice(&self.dev.to_le_bytes());
        self.qid.encode(&mut out[8..21]);
        out[21..25].copy_from_slice(&self.mode.to_le_bytes());
        out[25..29].copy_from_slice(&self.atime.to_le_bytes());
        out[29..33].copy_from_slice(&self.mtime.to_le_bytes());
        out[33..41].copy_from_slice(&self.length.to_le_bytes());
        let mut at = 41;
        for s in [self.name, self.uid, self.gid, self.muid] {
            let n = u16::try_from(s.len()).ok()?;
            out[at..at + 2].copy_from_slice(&n.to_le_bytes());
            out[at + 2..at + 2 + s.len()].copy_from_slice(s.as_bytes());
            at += 2 + s.len();
        }
        Some(size)
    }

    /// The entry at the start of `b`, and its size.
    #[must_use]
    pub fn decode(b: &'a [u8]) -> Option<(Dir<'a>, usize)> {
        let size = 2 + usize::from(u16::from_le_bytes(b.get(..2)?.try_into().ok()?));
        let b = b.get(..size)?;
        if size < STATFIXLEN {
            return None;
        }
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().expect("four bytes"));
        let mut at = 41;
        let mut strings = [""; 4];
        for s in &mut strings {
            let n = usize::from(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?));
            *s = core::str::from_utf8(b.get(at + 2..at + 2 + n)?).ok()?;
            at += 2 + n;
        }
        if at != size {
            return None;
        }
        let d = Dir {
            ty: u16::from_le_bytes([b[2], b[3]]),
            dev: u32_at(4),
            qid: Qid::decode(&b[8..])?,
            mode: u32_at(21),
            atime: u32_at(25),
            mtime: u32_at(29),
            length: u64::from_le_bytes(b[33..41].try_into().ok()?),
            name: strings[0],
            uid: strings[1],
            gid: strings[2],
            muid: strings[3],
        };
        Some((d, size))
    }
}

/// The entries in `b`, the result of a directory read, until one is cut
/// short or malformed.
pub fn dirs(mut b: &[u8]) -> impl Iterator<Item = Dir<'_>> {
    core::iter::from_fn(move || {
        let (d, n) = Dir::decode(b)?;
        b = &b[n..];
        Some(d)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Dir<'static> {
        Dir {
            ty: u16::from(b'e'),
            dev: 0,
            qid: Qid {
                path: 0x0102_0304_0506_0708,
                vers: 3,
                ty: QTFILE,
            },
            mode: 0o664,
            atime: 1,
            mtime: 2,
            length: 3,
            name: "foo",
            uid: "eve",
            gid: "eve",
            muid: "",
        }
    }

    #[test]
    fn round_trip() {
        let d = sample();
        let mut b = [0u8; 128];
        let n = d.encode(&mut b).unwrap();
        assert_eq!(n, STATFIXLEN + 9);
        assert_eq!(n, d.size());
        assert_eq!(u16::from_le_bytes([b[0], b[1]]) as usize, n - 2);
        assert_eq!(Dir::decode(&b[..n]), Some((d, n)));
        // Too small a buffer.
        assert_eq!(d.encode(&mut b[..n - 1]), None);
        assert_eq!(Dir::decode(&b[..n - 1]), None);
    }

    #[test]
    fn known_bytes() {
        // A directory named "a" owned by "u", everything else zero.
        let d = Dir {
            ty: 0,
            dev: 0,
            qid: Qid {
                path: 1,
                vers: 0,
                ty: QTDIR,
            },
            mode: DMDIR | 0o555,
            atime: 0,
            mtime: 0,
            length: 0,
            name: "a",
            uid: "u",
            gid: "",
            muid: "",
        };
        let mut b = [0u8; 64];
        let n = d.encode(&mut b).unwrap();
        let mut want = Vec::new();
        want.extend(&[49, 0, 0, 0, 0, 0, 0, 0, QTDIR, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]);
        want.extend(&(DMDIR | 0o555).to_le_bytes());
        want.extend(&[0; 16]);
        want.extend(&[1, 0, b'a', 1, 0, b'u', 0, 0, 0, 0]);
        assert_eq!(&b[..n], &want[..]);
    }

    #[test]
    fn refuses_malformed() {
        let mut b = [0u8; 128];
        let n = sample().encode(&mut b).unwrap();
        // A string running past the end.
        let mut bad = b;
        bad[41] = 200;
        assert_eq!(Dir::decode(&bad[..n]), None);
        // Bytes left over after the strings.
        let mut bad = b;
        bad[0] += 1;
        assert_eq!(Dir::decode(&bad[..=n]), None);
        // Not UTF-8.
        let mut bad = b;
        bad[43] = 0xff;
        assert_eq!(Dir::decode(&bad[..n]), None);
        assert_eq!(Dir::decode(&[1]), None);
    }

    #[test]
    fn iterates_entries() {
        let mut buf = [0u8; 256];
        let first = sample();
        let len1 = first.encode(&mut buf).unwrap();
        let second = Dir {
            name: "barbaz",
            ..first
        };
        let len2 = second.encode(&mut buf[len1..]).unwrap();
        let got: Vec<Dir<'_>> = dirs(&buf[..len1 + len2]).collect();
        assert_eq!(got, [first, second]);
        // A cut-off second entry is left out.
        assert_eq!(dirs(&buf[..len1 + len2 - 1]).count(), 1);
    }

    #[test]
    fn dont_touch() {
        let mut b = [0u8; STATFIXLEN];
        let n = Dir::DONT_TOUCH.encode(&mut b).unwrap();
        assert_eq!(n, STATFIXLEN);
        assert!(b[2..41].iter().all(|&x| x == 0xff));
    }
}
