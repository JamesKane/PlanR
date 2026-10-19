//! A 9P server: the fid table and the dispatch of requests, Plan 9's
//! `lib9p` `srv`, over a file system that implements [`Fs`].
//!
//! [`Srv`] does what every server must and none should do differently:
//! it negotiates the version and message size, keeps each fid's file,
//! user and open mode, refuses requests that break the protocol (an
//! unknown or duplicate fid, a walk from an open fid or from a file, a
//! read without read access, a directory opened for writing), clones and
//! moves fids for `Twalk` and answers a partial walk, packs directory
//! entries into reads and checks their offsets, and removes `ORCLOSE`
//! files at clunk. The file system answers for single files: walk one
//! name, open, create, read, write, stat, wstat, remove.
//!
//! Requests are answered one at a time, in order, so `Tflush` has nothing
//! to cancel: by the time it is read, what it names has been answered.

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use abi::{Dir, QTDIR};

use crate::{Elems, IOHDRSZ, MAXWELEM, NOFID, ORCLOSE, OREAD, Qid, Rmsg, Tmsg};

/// An error's text, as `Rerror` carries it.
pub type Ename = Cow<'static, str>;

pub type Result<T> = core::result::Result<T, Ename>;

pub const EUNKNOWNFID: &str = "unknown fid";
pub const EDUPFID: &str = "duplicate fid";
pub const EOPEN: &str = "fid already open";
pub const ENOTOPEN: &str = "fid not open";
pub const ECLONEOPEN: &str = "cannot clone open fid";
pub const EWALKNODIR: &str = "walk in non-directory";
pub const ECREATENONDIR: &str = "create in non-directory";
pub const EISDIR: &str = "file is a directory";
pub const EPERM: &str = "permission denied";
pub const EBADOFFSET: &str = "bad offset in directory read";
pub const EBADSTAT: &str = "malformed stat buffer";
pub const EBADNAME: &str = "illegal file name";
pub const ENOAUTH: &str = "authentication not required";
pub const ESMALL: &str = "version: message size too small";
pub const ENOCREATE: &str = "create prohibited";
pub const ENOWRITE: &str = "write prohibited";
pub const ENOREMOVE: &str = "remove prohibited";
pub const ENOWSTAT: &str = "wstat prohibited";

/// The smallest message size a session may have.
pub const MSIZE_MIN: u32 = 256;

/// What a file system tells `stat` of a file: an [`abi::Dir`] that owns
/// its strings. Type and device are the kernel's to fill in.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Stat {
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

impl Stat {
    /// This entry, borrowing its strings.
    #[must_use]
    pub fn as_dir(&self) -> Dir<'_> {
        Dir {
            ty: 0,
            dev: 0,
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
}

/// A file system [`Srv`] serves. `File` is what a fid holds: a handle on
/// one file, cloned when a fid is, and dropped when the last fid holding
/// it is clunked. `uname` is the user the fid was attached as.
///
/// [`Srv`] has already checked the protocol: `walk` and `create` get a
/// directory, `read` and `write` a file open for them (directories are
/// read through `dirent`), and an open or create of a directory asks only
/// to read it.
pub trait Fs {
    type File: Clone;

    /// The root of tree `aname`.
    fn attach(&mut self, uname: &str, aname: &str) -> Result<Self::File>;

    fn qid(&self, f: &Self::File) -> Qid;

    /// The file `name` in directory `dir` (`..` its parent).
    fn walk(&mut self, dir: &Self::File, name: &str, uname: &str) -> Result<Self::File>;

    /// Checks that `f` may be opened with `mode`, and does what `OTRUNC`
    /// asks.
    fn open(&mut self, f: &Self::File, mode: u8, uname: &str) -> Result<()>;

    /// Makes file `name` in `dir`, open with `mode`.
    fn create(&mut self, dir: &Self::File, name: &str, perm: u32, mode: u8, uname: &str) -> Result<Self::File> {
        let _ = (dir, name, perm, mode, uname);
        Err(ENOCREATE.into())
    }

    /// Reads at most `buf.len()` bytes from `offset`.
    fn read(&mut self, f: &Self::File, offset: u64, buf: &mut [u8]) -> Result<usize>;

    /// Entry `index` of directory `dir`, if there are that many. A read
    /// from offset 0 starts again from index 0.
    fn dirent(&mut self, dir: &Self::File, index: usize) -> Option<Stat>;

    fn write(&mut self, f: &Self::File, offset: u64, data: &[u8], uname: &str) -> Result<usize> {
        let _ = (f, offset, data, uname);
        Err(ENOWRITE.into())
    }

    fn stat(&mut self, f: &Self::File) -> Result<Stat>;

    /// Changes what `d` does not leave as [`Dir::DONT_TOUCH`]: all of it,
    /// or nothing.
    fn wstat(&mut self, f: &Self::File, d: &Dir<'_>, uname: &str) -> Result<()> {
        let _ = (f, d, uname);
        Err(ENOWSTAT.into())
    }

    fn remove(&mut self, f: &Self::File, uname: &str) -> Result<()> {
        let _ = (f, uname);
        Err(ENOREMOVE.into())
    }

    /// A fid on `f`, open with `mode` if it was, is gone (after any
    /// `ORCLOSE` remove).
    fn clunk(&mut self, f: &Self::File, mode: Option<u8>) {
        let _ = (f, mode);
    }
}

struct Fid<F> {
    file: F,
    uname: String,
    mode: Option<u8>,
    /// For a directory: where the next read must start, and the entry it
    /// starts with.
    dir_offset: u64,
    dir_index: usize,
}

/// Where a server reads requests and writes replies.
pub trait Transport {
    /// Reads some bytes; 0 at end of file.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize>;
    fn write(&mut self, buf: &[u8]) -> Result<usize>;
}

/// A trace of the messages, for `-D`.
pub type Chatty = Box<dyn FnMut(fmt::Arguments<'_>)>;

/// A server for `fs`; see the module.
pub struct Srv<F: Fs> {
    pub fs: F,
    fids: BTreeMap<u32, Fid<F::File>>,
    /// The largest message this server takes.
    max_msize: u32,
    /// The session's, once `Tversion` has set it.
    msize: u32,
    data: Vec<u8>,
    chatty: Option<Chatty>,
}

impl<F: Fs> fmt::Debug for Srv<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Srv")
            .field("fids", &self.fids.len())
            .field("msize", &self.msize)
            .finish_non_exhaustive()
    }
}

impl<F: Fs> Srv<F> {
    /// A server for `fs` taking messages of at most `max_msize` bytes.
    #[must_use]
    pub fn new(fs: F, max_msize: u32) -> Srv<F> {
        let max_msize = max_msize.max(MSIZE_MIN);
        Srv {
            fs,
            fids: BTreeMap::new(),
            max_msize,
            msize: max_msize,
            data: Vec::new(),
            chatty: None,
        }
    }

    /// Passes each message, as text, to `trace`.
    pub fn set_chatty(&mut self, trace: Chatty) {
        self.chatty = Some(trace);
    }

    /// How many fids are in use.
    #[must_use]
    pub fn fids(&self) -> usize {
        self.fids.len()
    }

    /// Answers requests from `t` until end of file. Fails on a message
    /// that is not 9P, or if `t` fails.
    pub fn serve(&mut self, t: &mut impl Transport) -> Result<()> {
        let size = self.max_msize as usize;
        let (mut msg, mut out) = (vec![0u8; size], vec![0u8; size]);
        let r = loop {
            match read_msg(t, &mut msg) {
                Ok(Some(n)) => match self.handle(&msg[..n], &mut out) {
                    Ok(k) => {
                        if let Err(e) = write_all(t, &out[..k]) {
                            break Err(e);
                        }
                    }
                    Err(e) => break Err(e.as_str().into()),
                },
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        self.clunk_all();
        r
    }

    /// Answers request `msg` with a reply in `out`, which holds a message
    /// of the largest size; returns the reply's size. Fails only if `msg`
    /// is not a request.
    pub fn handle(&mut self, msg: &[u8], out: &mut [u8]) -> core::result::Result<usize, crate::Error> {
        let (tag, t) = Tmsg::decode(msg)?;
        if let Some(trace) = &mut self.chatty {
            trace(format_args!("<- {tag} {t:?}"));
        }
        let mut data = core::mem::take(&mut self.data);
        let answer = self.answer(&t, &mut data);
        let reply = match &answer {
            Ok(m) => *m,
            Err(e) => Rmsg::Error { ename: e },
        };
        if let Some(trace) = &mut self.chatty {
            trace(format_args!("-> {tag} {reply:?}"));
        }
        let n = reply.encode(tag, out);
        drop(answer);
        self.data = data;
        n
    }

    fn iounit(&self) -> u32 {
        #[allow(clippy::cast_possible_truncation)] // 24
        let hdr = IOHDRSZ as u32;
        self.msize - hdr
    }

    fn fid(&mut self, fid: u32) -> Result<&mut Fid<F::File>> {
        self.fids.get_mut(&fid).ok_or(Cow::Borrowed(EUNKNOWNFID))
    }

    fn clunk_all(&mut self) {
        for (_, f) in core::mem::take(&mut self.fids) {
            self.fs.clunk(&f.file, f.mode);
        }
    }

    #[allow(clippy::too_many_lines)] // one arm per message
    fn answer<'d>(&mut self, t: &Tmsg<'_>, data: &'d mut Vec<u8>) -> Result<Rmsg<'d>> {
        match *t {
            Tmsg::Version { msize, version } => {
                // A new session: whatever the old one had is gone.
                self.clunk_all();
                if msize < MSIZE_MIN {
                    return Err(ESMALL.into());
                }
                self.msize = msize.min(self.max_msize);
                let version = if version.starts_with("9P2000") {
                    crate::VERSION
                } else {
                    "unknown"
                };
                Ok(Rmsg::Version {
                    msize: self.msize,
                    version,
                })
            }
            Tmsg::Auth { .. } => Err(ENOAUTH.into()),
            Tmsg::Attach {
                fid,
                afid,
                uname,
                aname,
            } => {
                if self.fids.contains_key(&fid) {
                    return Err(EDUPFID.into());
                }
                if afid != NOFID {
                    return Err(EUNKNOWNFID.into());
                }
                let file = self.fs.attach(uname, aname)?;
                let qid = self.fs.qid(&file);
                self.fids.insert(
                    fid,
                    Fid {
                        file,
                        uname: uname.into(),
                        mode: None,
                        dir_offset: 0,
                        dir_index: 0,
                    },
                );
                Ok(Rmsg::Attach { qid })
            }
            Tmsg::Flush { .. } => Ok(Rmsg::Flush),
            Tmsg::Walk { fid, newfid, wnames } => self.walk(fid, newfid, wnames.as_slice()),
            Tmsg::Open { fid, mode } => {
                let iounit = self.iounit();
                let f = self.fids.get_mut(&fid).ok_or(Cow::Borrowed(EUNKNOWNFID))?;
                if f.mode.is_some() {
                    return Err(EOPEN.into());
                }
                if self.fs.qid(&f.file).is_dir() && mode & !ORCLOSE != OREAD {
                    return Err(EISDIR.into());
                }
                self.fs.open(&f.file, mode, &f.uname)?;
                f.mode = Some(mode);
                (f.dir_offset, f.dir_index) = (0, 0);
                Ok(Rmsg::Open {
                    qid: self.fs.qid(&f.file),
                    iounit,
                })
            }
            Tmsg::Create { fid, name, perm, mode } => {
                let iounit = self.iounit();
                let f = self.fids.get_mut(&fid).ok_or(Cow::Borrowed(EUNKNOWNFID))?;
                if f.mode.is_some() {
                    return Err(EOPEN.into());
                }
                if !self.fs.qid(&f.file).is_dir() {
                    return Err(ECREATENONDIR.into());
                }
                if name.is_empty() || name == "." || name == ".." || name.contains('/') {
                    return Err(EBADNAME.into());
                }
                if perm & abi::DMDIR != 0 && mode & !ORCLOSE != OREAD {
                    return Err(EISDIR.into());
                }
                let new = self.fs.create(&f.file, name, perm, mode, &f.uname)?;
                f.file = new;
                f.mode = Some(mode);
                (f.dir_offset, f.dir_index) = (0, 0);
                Ok(Rmsg::Create {
                    qid: self.fs.qid(&f.file),
                    iounit,
                })
            }
            Tmsg::Read { fid, offset, count } => {
                let count = count.min(self.iounit()) as usize;
                let f = self.fids.get_mut(&fid).ok_or(Cow::Borrowed(EUNKNOWNFID))?;
                let mode = f.mode.ok_or(Cow::Borrowed(ENOTOPEN))?;
                if mode & 3 == crate::OWRITE {
                    return Err(EPERM.into());
                }
                data.resize(count, 0);
                if self.fs.qid(&f.file).ty & QTDIR == 0 {
                    let n = self.fs.read(&f.file, offset, &mut data[..count])?;
                    return Ok(Rmsg::Read {
                        data: &data[..n.min(count)],
                    });
                }
                if offset == 0 {
                    (f.dir_offset, f.dir_index) = (0, 0);
                } else if offset != f.dir_offset {
                    return Err(EBADOFFSET.into());
                }
                let mut n = 0;
                while let Some(s) = self.fs.dirent(&f.file, f.dir_index) {
                    match s.as_dir().encode(&mut data[n..count]) {
                        Some(k) => n += k,
                        None => break,
                    }
                    f.dir_index += 1;
                }
                f.dir_offset += n as u64;
                Ok(Rmsg::Read { data: &data[..n] })
            }
            Tmsg::Write { fid, offset, data: d } => {
                let f = self.fids.get_mut(&fid).ok_or(Cow::Borrowed(EUNKNOWNFID))?;
                let mode = f.mode.ok_or(Cow::Borrowed(ENOTOPEN))?;
                if mode & 3 == OREAD || mode & 3 == crate::OEXEC {
                    return Err(EPERM.into());
                }
                let d = &d[..d.len().min(self.msize as usize - IOHDRSZ)];
                let n = self.fs.write(&f.file, offset, d, &f.uname)?;
                Ok(Rmsg::Write {
                    count: u32::try_from(n.min(d.len())).unwrap_or(0),
                })
            }
            Tmsg::Clunk { fid } => {
                let f = self.fids.remove(&fid).ok_or(Cow::Borrowed(EUNKNOWNFID))?;
                if f.mode.is_some_and(|m| m & ORCLOSE != 0) {
                    let _ = self.fs.remove(&f.file, &f.uname);
                }
                self.fs.clunk(&f.file, f.mode);
                Ok(Rmsg::Clunk)
            }
            Tmsg::Remove { fid } => {
                // The fid goes whether or not the file does.
                let f = self.fids.remove(&fid).ok_or(Cow::Borrowed(EUNKNOWNFID))?;
                let r = self.fs.remove(&f.file, &f.uname);
                self.fs.clunk(&f.file, f.mode);
                r.map(|()| Rmsg::Remove)
            }
            Tmsg::Stat { fid } => {
                let f = self.fid(fid)?.file.clone();
                let s = self.fs.stat(&f)?;
                let d = s.as_dir();
                data.resize(d.size(), 0);
                d.encode(data).ok_or(Cow::Borrowed(EBADSTAT))?;
                Ok(Rmsg::Stat { stat: data })
            }
            Tmsg::Wstat { fid, stat } => {
                let d = match Dir::decode(stat) {
                    Some((d, n)) if n == stat.len() => d,
                    _ => return Err(EBADSTAT.into()),
                };
                let f = self.fids.get(&fid).ok_or(Cow::Borrowed(EUNKNOWNFID))?;
                self.fs.wstat(&f.file, &d, &f.uname)?;
                Ok(Rmsg::Wstat)
            }
        }
    }

    /// `Twalk`: an error only if the first name fails; a shorter list of
    /// qids, and no new fid, if a later one does.
    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<Rmsg<'static>> {
        let f = self.fids.get(&fid).ok_or(Cow::Borrowed(EUNKNOWNFID))?;
        if f.mode.is_some() {
            return Err(ECLONEOPEN.into());
        }
        if newfid != fid && self.fids.contains_key(&newfid) {
            return Err(EDUPFID.into());
        }
        let mut cur = f.file.clone();
        let uname = f.uname.clone();
        let mut qids = [Qid::default(); MAXWELEM];
        let mut n = 0;
        for name in names {
            let step = if self.fs.qid(&cur).is_dir() {
                self.fs.walk(&cur, name, &uname)
            } else {
                Err(EWALKNODIR.into())
            };
            match step {
                Ok(next) => {
                    qids[n] = self.fs.qid(&next);
                    n += 1;
                    cur = next;
                }
                Err(e) if n == 0 => return Err(e),
                Err(_) => break,
            }
        }
        if n == names.len() {
            if newfid == fid {
                if let Some(f) = self.fids.get_mut(&fid) {
                    f.file = cur;
                }
            } else {
                self.fids.insert(
                    newfid,
                    Fid {
                        file: cur,
                        uname,
                        mode: None,
                        dir_offset: 0,
                        dir_index: 0,
                    },
                );
            }
        }
        Ok(Rmsg::Walk {
            wqids: Elems::new(&qids[..n]).expect("at most MAXWELEM"),
        })
    }
}

/// Reads one message into `buf`; `None` at end of file before one starts.
fn read_msg(t: &mut impl Transport, buf: &mut [u8]) -> Result<Option<usize>> {
    let mut have = 0;
    loop {
        let need = if have < 4 {
            4
        } else {
            let n = crate::msg_size(&buf[..have]).unwrap_or(0);
            if n < crate::HDRSZ || n > buf.len() {
                return Err("9P message of a bad size".into());
            }
            if have == n {
                return Ok(Some(n));
            }
            n
        };
        match t.read(&mut buf[have..need])? {
            0 if have == 0 => return Ok(None),
            0 => return Err("end of file in a 9P message".into()),
            got => have += got,
        }
    }
}

fn write_all(t: &mut impl Transport, mut b: &[u8]) -> Result<()> {
    while !b.is_empty() {
        match t.write(b)? {
            0 => return Err("9P write wrote nothing".into()),
            n => b = &b[n.min(b.len())..],
        }
    }
    Ok(())
}
