//! `#M`, the mount driver (Plan 9's `devmnt`): files served over 9P by a
//! server at the other end of a chan, a pipe end or a `#s` service. The
//! `mount` call attaches to the server ([`attach`]) and mounts the root it
//! gives; every operation on a file under it is then a 9P request.
//!
//! **Sessions.** A transport chan carries one session ([`Mnt`]), set up by
//! the first `fversion` or `mount` on it and shared by every mount over
//! it. As in Plan 9 the session lasts as long as the transport, which
//! every fid holds (and so every chan of the mount): it ends, and the
//! server sees end of file, once the last of them and of the descriptors
//! for it are gone.
//!
//! **Fids.** Each chan holds a fid of the server's ([`Fid`]), clunked when
//! the last chan holding it goes. Chans that name the same file without
//! being opened (what the mount table keeps, and the copies made from
//! them) share their fid, which nothing changes; opening, creating in and
//! removing, which change a fid, first walk a fid of their own from it
//! unless the chan holds the only reference. A chan dropped where it may
//! not sleep leaves its clunk to the session's next request.
//!
//! **Replies.** Requests are tagged, and any number may be outstanding.
//! There is no reader thread: as in Plan 9 the first waiter to find no
//! one reading reads the next reply and hands it to whoever waits for it,
//! until its own comes. A note ends a wait with a `Tflush` for the request
//! and, once the server has answered that, "interrupted", unless the
//! reply came first.

use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use abi::{Access, Dir, OpenFlags, OpenMode};
use ninep::{Elems, HDRSZ, IOHDRSZ, NOFID, NOTAG, Rmsg, Tmsg, VERSION};

use crate::port::chan::{Chan, Walkqid};
use crate::port::dev::{Dev, EVE};
use crate::port::error::{
    EBADARG, EBADSTAT, EBADUSEFD, EHUNGUP, EINTR, EMOUNTRPC, ENOATTACH, ENONEXIST, Error, Result,
};
use crate::port::lock::SpinLock;
use crate::port::proc::{self, Rendez, Sleepable};
use crate::port::qlock::QLock;

/// The largest message asked for: room for 32 KiB of data, which keeps a
/// message to one pipe block.
#[allow(clippy::cast_possible_truncation)] // 24
pub(crate) const MSIZE: u32 = IOHDRSZ as u32 + 32 * 1024;
/// The smallest a server may make it, as in Plan 9.
const MSIZE_MIN: u32 = 256;

pub(crate) struct MntDev;

pub(crate) static MNT: MntDev = MntDev;

/// A session with a server.
#[derive(Debug)]
pub(crate) struct Mnt {
    /// The device instance its chans carry.
    id: u32,
    /// The transport, open for reading and writing, which carries the
    /// session and is kept by the session's fids.
    c: Weak<Chan>,
    msize: usize,
    version: String,
    mux: SpinLock<Mux>,
    /// Waiters for replies, and for the reader's place.
    r: Rendez,
    next_fid: AtomicU32,
}

/// The outstanding requests and who reads replies.
#[derive(Debug, Default)]
struct Mux {
    reqs: Vec<Req>,
    /// Where the search for a free tag starts.
    next_tag: u16,
    /// A waiter is reading a reply.
    reader: bool,
    /// The transport failed: every request fails with this.
    dead: Option<Error>,
    /// What a reader interrupted mid-message had read of it.
    partial: Vec<u8>,
    /// Fids whose clunks wait for the next request.
    clunks: Vec<u32>,
}

#[derive(Debug)]
struct Req {
    tag: u16,
    reply: Option<Vec<u8>>,
    /// No one waits: the reply is dropped when it comes.
    abandoned: bool,
    /// For a `Tflush`, the request it flushes.
    flushes: Option<u16>,
    /// Flushed before its reply came: none will.
    flushed: bool,
}

impl Mux {
    fn find(&self, tag: u16) -> Option<usize> {
        self.reqs.iter().position(|r| r.tag == tag)
    }

    /// A new request's tag.
    fn add(&mut self, abandoned: bool, flushes: Option<u16>) -> Result<u16> {
        let tag = (0..NOTAG)
            .map(|i| self.next_tag.wrapping_add(i) % NOTAG)
            .find(|&t| self.find(t).is_none())
            .ok_or("too many outstanding 9P requests")?;
        self.next_tag = tag.wrapping_add(1) % NOTAG;
        self.reqs.push(Req {
            tag,
            reply: None,
            abandoned,
            flushes,
            flushed: false,
        });
        Ok(tag)
    }

    fn remove(&mut self, tag: u16) -> Option<Req> {
        self.find(tag).map(|i| self.reqs.swap_remove(i))
    }

    /// Stops waiting for `tag`'s reply.
    fn abandon(&mut self, tag: u16) {
        if let Some(i) = self.find(tag) {
            if self.reqs[i].reply.is_some() || self.reqs[i].flushed {
                self.reqs.swap_remove(i);
            } else {
                self.reqs[i].abandoned = true;
            }
        }
    }

    /// A reply has come: it goes to its request, or is dropped if none
    /// waits for it. A flush's reply ends the request it flushed.
    fn deliver(&mut self, msg: Vec<u8>) {
        let Some((_, tag)) = ninep::peek(&msg) else {
            return;
        };
        let Some(i) = self.find(tag) else {
            return;
        };
        if let Some(old) = self.reqs[i].flushes
            && let Some(j) = self.find(old)
            && self.reqs[j].reply.is_none()
        {
            if self.reqs[j].abandoned {
                self.reqs.swap_remove(j);
            } else {
                self.reqs[j].flushed = true;
            }
        }
        let Some(i) = self.find(tag) else {
            return;
        };
        if self.reqs[i].abandoned {
            self.reqs.swap_remove(i);
        } else {
            self.reqs[i].reply = Some(msg);
        }
    }
}

/// How a wait for a reply ended.
enum Got {
    Reply(Vec<u8>),
    /// A flush ended it with no reply.
    Flushed,
    Interrupted,
    /// The transport failed.
    Dead(Error),
}

/// Reads one whole message from `c` into `buf`, which may hold the start
/// of it already, and keeps what it has read there if it fails.
fn read_msg(c: &Chan, buf: &mut Vec<u8>, msize: usize) -> Result<()> {
    loop {
        let need = if buf.len() < 4 {
            4
        } else {
            let n = ninep::msg_size(buf).unwrap_or(0);
            if !(HDRSZ..=msize).contains(&n) {
                return Err(EMOUNTRPC.into());
            }
            if buf.len() == n {
                return Ok(());
            }
            n
        };
        let have = buf.len();
        buf.resize(need, 0);
        let got = match c.dev.read(c, &mut buf[have..], 0) {
            Ok(n) => n.min(need - have),
            Err(e) => {
                buf.truncate(have);
                return Err(e);
            }
        };
        buf.truncate(have + got);
        if got == 0 {
            return Err(EHUNGUP.into());
        }
    }
}

/// Encodes `t`, tagged `tag`.
fn encode(t: &Tmsg<'_>, tag: u16) -> Result<Vec<u8>> {
    let mut b = alloc::vec![0; t.size()];
    t.encode(tag, &mut b).map_err(|e| Error::from(e.as_str()))?;
    Ok(b)
}

impl Mnt {
    fn new_fid(&self) -> u32 {
        loop {
            let f = self.next_fid.fetch_add(1, Ordering::Relaxed);
            if f != NOFID {
                return f;
            }
        }
    }

    fn transport(&self) -> Result<Arc<Chan>> {
        self.c.upgrade().ok_or_else(|| EHUNGUP.into())
    }

    fn send(&self, t: &Tmsg<'_>, tag: u16) -> Result<()> {
        let b = encode(t, tag)?;
        if b.len() > self.msize {
            return Err("9P request too big for the session".into());
        }
        let c = self.transport()?;
        if c.dev.write(&c, &b, 0)? != b.len() {
            return Err(EMOUNTRPC.into());
        }
        Ok(())
    }

    /// Sends the clunks that drops which could not wait left behind; their
    /// replies are dropped when they come.
    fn send_clunks(&self) {
        let fids = core::mem::take(&mut self.mux.lock().clunks);
        for fid in fids {
            let Ok(tag) = self.mux.lock().add(true, None) else {
                return;
            };
            if self.send(&Tmsg::Clunk { fid }, tag).is_err() {
                self.mux.lock().remove(tag);
            }
        }
    }

    /// Waits for `tag`'s reply, reading replies while no one else does. A
    /// note ends the wait if `intr`, leaving the request outstanding;
    /// otherwise it is held over until the wait ends.
    fn wait(&self, s: &Sleepable, tag: u16, intr: bool) -> Got {
        let mut held = false;
        let got = loop {
            let read = {
                let mut m = self.mux.lock();
                let i = m.find(tag).expect("a request waits");
                if let Some(r) = m.reqs[i].reply.take() {
                    m.reqs.swap_remove(i);
                    break Got::Reply(r);
                }
                if m.reqs[i].flushed {
                    m.reqs.swap_remove(i);
                    break Got::Flushed;
                }
                if let Some(e) = m.dead.clone() {
                    m.reqs.swap_remove(i);
                    break Got::Dead(e);
                }
                if m.reader {
                    None
                } else {
                    m.reader = true;
                    Some(core::mem::take(&mut m.partial))
                }
            };
            if let Some(mut buf) = read {
                let r = self.transport().and_then(|c| read_msg(&c, &mut buf, self.msize));
                let interrupted = {
                    let mut m = self.mux.lock();
                    m.reader = false;
                    match r {
                        Ok(()) => {
                            m.deliver(buf);
                            false
                        }
                        Err(e) if e.as_str() == EINTR => {
                            m.partial = buf;
                            true
                        }
                        Err(e) => {
                            m.dead = Some(e);
                            false
                        }
                    }
                };
                // Its reply may have come, or another must read now.
                self.r.wakeup_all();
                if interrupted {
                    if intr {
                        break Got::Interrupted;
                    }
                    held = true;
                }
                continue;
            }
            let cond = || {
                let m = self.mux.lock();
                m.dead.is_some()
                    || !m.reader
                    || m.find(tag)
                        .is_none_or(|i| m.reqs[i].reply.is_some() || m.reqs[i].flushed)
            };
            if intr {
                if self.r.sleep_intr(s, cond).is_err() {
                    break Got::Interrupted;
                }
            } else {
                self.r.sleep(s, cond);
            }
        };
        if held && let Some(me) = proc::current() {
            me.interrupt();
        }
        got
    }

    /// Flushes `tag`, whose wait a note cut short, and waits until the
    /// server has answered the flush: then the request's reply, if it came
    /// first, else "interrupted".
    fn flush(&self, s: &Sleepable, tag: u16) -> Result<Vec<u8>> {
        let ftag = match self.mux.lock().add(false, Some(tag)) {
            Ok(f) => f,
            Err(e) => {
                self.mux.lock().abandon(tag);
                return Err(e);
            }
        };
        if let Err(e) = self.send(&Tmsg::Flush { oldtag: tag }, ftag) {
            let mut m = self.mux.lock();
            m.remove(ftag);
            m.abandon(tag);
            return Err(e);
        }
        let flushed = self.wait(s, ftag, false);
        let orig = self.mux.lock().remove(tag);
        match (flushed, orig.and_then(|r| r.reply)) {
            (Got::Dead(e), _) => Err(e),
            (_, Some(reply)) => Ok(reply),
            (_, None) => Err(EINTR.into()),
        }
    }

    /// Sends `t` and returns its reply, whole; a note ends the wait if
    /// `intr` (see [`Mnt::flush`]).
    fn rpc(&self, t: &Tmsg<'_>, intr: bool) -> Result<Vec<u8>> {
        let s = proc::sleepable().ok_or(EBADARG)?;
        self.send_clunks();
        let tag = self.mux.lock().add(false, None)?;
        if let Err(e) = self.send(t, tag) {
            self.mux.lock().remove(tag);
            return Err(e);
        }
        match self.wait(&s, tag, intr) {
            Got::Reply(r) => Ok(r),
            Got::Dead(e) => Err(e),
            Got::Flushed => Err(EINTR.into()),
            Got::Interrupted => self.flush(&s, tag),
        }
    }

    /// Sends `t` and gives its reply to `f`; an `Rerror` fails with its
    /// text, and a reply of the wrong type, or one `f` refuses, with
    /// "mount rpc error".
    fn call<T>(&self, t: &Tmsg<'_>, intr: bool, f: impl FnOnce(Rmsg<'_>) -> Option<T>) -> Result<T> {
        let b = self.rpc(t, intr)?;
        match Rmsg::decode(&b) {
            Ok((_, Rmsg::Error { ename })) => Err(Error::from(String::from(ename))),
            Ok((_, r)) if r.ty() == t.ty() + 1 => f(r).ok_or_else(|| EMOUNTRPC.into()),
            _ => Err(EMOUNTRPC.into()),
        }
    }

    /// Walks a new fid to where `fid` is.
    fn clone_fid(&self, fid: u32) -> Result<u32> {
        let newfid = self.new_fid();
        let wnames = Elems::new(&[]).expect("no names");
        self.call(&Tmsg::Walk { fid, newfid, wnames }, true, |r| match r {
            Rmsg::Walk { wqids } if wqids.is_empty() => Some(()),
            _ => None,
        })?;
        Ok(newfid)
    }
}

/// A fid of a session's, clunked when dropped.
#[derive(Debug)]
struct Fid {
    mnt: Arc<Mnt>,
    /// Kept for the session (Plan 9's `incref(m->c)`).
    transport: Arc<Chan>,
    num: u32,
    /// What the open said: the most a read or write should ask for, or 0.
    iounit: AtomicU32,
    /// Clunked already, by a remove.
    gone: AtomicBool,
}

impl Fid {
    fn new(mnt: &Arc<Mnt>, transport: &Arc<Chan>, num: u32) -> Arc<Fid> {
        Arc::new(Fid {
            mnt: mnt.clone(),
            transport: transport.clone(),
            num,
            iounit: AtomicU32::new(0),
            gone: AtomicBool::new(false),
        })
    }

    /// The most data one read or write carries.
    fn unit(&self) -> usize {
        let max = self.mnt.msize - IOHDRSZ;
        match self.iounit.load(Ordering::Relaxed) as usize {
            0 => max,
            n => n.min(max),
        }
    }
}

impl Drop for Fid {
    fn drop(&mut self) {
        if self.gone.load(Ordering::Relaxed) {
            return;
        }
        if proc::sleepable().is_some() {
            let _ = self.mnt.call(&Tmsg::Clunk { fid: self.num }, false, |r| {
                matches!(r, Rmsg::Clunk).then_some(())
            });
        } else {
            self.mnt.mux.lock().clunks.push(self.num);
        }
    }
}

/// Held while a session is set up.
static VERSIONING: QLock<()> = QLock::new(());
static NEXT_ID: AtomicU32 = AtomicU32::new(1);

/// The session on `c`, set up with `Tversion` if it has none: messages of
/// at most `msize` bytes (0 for [`MSIZE`]) in `version` (empty for
/// 9P2000), as `fversion` asks.
fn version(c: &Arc<Chan>, msize: u32, version: &str) -> Result<Arc<Mnt>> {
    let s = proc::sleepable().ok_or(EBADARG)?;
    let version = if version.is_empty() { VERSION } else { version };
    if !version.starts_with(VERSION) {
        return Err("bad 9P version specification".into());
    }
    let _v = VERSIONING.lock(&s);
    if let Some(m) = c.mux::<Mnt>() {
        if !version.starts_with(m.version.as_str()) {
            return Err(alloc::format!("incompatible 9P versions {} {version}", m.version).into());
        }
        return Ok(m);
    }
    if !(c.readable() && c.writable()) {
        return Err(EBADUSEFD.into());
    }
    let msize = if msize == 0 { MSIZE } else { msize.min(MSIZE) };
    if msize < MSIZE_MIN {
        return Err("bad iounit in version call".into());
    }
    let req = encode(&Tmsg::Version { msize, version }, NOTAG)?;
    if c.dev.write(c, &req, 0)? != req.len() {
        return Err("short write in fversion".into());
    }
    let mut reply = Vec::new();
    read_msg(c, &mut reply, MSIZE as usize).map_err(|e| {
        if e.as_str() == EHUNGUP {
            Error::from("EOF receiving fversion reply")
        } else {
            e
        }
    })?;
    let (got_msize, got) = match Rmsg::decode(&reply) {
        Ok((NOTAG, Rmsg::Version { msize, version })) => (msize, version),
        Ok((_, Rmsg::Error { ename })) => return Err(Error::from(String::from(ename))),
        _ => return Err("unexpected reply type in fversion".into()),
    };
    if got_msize > msize {
        return Err("server tries to increase msize in fversion".into());
    }
    if got_msize < MSIZE_MIN {
        return Err("nonsense value of msize in fversion".into());
    }
    if got.is_empty() || !version.starts_with(got) {
        return Err("bad 9P version returned from server".into());
    }
    let m = Arc::new(Mnt {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        c: Arc::downgrade(c),
        msize: got_msize as usize,
        version: String::from(got),
        mux: SpinLock::new(Mux::default()),
        r: Rendez::new(),
        next_fid: AtomicU32::new(1),
    });
    c.set_mux(m.clone());
    Ok(m)
}

/// `fversion`: the session on `c`, set up if need be; its version.
pub(crate) fn fversion(c: &Arc<Chan>, msize: u32, v: &str) -> Result<String> {
    Ok(version(c, msize, v)?.version.clone())
}

/// `fauth`: a file for authenticating to the server on `c` as the host
/// owner, for `aname`, opened for reading and writing.
pub(crate) fn auth(c: &Arc<Chan>, aname: &str) -> Result<Arc<Chan>> {
    let m = version(c, 0, "")?;
    let afid = m.new_fid();
    let qid = m.call(
        &Tmsg::Auth {
            afid,
            uname: EVE,
            aname,
        },
        true,
        |r| match r {
            Rmsg::Auth { aqid } => Some(aqid),
            _ => None,
        },
    )?;
    let rw = OpenMode {
        access: Access::ReadWrite,
        flags: OpenFlags::empty(),
    };
    Ok(Chan::new(&MNT, m.id, qid, String::from("#M"))
        .with_aux(Fid::new(&m, c, afid))
        .opened(rw))
}

/// Attaches to the server on `c` as the host owner, to its tree `aname`,
/// authenticated by `afile` (from [`auth`]) if given: the root, unopened.
pub(crate) fn attach(c: &Arc<Chan>, afile: Option<&Chan>, aname: &str) -> Result<Chan> {
    let m = version(c, 0, "")?;
    let afid = match afile {
        None => NOFID,
        Some(a) => {
            a.aux::<Fid>()
                .filter(|f| a.dev.dc() == 'M' && Arc::ptr_eq(&f.mnt, &m))
                .ok_or(EBADUSEFD)?
                .num
        }
    };
    let fid = m.new_fid();
    let qid = m.call(
        &Tmsg::Attach {
            fid,
            afid,
            uname: EVE,
            aname,
        },
        true,
        |r| match r {
            Rmsg::Attach { qid } => Some(qid),
            _ => None,
        },
    )?;
    Ok(Chan::new(&MNT, m.id, qid, String::from("#M")).with_aux(Fid::new(&m, c, fid)))
}

/// The chan carrying the session `c` is a file of, if it is one of
/// `#M`'s: Plan 9's `c->mchan`.
pub(crate) fn transport(c: &Chan) -> Option<&Arc<Chan>> {
    if c.dev.dc() != 'M' {
        return None;
    }
    c.aux::<Fid>().map(|f| &f.transport)
}

fn fid(c: &Chan) -> Result<&Fid> {
    c.aux::<Fid>().ok_or_else(|| EMOUNTRPC.into())
}

/// `c` with a fid that is its alone, to change.
fn own(c: Chan) -> Result<Chan> {
    if !c.aux_shared() {
        return Ok(c);
    }
    let f = fid(&c)?;
    let newfid = f.mnt.clone_fid(f.num)?;
    let new = Fid::new(&f.mnt, &f.transport, newfid);
    Ok(c.with_aux(new))
}

/// The mode on the wire: `CEXEC` is the kernel's business.
fn wire_mode(mode: OpenMode) -> u8 {
    let keep = 3 | OpenFlags::TRUNC.bits() | OpenFlags::RCLOSE.bits();
    #[allow(clippy::cast_possible_truncation)] // the bits kept are below 0x80
    let m = (mode.raw() & keep) as u8;
    m
}

/// The mode the chan keeps: the server removes the file on the clunk, so
/// the kernel must not.
fn chan_mode(mode: OpenMode) -> OpenMode {
    OpenMode {
        access: mode.access,
        flags: OpenFlags::from_bits(mode.flags.bits() & !OpenFlags::RCLOSE.bits()).unwrap_or(OpenFlags::empty()),
    }
}

/// Makes the stat at the start of `b` say it is this device's, as Plan 9's
/// `mntdirfix` does.
fn dirfix(b: &mut [u8], devno: u32) {
    b[2..4].copy_from_slice(&u16::from(b'M').to_le_bytes());
    b[4..8].copy_from_slice(&devno.to_le_bytes());
}

impl Dev for MntDev {
    fn dc(&self) -> char {
        'M'
    }

    fn name(&self) -> &'static str {
        "mnt"
    }

    /// Only `mount` attaches.
    fn attach(&self, _spec: &str) -> Result<Chan> {
        Err(ENOATTACH.into())
    }

    /// One `Twalk` for all the names, as Plan 9's `mntwalk`: a server
    /// that walks only some gives no new fid.
    fn walk(&self, c: &Chan, names: &[&str]) -> Result<Walkqid> {
        let wnames = Elems::new(names).ok_or("devmnt: too many name elements")?;
        let f = fid(c)?;
        let newfid = f.mnt.new_fid();
        let wqids = f.mnt.call(
            &Tmsg::Walk {
                fid: f.num,
                newfid,
                wnames,
            },
            true,
            |r| match r {
                Rmsg::Walk { wqids } => Some(wqids.as_slice().to_vec()),
                _ => None,
            },
        )?;
        if wqids.len() > names.len() {
            return Err("too many QIDs returned by walk".into());
        }
        // A server should have said why; Plan 9 says what it said last.
        if wqids.is_empty() && !names.is_empty() {
            return Err(ENONEXIST.into());
        }
        let clone = (wqids.len() == names.len()).then(|| {
            let mut nc = Chan::new(&MNT, c.devno, wqids.last().copied().unwrap_or(c.qid), String::new());
            nc.path = c.path.clone();
            nc.with_aux(Fid::new(&f.mnt, &f.transport, newfid))
        });
        Ok(Walkqid { clone, qids: wqids })
    }

    fn stat(&self, c: &Chan) -> Result<Vec<u8>> {
        let f = fid(c)?;
        let mut st = f.mnt.call(&Tmsg::Stat { fid: f.num }, true, |r| match r {
            Rmsg::Stat { stat } => Some(stat.to_vec()),
            _ => None,
        })?;
        if Dir::decode(&st).is_none_or(|(_, n)| n != st.len()) {
            return Err(EBADSTAT.into());
        }
        dirfix(&mut st, c.devno);
        Ok(st)
    }

    fn open(&self, c: Chan, mode: OpenMode) -> Result<Arc<Chan>> {
        let mut c = own(c)?;
        let f = fid(&c)?;
        let (qid, iounit) = f.mnt.call(
            &Tmsg::Open {
                fid: f.num,
                mode: wire_mode(mode),
            },
            true,
            |r| match r {
                Rmsg::Open { qid, iounit } => Some((qid, iounit)),
                _ => None,
            },
        )?;
        f.iounit.store(iounit, Ordering::Relaxed);
        c.qid = qid;
        Ok(c.opened(chan_mode(mode)))
    }

    fn create(&self, dir: Chan, name: &str, mode: OpenMode, perm: u32) -> Result<Arc<Chan>> {
        let mut c = own(dir)?;
        let f = fid(&c)?;
        let (qid, iounit) = f.mnt.call(
            &Tmsg::Create {
                fid: f.num,
                name,
                perm,
                mode: wire_mode(mode),
            },
            true,
            |r| match r {
                Rmsg::Create { qid, iounit } => Some((qid, iounit)),
                _ => None,
            },
        )?;
        f.iounit.store(iounit, Ordering::Relaxed);
        c.qid = qid;
        c.path.addelem(name, None);
        c.union = None;
        Ok(c.opened(chan_mode(mode)))
    }

    /// Reads in pieces the session allows until one comes up short; a
    /// directory reads one piece, of whole entries.
    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        let f = fid(c)?;
        let unit = f.unit();
        let mut done = 0;
        while done < buf.len() {
            let want = (buf.len() - done).min(unit);
            let dst = &mut buf[done..];
            let req = Tmsg::Read {
                fid: f.num,
                offset: offset + done as u64,
                count: u32::try_from(want).unwrap_or(u32::MAX),
            };
            let got = f.mnt.call(&req, true, |reply| match reply {
                Rmsg::Read { data } if data.len() <= want => {
                    dst[..data.len()].copy_from_slice(data);
                    Some(data.len())
                }
                _ => None,
            });
            let n = match got {
                Ok(n) => n,
                // What was read stands.
                Err(_) if done > 0 => break,
                Err(e) => return Err(e),
            };
            if c.qid.is_dir() {
                let mut at = 0;
                while let Some((_, len)) = Dir::decode(&dst[at..n]) {
                    dirfix(&mut dst[at..at + len], c.devno);
                    at += len;
                }
                return Ok(n);
            }
            done += n;
            if n < want {
                break;
            }
        }
        Ok(done)
    }

    fn write(&self, c: &Chan, buf: &[u8], offset: u64) -> Result<usize> {
        let f = fid(c)?;
        let unit = f.unit();
        let mut done = 0;
        loop {
            let want = (buf.len() - done).min(unit);
            let req = Tmsg::Write {
                fid: f.num,
                offset: offset + done as u64,
                data: &buf[done..done + want],
            };
            let got = f.mnt.call(&req, true, |reply| match reply {
                Rmsg::Write { count } if count as usize <= want => Some(count as usize),
                _ => None,
            });
            let n = match got {
                Ok(n) => n,
                Err(_) if done > 0 => break,
                Err(e) => return Err(e),
            };
            done += n;
            if n < want || done == buf.len() {
                break;
            }
        }
        Ok(done)
    }

    fn remove(&self, c: &Chan) -> Result<()> {
        let f = fid(c)?;
        // The remove clunks the fid, whatever it says.
        let fid = if c.aux_shared() {
            f.mnt.clone_fid(f.num)?
        } else {
            f.gone.store(true, Ordering::Relaxed);
            f.num
        };
        f.mnt
            .call(&Tmsg::Remove { fid }, true, |r| matches!(r, Rmsg::Remove).then_some(()))
    }

    fn wstat(&self, c: &Chan, d: &Dir<'_>) -> Result<()> {
        let f = fid(c)?;
        let mut b = alloc::vec![0; d.size()];
        d.encode(&mut b).ok_or(EBADSTAT)?;
        f.mnt.call(&Tmsg::Wstat { fid: f.num, stat: &b }, true, |r| {
            matches!(r, Rmsg::Wstat).then_some(())
        })
    }
}
