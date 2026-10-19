//! 9P2000, the file protocol (`intro(5)`): its messages as two enums,
//! [`Tmsg`] for requests and [`Rmsg`] for replies, each with a decoder
//! that borrows from the buffer it reads (strings and data are slices of
//! it) and an encoder that writes into one, so neither allocates.
//!
//! A message, all little-endian, is
//!
//! | size | |
//! |---|---|
//! | 4 | the size of the whole message, this field included |
//! | 1 | type: `Tversion` (100) to `Rwstat` (127), T even and R odd |
//! | 2 | tag: which request a reply answers |
//! | | the fields of the type |
//!
//! A string is a `u16` length and that many UTF-8 bytes, with no NUL; a
//! qid is 13 bytes ([`abi::Qid`]); a stat is an encoded [`abi::Dir`],
//! which 9P wraps in a further `u16` count.
//!
//! Decoding takes exactly one message: a size field that does not match
//! the buffer, a field that runs past the end, bytes left over after the
//! last field, a string that is not UTF-8 or a walk of more than
//! [`MAXWELEM`] elements are all refused. So the encoding is canonical:
//! whatever decodes encodes back to the same bytes, which the tests check
//! on random input.

#![cfg_attr(not(test), no_std)]

use core::fmt;

pub use abi::{QIDSZ, Qid};

/// The protocol version this speaks.
pub const VERSION: &str = "9P2000";
/// The tag of a `Tversion`, which has no other request to answer.
pub const NOTAG: u16 = 0xffff;
/// "No fid": the `afid` of an attach without authentication.
pub const NOFID: u32 = !0;
/// The most elements one walk takes.
pub const MAXWELEM: usize = 16;
/// The room a read or write message's header needs beside its data, so
/// the most data one carries is `msize - IOHDRSZ`.
pub const IOHDRSZ: usize = 24;
/// The header every message starts with: size, type, tag.
pub const HDRSZ: usize = 7;

/// Open modes on the wire (`open(5)`): the access in the low two bits.
pub const OREAD: u8 = 0;
pub const OWRITE: u8 = 1;
pub const ORDWR: u8 = 2;
pub const OEXEC: u8 = 3;
pub const OTRUNC: u8 = 0x10;
pub const ORCLOSE: u8 = 0x40;

/// Message types.
pub const TVERSION: u8 = 100;
pub const RVERSION: u8 = 101;
pub const TAUTH: u8 = 102;
pub const RAUTH: u8 = 103;
pub const TATTACH: u8 = 104;
pub const RATTACH: u8 = 105;
pub const RERROR: u8 = 107;
pub const TFLUSH: u8 = 108;
pub const RFLUSH: u8 = 109;
pub const TWALK: u8 = 110;
pub const RWALK: u8 = 111;
pub const TOPEN: u8 = 112;
pub const ROPEN: u8 = 113;
pub const TCREATE: u8 = 114;
pub const RCREATE: u8 = 115;
pub const TREAD: u8 = 116;
pub const RREAD: u8 = 117;
pub const TWRITE: u8 = 118;
pub const RWRITE: u8 = 119;
pub const TCLUNK: u8 = 120;
pub const RCLUNK: u8 = 121;
pub const TREMOVE: u8 = 122;
pub const RREMOVE: u8 = 123;
pub const TSTAT: u8 = 124;
pub const RSTAT: u8 = 125;
pub const TWSTAT: u8 = 126;
pub const RWSTAT: u8 = 127;

/// Why a message could not be decoded or encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A field runs past the end.
    Short,
    /// The size field is not the buffer's length.
    Size,
    /// Bytes are left after the last field.
    Trailing,
    /// No such message type, or one of the other kind (a reply where a
    /// request was wanted).
    Type(u8),
    /// A string is not UTF-8.
    Utf8,
    /// A walk of more than [`MAXWELEM`] elements.
    TooManyElems,
    /// A string, data or the whole message is too long for its count.
    TooLong,
    /// The buffer is too small for the message.
    Room,
}

impl Error {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Error::Short => "short 9P message",
            Error::Size => "9P message size mismatch",
            Error::Trailing => "trailing bytes in 9P message",
            Error::Type(_) => "bad 9P message type",
            Error::Utf8 => "9P string not UTF-8",
            Error::TooManyElems => "too many 9P walk elements",
            Error::TooLong => "9P field too long",
            Error::Room => "9P message too big for its buffer",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The size field at the start of `b`, once four bytes have come: how
/// long the message is.
#[must_use]
pub fn msg_size(b: &[u8]) -> Option<usize> {
    let n = u32::from_le_bytes(b.get(..4)?.try_into().ok()?);
    usize::try_from(n).ok()
}

/// The type and tag of the message `b` starts with.
#[must_use]
pub fn peek(b: &[u8]) -> Option<(u8, u16)> {
    let h = b.get(..HDRSZ)?;
    Some((h[4], u16::from_le_bytes([h[5], h[6]])))
}

/// A list of at most `MAXWELEM` items, kept in place.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Elems<T: Copy> {
    items: [T; MAXWELEM],
    n: usize,
}

impl<T: Copy + Default> Elems<T> {
    /// The list of `items`, if there are no more than [`MAXWELEM`].
    #[must_use]
    pub fn new(items: &[T]) -> Option<Elems<T>> {
        if items.len() > MAXWELEM {
            return None;
        }
        let mut e = Elems {
            items: [T::default(); MAXWELEM],
            n: items.len(),
        };
        e.items[..items.len()].copy_from_slice(items);
        Some(e)
    }
}

impl<T: Copy> Elems<T> {
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.items[..self.n]
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.n
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }
}

impl<T: Copy + fmt::Debug> fmt::Debug for Elems<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

/// A request.
// A walk's names are kept in place, so a message needs no allocation;
// 280 bytes is a fair price.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tmsg<'a> {
    Version {
        msize: u32,
        version: &'a str,
    },
    Auth {
        afid: u32,
        uname: &'a str,
        aname: &'a str,
    },
    Attach {
        fid: u32,
        afid: u32,
        uname: &'a str,
        aname: &'a str,
    },
    Flush {
        oldtag: u16,
    },
    Walk {
        fid: u32,
        newfid: u32,
        wnames: Elems<&'a str>,
    },
    Open {
        fid: u32,
        mode: u8,
    },
    Create {
        fid: u32,
        name: &'a str,
        perm: u32,
        mode: u8,
    },
    Read {
        fid: u32,
        offset: u64,
        count: u32,
    },
    Write {
        fid: u32,
        offset: u64,
        data: &'a [u8],
    },
    Clunk {
        fid: u32,
    },
    Remove {
        fid: u32,
    },
    Stat {
        fid: u32,
    },
    /// `stat` is an encoded [`abi::Dir`].
    Wstat {
        fid: u32,
        stat: &'a [u8],
    },
}

/// A reply.
#[allow(clippy::large_enum_variant)] // as for `Tmsg`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rmsg<'a> {
    Version {
        msize: u32,
        version: &'a str,
    },
    Auth {
        aqid: Qid,
    },
    Error {
        ename: &'a str,
    },
    Flush,
    Attach {
        qid: Qid,
    },
    Walk {
        wqids: Elems<Qid>,
    },
    Open {
        qid: Qid,
        iounit: u32,
    },
    Create {
        qid: Qid,
        iounit: u32,
    },
    Read {
        data: &'a [u8],
    },
    Write {
        count: u32,
    },
    Clunk,
    Remove,
    /// `stat` is an encoded [`abi::Dir`].
    Stat {
        stat: &'a [u8],
    },
    Wstat,
}

impl<'a> Tmsg<'a> {
    /// The message type.
    #[must_use]
    pub fn ty(&self) -> u8 {
        match self {
            Tmsg::Version { .. } => TVERSION,
            Tmsg::Auth { .. } => TAUTH,
            Tmsg::Attach { .. } => TATTACH,
            Tmsg::Flush { .. } => TFLUSH,
            Tmsg::Walk { .. } => TWALK,
            Tmsg::Open { .. } => TOPEN,
            Tmsg::Create { .. } => TCREATE,
            Tmsg::Read { .. } => TREAD,
            Tmsg::Write { .. } => TWRITE,
            Tmsg::Clunk { .. } => TCLUNK,
            Tmsg::Remove { .. } => TREMOVE,
            Tmsg::Stat { .. } => TSTAT,
            Tmsg::Wstat { .. } => TWSTAT,
        }
    }

    /// The request in `b`, which holds it and nothing else, and its tag.
    pub fn decode(b: &'a [u8]) -> Result<(u16, Tmsg<'a>), Error> {
        let (ty, tag, mut d) = Dec::start(b)?;
        let m = match ty {
            TVERSION => Tmsg::Version {
                msize: d.u32()?,
                version: d.str()?,
            },
            TAUTH => Tmsg::Auth {
                afid: d.u32()?,
                uname: d.str()?,
                aname: d.str()?,
            },
            TATTACH => Tmsg::Attach {
                fid: d.u32()?,
                afid: d.u32()?,
                uname: d.str()?,
                aname: d.str()?,
            },
            TFLUSH => Tmsg::Flush { oldtag: d.u16()? },
            TWALK => {
                let (fid, newfid) = (d.u32()?, d.u32()?);
                let n = usize::from(d.u16()?);
                if n > MAXWELEM {
                    return Err(Error::TooManyElems);
                }
                let mut wnames = Elems {
                    items: [""; MAXWELEM],
                    n,
                };
                for w in &mut wnames.items[..n] {
                    *w = d.str()?;
                }
                Tmsg::Walk { fid, newfid, wnames }
            }
            TOPEN => Tmsg::Open {
                fid: d.u32()?,
                mode: d.u8()?,
            },
            TCREATE => Tmsg::Create {
                fid: d.u32()?,
                name: d.str()?,
                perm: d.u32()?,
                mode: d.u8()?,
            },
            TREAD => Tmsg::Read {
                fid: d.u32()?,
                offset: d.u64()?,
                count: d.u32()?,
            },
            TWRITE => {
                let (fid, offset) = (d.u32()?, d.u64()?);
                let n = d.u32()?;
                Tmsg::Write {
                    fid,
                    offset,
                    data: d.bytes(usize::try_from(n).map_err(|_| Error::Short)?)?,
                }
            }
            TCLUNK => Tmsg::Clunk { fid: d.u32()? },
            TREMOVE => Tmsg::Remove { fid: d.u32()? },
            TSTAT => Tmsg::Stat { fid: d.u32()? },
            TWSTAT => {
                let fid = d.u32()?;
                let n = usize::from(d.u16()?);
                Tmsg::Wstat { fid, stat: d.bytes(n)? }
            }
            _ => return Err(Error::Type(ty)),
        };
        d.end()?;
        Ok((tag, m))
    }

    /// The size of the request's encoding.
    #[must_use]
    pub fn size(&self) -> usize {
        let mut e = Enc::start(&mut [], self.ty(), 0);
        self.fields(&mut e);
        e.len()
    }

    /// Encodes the request, tagged `tag`, at the start of `out`; returns
    /// its size.
    pub fn encode(&self, tag: u16, out: &mut [u8]) -> Result<usize, Error> {
        let mut e = Enc::start(out, self.ty(), tag);
        self.fields(&mut e);
        e.finish()
    }

    fn fields(&self, e: &mut Enc<'_>) {
        match *self {
            Tmsg::Version { msize, version } => {
                e.u32(msize);
                e.str(version);
            }
            Tmsg::Auth { afid, uname, aname } => {
                e.u32(afid);
                e.str(uname);
                e.str(aname);
            }
            Tmsg::Attach {
                fid,
                afid,
                uname,
                aname,
            } => {
                e.u32(fid);
                e.u32(afid);
                e.str(uname);
                e.str(aname);
            }
            Tmsg::Flush { oldtag } => e.u16(oldtag),
            Tmsg::Walk { fid, newfid, wnames } => {
                e.u32(fid);
                e.u32(newfid);
                e.count16(wnames.len());
                for w in wnames.as_slice() {
                    e.str(w);
                }
            }
            Tmsg::Open { fid, mode } => {
                e.u32(fid);
                e.u8(mode);
            }
            Tmsg::Create { fid, name, perm, mode } => {
                e.u32(fid);
                e.str(name);
                e.u32(perm);
                e.u8(mode);
            }
            Tmsg::Read { fid, offset, count } => {
                e.u32(fid);
                e.u64(offset);
                e.u32(count);
            }
            Tmsg::Write { fid, offset, data } => {
                e.u32(fid);
                e.u64(offset);
                e.data32(data);
            }
            Tmsg::Clunk { fid } | Tmsg::Remove { fid } | Tmsg::Stat { fid } => e.u32(fid),
            Tmsg::Wstat { fid, stat } => {
                e.u32(fid);
                e.data16(stat);
            }
        }
    }
}

impl<'a> Rmsg<'a> {
    /// The message type.
    #[must_use]
    pub fn ty(&self) -> u8 {
        match self {
            Rmsg::Version { .. } => RVERSION,
            Rmsg::Auth { .. } => RAUTH,
            Rmsg::Error { .. } => RERROR,
            Rmsg::Flush => RFLUSH,
            Rmsg::Attach { .. } => RATTACH,
            Rmsg::Walk { .. } => RWALK,
            Rmsg::Open { .. } => ROPEN,
            Rmsg::Create { .. } => RCREATE,
            Rmsg::Read { .. } => RREAD,
            Rmsg::Write { .. } => RWRITE,
            Rmsg::Clunk => RCLUNK,
            Rmsg::Remove => RREMOVE,
            Rmsg::Stat { .. } => RSTAT,
            Rmsg::Wstat => RWSTAT,
        }
    }

    /// The reply in `b`, which holds it and nothing else, and its tag.
    pub fn decode(b: &'a [u8]) -> Result<(u16, Rmsg<'a>), Error> {
        let (ty, tag, mut d) = Dec::start(b)?;
        let m = match ty {
            RVERSION => Rmsg::Version {
                msize: d.u32()?,
                version: d.str()?,
            },
            RAUTH => Rmsg::Auth { aqid: d.qid()? },
            RERROR => Rmsg::Error { ename: d.str()? },
            RFLUSH => Rmsg::Flush,
            RATTACH => Rmsg::Attach { qid: d.qid()? },
            RWALK => {
                let n = usize::from(d.u16()?);
                if n > MAXWELEM {
                    return Err(Error::TooManyElems);
                }
                let mut wqids = Elems {
                    items: [Qid::default(); MAXWELEM],
                    n,
                };
                for q in &mut wqids.items[..n] {
                    *q = d.qid()?;
                }
                Rmsg::Walk { wqids }
            }
            ROPEN => Rmsg::Open {
                qid: d.qid()?,
                iounit: d.u32()?,
            },
            RCREATE => Rmsg::Create {
                qid: d.qid()?,
                iounit: d.u32()?,
            },
            RREAD => {
                let n = d.u32()?;
                Rmsg::Read {
                    data: d.bytes(usize::try_from(n).map_err(|_| Error::Short)?)?,
                }
            }
            RWRITE => Rmsg::Write { count: d.u32()? },
            RCLUNK => Rmsg::Clunk,
            RREMOVE => Rmsg::Remove,
            RSTAT => {
                let n = usize::from(d.u16()?);
                Rmsg::Stat { stat: d.bytes(n)? }
            }
            RWSTAT => Rmsg::Wstat,
            _ => return Err(Error::Type(ty)),
        };
        d.end()?;
        Ok((tag, m))
    }

    /// The size of the reply's encoding.
    #[must_use]
    pub fn size(&self) -> usize {
        let mut e = Enc::start(&mut [], self.ty(), 0);
        self.fields(&mut e);
        e.len()
    }

    /// Encodes the reply, tagged `tag`, at the start of `out`; returns its
    /// size.
    pub fn encode(&self, tag: u16, out: &mut [u8]) -> Result<usize, Error> {
        let mut e = Enc::start(out, self.ty(), tag);
        self.fields(&mut e);
        e.finish()
    }

    fn fields(&self, e: &mut Enc<'_>) {
        match *self {
            Rmsg::Version { msize, version } => {
                e.u32(msize);
                e.str(version);
            }
            Rmsg::Auth { aqid: q } | Rmsg::Attach { qid: q } => e.qid(q),
            Rmsg::Error { ename } => e.str(ename),
            Rmsg::Flush | Rmsg::Clunk | Rmsg::Remove | Rmsg::Wstat => {}
            Rmsg::Walk { wqids } => {
                e.count16(wqids.len());
                for &q in wqids.as_slice() {
                    e.qid(q);
                }
            }
            Rmsg::Open { qid, iounit } | Rmsg::Create { qid, iounit } => {
                e.qid(qid);
                e.u32(iounit);
            }
            Rmsg::Read { data } => e.data32(data),
            Rmsg::Write { count } => e.u32(count),
            Rmsg::Stat { stat } => e.data16(stat),
        }
    }
}

/// Reads fields from a message.
struct Dec<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Dec<'a> {
    /// Checks the size field and reads the header.
    fn start(b: &'a [u8]) -> Result<(u8, u16, Dec<'a>), Error> {
        let size = msg_size(b).ok_or(Error::Short)?;
        if size != b.len() {
            return Err(Error::Size);
        }
        let (ty, tag) = peek(b).ok_or(Error::Short)?;
        Ok((ty, tag, Dec { b, at: HDRSZ }))
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.at.checked_add(n).ok_or(Error::Short)?;
        let s = self.b.get(self.at..end).ok_or(Error::Short)?;
        self.at = end;
        Ok(s)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        Ok(self.bytes(N)?.try_into().expect("N bytes"))
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, Error> {
        self.array().map(u16::from_le_bytes)
    }

    fn u32(&mut self) -> Result<u32, Error> {
        self.array().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64, Error> {
        self.array().map(u64::from_le_bytes)
    }

    fn str(&mut self) -> Result<&'a str, Error> {
        let n = usize::from(self.u16()?);
        core::str::from_utf8(self.bytes(n)?).map_err(|_| Error::Utf8)
    }

    fn qid(&mut self) -> Result<Qid, Error> {
        Qid::decode(self.bytes(QIDSZ)?).ok_or(Error::Short)
    }

    fn end(&self) -> Result<(), Error> {
        if self.at == self.b.len() {
            Ok(())
        } else {
            Err(Error::Trailing)
        }
    }
}

/// Writes a message's fields into a buffer, then fills in its size. A
/// buffer too small still has every field counted, so an empty one
/// measures the message.
struct Enc<'v> {
    out: &'v mut [u8],
    at: usize,
    /// A field did not fit its count.
    bad: bool,
}

impl<'v> Enc<'v> {
    fn start(out: &'v mut [u8], ty: u8, tag: u16) -> Enc<'v> {
        let mut e = Enc { out, at: 4, bad: false };
        e.u8(ty);
        e.u16(tag);
        e
    }

    fn put(&mut self, b: &[u8]) {
        if let Some(d) = self.out.get_mut(self.at..self.at + b.len()) {
            d.copy_from_slice(b);
        }
        self.at += b.len();
    }

    fn u8(&mut self, v: u8) {
        self.put(&[v]);
    }

    fn u16(&mut self, v: u16) {
        self.put(&v.to_le_bytes());
    }

    fn u32(&mut self, v: u32) {
        self.put(&v.to_le_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.put(&v.to_le_bytes());
    }

    fn count16(&mut self, n: usize) {
        match u16::try_from(n) {
            Ok(n) => self.u16(n),
            Err(_) => self.bad = true,
        }
    }

    fn str(&mut self, s: &str) {
        self.data16(s.as_bytes());
    }

    fn data16(&mut self, b: &[u8]) {
        self.count16(b.len());
        self.put(b);
    }

    fn data32(&mut self, b: &[u8]) {
        match u32::try_from(b.len()) {
            Ok(n) => self.u32(n),
            Err(_) => self.bad = true,
        }
        self.put(b);
    }

    fn qid(&mut self, q: Qid) {
        let mut b = [0; QIDSZ];
        q.encode(&mut b);
        self.put(&b);
    }

    /// The message's size.
    fn len(&self) -> usize {
        self.at
    }

    fn finish(self) -> Result<usize, Error> {
        let size = u32::try_from(self.at)
            .ok()
            .filter(|_| !self.bad)
            .ok_or(Error::TooLong)?;
        if self.at > self.out.len() {
            return Err(Error::Room);
        }
        self.out[..4].copy_from_slice(&size.to_le_bytes());
        Ok(self.at)
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::many_single_char_names)]
mod tests {
    use super::*;

    fn qid(path: u64) -> Qid {
        Qid {
            path,
            vers: 7,
            ty: abi::QTDIR,
        }
    }

    fn requests() -> Vec<Tmsg<'static>> {
        vec![
            Tmsg::Version {
                msize: 8192,
                version: VERSION,
            },
            Tmsg::Auth {
                afid: 3,
                uname: "eve",
                aname: "",
            },
            Tmsg::Attach {
                fid: 1,
                afid: NOFID,
                uname: "eve",
                aname: "main",
            },
            Tmsg::Flush { oldtag: 9 },
            Tmsg::Walk {
                fid: 1,
                newfid: 2,
                wnames: Elems::new(&["usr", "glenda", "lib"]).unwrap(),
            },
            Tmsg::Walk {
                fid: 1,
                newfid: 2,
                wnames: Elems::new(&[]).unwrap(),
            },
            Tmsg::Open { fid: 2, mode: ORDWR },
            Tmsg::Create {
                fid: 2,
                name: "ünïcode",
                perm: 0o644,
                mode: OWRITE | OTRUNC,
            },
            Tmsg::Read {
                fid: 2,
                offset: 1 << 40,
                count: 8168,
            },
            Tmsg::Write {
                fid: 2,
                offset: 3,
                data: b"hello",
            },
            Tmsg::Clunk { fid: 2 },
            Tmsg::Remove { fid: 2 },
            Tmsg::Stat { fid: 2 },
            Tmsg::Wstat {
                fid: 2,
                stat: &[1, 2, 3],
            },
        ]
    }

    fn replies() -> Vec<Rmsg<'static>> {
        vec![
            Rmsg::Version {
                msize: 8192,
                version: VERSION,
            },
            Rmsg::Auth { aqid: qid(1) },
            Rmsg::Error { ename: "no such file" },
            Rmsg::Flush,
            Rmsg::Attach { qid: qid(2) },
            Rmsg::Walk {
                wqids: Elems::new(&[qid(3), qid(4)]).unwrap(),
            },
            Rmsg::Open { qid: qid(5), iounit: 0 },
            Rmsg::Create {
                qid: qid(6),
                iounit: 4096,
            },
            Rmsg::Read { data: b"some data" },
            Rmsg::Write { count: 5 },
            Rmsg::Clunk,
            Rmsg::Remove,
            Rmsg::Stat { stat: &[4, 5] },
            Rmsg::Wstat,
        ]
    }

    fn t(m: &Tmsg<'_>, tag: u16) -> Vec<u8> {
        let mut b = vec![0; m.size()];
        assert_eq!(m.encode(tag, &mut b), Ok(b.len()));
        b
    }

    fn r(m: &Rmsg<'_>, tag: u16) -> Vec<u8> {
        let mut b = vec![0; m.size()];
        assert_eq!(m.encode(tag, &mut b), Ok(b.len()));
        b
    }

    #[test]
    fn requests_round_trip() {
        for (i, m) in requests().into_iter().enumerate() {
            let b = t(&m, i as u16);
            assert_eq!(msg_size(&b), Some(b.len()));
            assert_eq!(Tmsg::decode(&b), Ok((i as u16, m)), "{m:?}");
            // A reply decoder refuses it.
            assert_eq!(Rmsg::decode(&b), Err(Error::Type(m.ty())));
        }
    }

    #[test]
    fn replies_round_trip() {
        for (i, m) in replies().into_iter().enumerate() {
            let b = r(&m, i as u16);
            assert_eq!(Rmsg::decode(&b), Ok((i as u16, m)), "{m:?}");
            assert_eq!(Tmsg::decode(&b), Err(Error::Type(m.ty())));
        }
    }

    #[test]
    fn known_bytes() {
        // Plan 9's Tversion, as `aux/9pcon` shows it.
        let v = Tmsg::Version {
            msize: 8192,
            version: VERSION,
        };
        assert_eq!(
            t(&v, NOTAG),
            [
                19, 0, 0, 0, 100, 0xff, 0xff, 0, 0x20, 0, 0, 6, 0, b'9', b'P', b'2', b'0', b'0', b'0'
            ]
        );
        let w = Rmsg::Walk {
            wqids: Elems::new(&[Qid {
                path: 0x0102,
                vers: 1,
                ty: abi::QTFILE,
            }])
            .unwrap(),
        };
        assert_eq!(
            r(&w, 5),
            [22, 0, 0, 0, 111, 5, 0, 1, 0, 0, 1, 0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            t(&Tmsg::Wstat { fid: 1, stat: &[9] }, 0),
            [14, 0, 0, 0, 126, 0, 0, 1, 0, 0, 0, 1, 0, 9]
        );
    }

    #[test]
    fn refuses_malformed() {
        let b = t(&Tmsg::Clunk { fid: 1 }, 1);
        // Size mismatches, either way.
        assert_eq!(Tmsg::decode(&b[..b.len() - 1]), Err(Error::Size));
        let mut long = b.clone();
        long.push(0);
        assert_eq!(Tmsg::decode(&long), Err(Error::Size));
        // A size that agrees, with a byte left over.
        long[0] += 1;
        assert_eq!(Tmsg::decode(&long), Err(Error::Trailing));
        // A field cut short.
        let mut short = b.clone();
        short.pop();
        short[0] -= 1;
        assert_eq!(Tmsg::decode(&short), Err(Error::Short));
        assert_eq!(Tmsg::decode(&[1, 0]), Err(Error::Short));
        assert_eq!(Tmsg::decode(&[4, 0, 0, 0]), Err(Error::Short));
        // Terror does not exist; nor does 99.
        let mut bad = b.clone();
        bad[4] = 106;
        assert_eq!(Tmsg::decode(&bad), Err(Error::Type(106)));
        bad[4] = 99;
        assert_eq!(Tmsg::decode(&bad), Err(Error::Type(99)));
        // A string that is not UTF-8.
        let mut e = r(&Rmsg::Error { ename: "ab" }, 1);
        e[9] = 0xff;
        assert_eq!(Rmsg::decode(&e), Err(Error::Utf8));
        // A walk of 17 elements.
        let mut w = t(
            &Tmsg::Walk {
                fid: 0,
                newfid: 1,
                wnames: Elems::new(&[]).unwrap(),
            },
            1,
        );
        w[15] = 17;
        assert_eq!(Tmsg::decode(&w), Err(Error::TooManyElems));
        // A count that runs past the end, without overflow.
        let mut rd = r(&Rmsg::Read { data: b"x" }, 1);
        rd[7..11].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(Rmsg::decode(&rd), Err(Error::Short));
        assert!(Elems::new(&["a"; 17]).is_none());
    }

    #[test]
    fn encode_refuses() {
        let name = "x".repeat(70_000);
        let m = Tmsg::Create {
            fid: 1,
            name: &name,
            perm: 0,
            mode: 0,
        };
        let mut b = vec![0; 80_000];
        assert_eq!(m.encode(1, &mut b), Err(Error::TooLong));
        // One byte short of room.
        let m = Tmsg::Clunk { fid: 1 };
        let mut b = vec![0; m.size() - 1];
        assert_eq!(m.encode(1, &mut b), Err(Error::Room));
        assert_eq!(m.size(), HDRSZ + 4);
    }

    /// xorshift64*: deterministic, so a failure repeats.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Decodes `b` both ways; whatever decodes must encode to `b` again.
    fn check(b: &[u8]) {
        if let Ok((tag, m)) = Tmsg::decode(b) {
            assert_eq!(t(&m, tag), b, "{m:?}");
        }
        if let Ok((tag, m)) = Rmsg::decode(b) {
            assert_eq!(r(&m, tag), b, "{m:?}");
        }
    }

    /// The fuzz test: valid messages with bytes changed, inserted and cut,
    /// and random bytes with a plausible header, decoded without a panic
    /// and, where they decode, canonical.
    #[test]
    fn fuzz() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut seeds = Vec::new();
        for (i, m) in requests().iter().enumerate() {
            seeds.push(t(m, i as u16));
        }
        for (i, m) in replies().iter().enumerate() {
            seeds.push(r(m, i as u16));
        }
        let mut decoded = 0;
        for _ in 0..200_000 {
            let mut b = seeds[rng.below(seeds.len())].clone();
            for _ in 0..=rng.below(4) {
                match rng.below(5) {
                    0 if !b.is_empty() => {
                        let i = rng.below(b.len());
                        b[i] = rng.next() as u8;
                    }
                    1 => {
                        let i = rng.below(b.len() + 1);
                        b.insert(i, rng.next() as u8);
                    }
                    2 if !b.is_empty() => {
                        b.truncate(rng.below(b.len()));
                    }
                    3 if b.len() > HDRSZ => {
                        // A count field made small, so strings and data
                        // shift.
                        let i = HDRSZ + rng.below(b.len() - HDRSZ);
                        b[i] = rng.below(4) as u8;
                    }
                    _ => {
                        let n = rng.below(40);
                        b = (0..n).map(|_| rng.next() as u8).collect();
                        if b.len() >= 5 {
                            b[4] = 100 + rng.below(28) as u8;
                        }
                    }
                }
            }
            // Mostly, keep the size field honest so the fields are what
            // gets exercised.
            if b.len() >= 4 && rng.below(8) != 0 {
                let n = b.len() as u32;
                b[..4].copy_from_slice(&n.to_le_bytes());
            }
            check(&b);
            decoded += usize::from(Tmsg::decode(&b).is_ok() || Rmsg::decode(&b).is_ok());
        }
        // The mutations must still reach the decoders' far ends.
        assert!(decoded > 10_000, "only {decoded} decoded");
    }
}
