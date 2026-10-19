//! `ramfs`: a file system held in memory, a port of Plan 9's `ramfs.c`
//! (`ramfs(4)`), its rules, messages and quirks kept.
//!
//! ```text
//! ramfs [-Dipsu] [-m mountpoint] [-S srvname]
//! ```
//!
//! It starts a server process and mounts the empty tree it serves on
//! `mountpoint` (`/tmp` by default), replacing what is there and taking
//! creates, then exits. `-s` posts the channel as `#s/ramfs` instead,
//! `-S` as `#s/srvname`; `-i` serves 9P on descriptors 0 and 1, without
//! forking. `-p` asks the kernel to keep the server's memory private, `-u`
//! lifts the limit on a file's size (768 MiB), and `-D` traces every
//! message on standard error.
//!
//! The root belongs to whoever runs `ramfs` (`/dev/user`), and a new file
//! to whoever attached. As Plan 9's, a new file takes its group and
//! `muid` from its directory, and its permissions are masked by the
//! directory's (but for a file's execute bits); `wstat` changes only the
//! mode, name, group and length (never the times or `muid`), and a write
//! never changes `muid`; groups are their own leaders.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use abi::{DMAPPEND, DMDIR, DMEXCL, Dir, QTDIR, Qid};
use ninep::{Elems, IOHDRSZ, Rmsg, Tmsg};
use r::{Access, BindFlags, OpenFlags, OpenMode, RforkFlags as F};

r::main!(run);

const NRAM: usize = 4096;
const MAXSIZE: u64 = 768 * 1024 * 1024;
const MAXFDATA: usize = 8192;
/// The largest message: a header and `MAXFDATA` bytes.
const MDATA: usize = IOHDRSZ + MAXFDATA;
const STATMAX: usize = 65535;

const EPERM: &str = "permission denied";
const ENOTDIR: &str = "not a directory";
const ENOTEXIST: &str = "file does not exist";
const EINUSE: &str = "file in use";
const EEXIST: &str = "file exists";
const EISDIR: &str = "file is a directory";
const ENOTOWNER: &str = "not owner";
const EISOPEN: &str = "file already open for I/O";
const EXCL: &str = "exclusive use file already open";
const ENAME: &str = "illegal name";
const EVERSION: &str = "unknown 9P version";
const ENOTEMPTY: &str = "directory not empty";

const PEXEC: u32 = 1;
const PWRITE: u32 = 2;
const PREAD: u32 = 4;
const POTHER: u32 = 1;
const PGROUP: u32 = 8;
const POWNER: u32 = 64;

// Open modes, as on the wire.
const OREAD: u8 = 0;
const OWRITE: u8 = 1;
const ORDWR: u8 = 2;
const OEXEC: u8 = 3;
const OPERM: u8 = 3;
const OTRUNC: u8 = 0x10;
const ORCLOSE: u8 = 0x40;

#[derive(Debug, Default, Clone)]
#[allow(clippy::struct_field_names)] // Plan 9's
struct Fid {
    busy: bool,
    open: bool,
    rclose: bool,
    fid: u32,
    user: String,
    /// Its file, by slot; `None` is Plan 9's nil.
    ram: Option<usize>,
}

#[derive(Debug, Default, Clone)]
struct Ram {
    busy: bool,
    open: u32,
    /// The directory, by slot.
    parent: usize,
    qid: Qid,
    perm: u32,
    name: String,
    atime: u32,
    mtime: u32,
    user: String,
    group: String,
    muid: String,
    data: Vec<u8>,
}

struct Ramfs {
    /// Incremented for each new file.
    path: u64,
    fids: Vec<Fid>,
    ram: Vec<Ram>,
    nram: usize,
    user: String,
    messagesize: usize,
    memlim: bool,
}

type Rep = Result<Reply, &'static str>;

/// A reply, with what it points into owned.
enum Reply {
    Version(u32, &'static str),
    Flush,
    Attach(Qid),
    Walk(Vec<Qid>),
    Open(Qid, u32),
    Create(Qid, u32),
    Read(Vec<u8>),
    Write(u32),
    Clunk,
    Remove,
    Stat(Vec<u8>),
    Wstat,
}

fn now() -> u32 {
    u32::try_from(r::time::time()).unwrap_or(u32::MAX)
}

impl Ramfs {
    fn new(user: &str) -> Ramfs {
        let t = now();
        let mut ram = vec![Ram::default(); NRAM];
        ram[0] = Ram {
            busy: true,
            perm: DMDIR | 0o775,
            qid: Qid {
                ty: QTDIR,
                vers: 0,
                path: 0,
            },
            user: user.into(),
            group: user.into(),
            muid: user.into(),
            atime: t,
            mtime: t,
            name: ".".into(),
            ..Ram::default()
        };
        Ramfs {
            path: 0,
            fids: Vec::new(),
            ram,
            nram: 1,
            user: user.into(),
            messagesize: MDATA,
            memlim: true,
        }
    }

    /// The fid numbered `fid`, made (in a free slot) if there is none.
    fn newfid(&mut self, fid: u32) -> usize {
        if let Some(i) = self.fids.iter().position(|f| f.fid == fid) {
            return i;
        }
        if let Some(i) = self.fids.iter().position(|f| !f.busy) {
            self.fids[i].fid = fid;
            return i;
        }
        self.fids.push(Fid { fid, ..Fid::default() });
        self.fids.len() - 1
    }

    fn perm(&self, f: usize, r: usize, p: u32) -> bool {
        let (f, r) = (&self.fids[f], &self.ram[r]);
        (p * POTHER) & r.perm != 0
            || f.user == r.group && (p * PGROUP) & r.perm != 0
            || f.user == r.user && (p * POWNER) & r.perm != 0
    }

    fn ramstat(&self, r: usize, nbuf: usize) -> Option<Vec<u8>> {
        let r = &self.ram[r];
        let d = Dir {
            ty: 0,
            dev: 0,
            qid: r.qid,
            mode: r.perm,
            atime: r.atime,
            mtime: r.mtime,
            length: r.data.len() as u64,
            name: &r.name,
            uid: &r.user,
            gid: &r.group,
            muid: &r.muid,
        };
        if d.size() > nbuf {
            return None;
        }
        let mut b = vec![0; d.size()];
        d.encode(&mut b)?;
        Some(b)
    }

    fn rversion(&mut self, msize: u32, version: &str) -> Rep {
        for i in 0..self.fids.len() {
            if self.fids[i].busy {
                let _ = self.rclunk(i);
            }
        }
        let msize = (msize as usize).min(MDATA);
        self.messagesize = msize;
        if !version.starts_with("9P2000") {
            return Err(EVERSION);
        }
        #[allow(clippy::cast_possible_truncation)] // at most MDATA
        Ok(Reply::Version(msize as u32, "9P2000"))
    }

    #[allow(clippy::unnecessary_wraps)] // as the other requests
    fn rattach(&mut self, f: usize, uname: &str) -> Rep {
        // No authentication!
        let fid = &mut self.fids[f];
        fid.busy = true;
        fid.rclose = false;
        fid.ram = Some(0);
        fid.user = if uname.is_empty() { "none".into() } else { uname.into() };
        if self.user == "none" {
            self.user.clone_from(&fid.user);
        }
        Ok(Reply::Attach(self.ram[0].qid))
    }

    fn clone(&mut self, f: usize, newfid: u32) -> Result<usize, &'static str> {
        if self.fids[f].open {
            return Err(EISOPEN);
        }
        let ram = self.fids[f].ram.unwrap_or(0);
        if !self.ram[ram].busy {
            return Err(ENOTEXIST);
        }
        let user = self.fids[f].user.clone();
        let nf = self.newfid(newfid);
        let n = &mut self.fids[nf];
        n.busy = true;
        n.open = false;
        n.rclose = false;
        n.ram = Some(ram);
        n.user = user;
        Ok(nf)
    }

    fn rwalk(&mut self, f: usize, fid: u32, newfid: u32, names: &[&str]) -> Rep {
        let mut f = f;
        let mut nf = None;
        if newfid != fid {
            let n = self.clone(f, newfid)?;
            nf = Some(n);
            f = n;
        }
        let mut err = None;
        let mut wqid = Vec::new();
        let mut fram = self.fids[f].ram.unwrap_or(0);
        if !names.is_empty() {
            let t = now();
            for &name in names {
                if self.ram[fram].qid.ty & QTDIR == 0 {
                    err = Some(ENOTDIR);
                    break;
                }
                if !self.ram[fram].busy {
                    err = Some(ENOTEXIST);
                    break;
                }
                self.ram[fram].atime = t;
                if name == "." {
                    wqid.push(self.ram[fram].qid);
                    continue;
                }
                // As Plan 9's: the search permission checked is the
                // parent's.
                let parent = self.ram[fram].parent;
                if !self.perm(f, parent, PEXEC) {
                    err = Some(EPERM);
                    break;
                }
                if name == ".." {
                    fram = parent;
                    wqid.push(self.ram[fram].qid);
                    continue;
                }
                let found = (0..self.nram).find(|&r| {
                    let r = &self.ram[r];
                    r.busy && r.parent == fram && r.name == name
                });
                let Some(r) = found else { break };
                fram = r;
                wqid.push(self.ram[fram].qid);
            }
            if wqid.is_empty() && err.is_none() {
                err = Some(ENOTEXIST);
            }
        }
        if nf.is_some() && (err.is_some() || wqid.len() < names.len()) {
            // Clunk the new fid, which is the one walked.
            self.fids[f].busy = false;
            self.fids[f].ram = None;
        }
        if !wqid.is_empty() {
            err = None;
        }
        if wqid.len() == names.len() {
            self.fids[f].ram = Some(fram);
        }
        match err {
            Some(e) => Err(e),
            None => Ok(Reply::Walk(wqid)),
        }
    }

    fn ropen(&mut self, f: usize, mode: u8) -> Rep {
        if self.fids[f].open {
            return Err(EISOPEN);
        }
        let r = self.fids[f].ram.unwrap_or(0);
        if !self.ram[r].busy {
            return Err(ENOTEXIST);
        }
        if self.ram[r].perm & DMEXCL != 0 && self.ram[r].open != 0 {
            return Err(EXCL);
        }
        if self.ram[r].qid.ty & QTDIR != 0 {
            if mode != OREAD {
                return Err(EPERM);
            }
            return Ok(Reply::Open(self.ram[r].qid, 0));
        }
        if mode & ORCLOSE != 0 {
            // Can't remove the root; must be able to write the parent.
            if self.ram[r].qid.path == 0 || !self.perm(f, self.ram[r].parent, PWRITE) {
                return Err(EPERM);
            }
            self.fids[f].rclose = true;
        }
        let trunc = mode & OTRUNC != 0;
        let mode = mode & OPERM;
        if (mode == OWRITE || mode == ORDWR || trunc) && !self.perm(f, r, PWRITE) {
            return Err(EPERM);
        }
        if (mode == OREAD || mode == ORDWR) && !self.perm(f, r, PREAD) {
            return Err(EPERM);
        }
        if mode == OEXEC && !self.perm(f, r, PEXEC) {
            return Err(EPERM);
        }
        if trunc && self.ram[r].perm & DMAPPEND == 0 {
            self.ram[r].data = Vec::new();
            self.ram[r].qid.vers = self.ram[r].qid.vers.wrapping_add(1);
        }
        self.fids[f].open = true;
        self.ram[r].open += 1;
        Ok(Reply::Open(self.ram[r].qid, self.iounit()))
    }

    #[allow(clippy::cast_possible_truncation)] // at most MDATA
    fn iounit(&self) -> u32 {
        (self.messagesize - IOHDRSZ) as u32
    }

    fn rcreate(&mut self, f: usize, name: &str, prm: u32, mode: u8) -> Rep {
        if self.fids[f].open {
            return Err(EISOPEN);
        }
        let parent = self.fids[f].ram.unwrap_or(0);
        if !self.ram[parent].busy {
            return Err(ENOTEXIST);
        }
        if self.ram[parent].qid.ty & QTDIR == 0 {
            return Err(ENOTDIR);
        }
        // Must be able to write the parent.
        if !self.perm(f, parent, PWRITE) {
            return Err(EPERM);
        }
        if name == "." || name == ".." {
            return Err(ENAME);
        }
        if (0..self.nram).any(|r| self.ram[r].busy && self.ram[r].parent == parent && self.ram[r].name == name) {
            return Err(EINUSE);
        }
        let Some(r) = (0..NRAM - 1).find(|&r| !self.ram[r].busy) else {
            return Err("no free ram resources");
        };
        self.path += 1;
        let dir = &self.ram[parent];
        let prm = if prm & DMDIR != 0 {
            (prm & !0o777) | (dir.perm & prm & 0o777)
        } else {
            (prm & (!0o777 | 0o111)) | (dir.perm & prm & 0o666)
        };
        let t = now();
        let (group, muid, user) = (dir.group.clone(), dir.muid.clone(), self.fids[f].user.clone());
        let mut qid = self.ram[r].qid;
        qid.path = self.path;
        qid.vers = 0;
        if prm & DMDIR != 0 {
            qid.ty |= QTDIR;
        }
        self.ram[r] = Ram {
            busy: true,
            open: 1,
            parent,
            qid,
            perm: prm,
            name: name.into(),
            atime: t,
            mtime: t,
            user,
            group,
            muid,
            data: Vec::new(),
        };
        if r >= self.nram {
            self.nram = r + 1;
        }
        self.ram[parent].mtime = t;
        self.fids[f].ram = Some(r);
        self.fids[f].open = true;
        if mode & ORCLOSE != 0 {
            self.fids[f].rclose = true;
        }
        Ok(Reply::Create(qid, self.iounit()))
    }

    fn rread(&mut self, f: usize, offset: u64, count: u32) -> Rep {
        let fr = self.fids[f].ram.unwrap_or(0);
        if !self.ram[fr].busy {
            return Err(ENOTEXIST);
        }
        #[allow(clippy::cast_possible_wrap)] // a vlong on the wire
        let mut off = offset as i64;
        if off < 0 {
            return Err("negative seek offset");
        }
        let cnt = (count as usize).min(self.messagesize);
        if self.ram[fr].qid.ty & QTDIR != 0 {
            let mut r = 1;
            while off > 0 {
                if self.ram[r].busy && self.ram[r].parent == fr {
                    let n = self.ramstat(r, STATMAX).map_or(0, |s| s.len());
                    #[allow(clippy::cast_possible_wrap)] // a stat's size
                    {
                        off -= n as i64;
                    }
                }
                if r + 1 >= self.nram {
                    return Ok(Reply::Read(Vec::new()));
                }
                r += 1;
            }
            let mut buf = Vec::new();
            while r < self.nram && buf.len() < cnt {
                if self.ram[r].busy && self.ram[r].parent == fr {
                    match self.ramstat(r, cnt - buf.len()) {
                        Some(s) => buf.extend_from_slice(&s),
                        None => break,
                    }
                }
                r += 1;
            }
            return Ok(Reply::Read(buf));
        }
        let data = &self.ram[fr].data;
        let Some(start) = usize::try_from(off).ok().filter(|&o| o < data.len()) else {
            return Ok(Reply::Read(Vec::new()));
        };
        let end = (start + cnt).min(data.len());
        let out = data[start..end].to_vec();
        self.ram[fr].atime = now();
        Ok(Reply::Read(out))
    }

    fn rwrite(&mut self, f: usize, offset: u64, data: &[u8]) -> Rep {
        let r = self.fids[f].ram.unwrap_or(0);
        if !self.ram[r].busy {
            return Err(ENOTEXIST);
        }
        #[allow(clippy::cast_possible_wrap)] // a vlong on the wire
        if (offset as i64) < 0 {
            return Err("negative seek offset");
        }
        let ram = &mut self.ram[r];
        let off = if ram.perm & DMAPPEND != 0 {
            ram.data.len() as u64
        } else {
            offset
        };
        if ram.qid.ty & QTDIR != 0 {
            return Err(EISDIR);
        }
        let cnt = data.len() as u64;
        if self.memlim && off + cnt >= MAXSIZE {
            return Err("write too big");
        }
        let Ok(off) = usize::try_from(off) else {
            return Err("write too big");
        };
        if off + data.len() > ram.data.len() {
            ram.data.resize(off + data.len(), 0);
        }
        ram.data[off..off + data.len()].copy_from_slice(data);
        ram.qid.vers = ram.qid.vers.wrapping_add(1);
        ram.mtime = now();
        #[allow(clippy::cast_possible_truncation)] // within a message
        Ok(Reply::Write(data.len() as u32))
    }

    fn emptydir(&self, dr: usize) -> bool {
        !(0..self.nram).any(|r| self.ram[r].busy && self.ram[r].parent == dr)
    }

    fn realremove(&mut self, r: usize) -> Result<(), &'static str> {
        if self.ram[r].qid.ty & QTDIR != 0 && !self.emptydir(r) {
            return Err(ENOTEMPTY);
        }
        let ram = &mut self.ram[r];
        ram.data = Vec::new();
        ram.parent = 0;
        ram.qid = Qid::default();
        ram.name = String::new();
        ram.busy = false;
        Ok(())
    }

    fn rclunk(&mut self, f: usize) -> Rep {
        let mut e = Ok(());
        if let Some(r) = self.fids[f].ram {
            if self.fids[f].open {
                self.ram[r].open = self.ram[r].open.saturating_sub(1);
            }
            if self.fids[f].rclose {
                e = self.realremove(r);
            }
        }
        let fid = &mut self.fids[f];
        fid.busy = false;
        fid.open = false;
        fid.ram = None;
        e.map(|()| Reply::Clunk)
    }

    fn rremove(&mut self, f: usize) -> Rep {
        let r = self.fids[f].ram.unwrap_or(0);
        if self.fids[f].open {
            self.ram[r].open = self.ram[r].open.saturating_sub(1);
        }
        self.fids[f].busy = false;
        self.fids[f].open = false;
        let ok = self.ram[r].qid.path != 0 && self.perm(f, self.ram[r].parent, PWRITE);
        self.fids[f].ram = None;
        if !ok {
            return Err(EPERM);
        }
        let parent = self.ram[r].parent;
        self.ram[parent].mtime = now();
        self.realremove(r).map(|()| Reply::Remove)
    }

    fn rstat(&self, f: usize) -> Rep {
        let r = self.fids[f].ram.unwrap_or(0);
        if !self.ram[r].busy {
            return Err(ENOTEXIST);
        }
        Ok(Reply::Stat(self.ramstat(r, STATMAX).unwrap_or_default()))
    }

    fn rwstat(&mut self, f: usize, stat: &[u8]) -> Rep {
        let r = self.fids[f].ram.unwrap_or(0);
        if !self.ram[r].busy {
            return Err(ENOTEXIST);
        }
        let Some((dir, _)) = Dir::decode(stat) else {
            return Err("bad stat");
        };
        let user = self.fids[f].user.clone();
        let ram = &self.ram[r];
        // To change the length, must have write permission on the file.
        if dir.length != u64::MAX && dir.length != ram.data.len() as u64 && !self.perm(f, r, PWRITE) {
            return Err(EPERM);
        }
        // To change the name, must have write permission in the parent,
        // and the name must be unique.
        if !dir.name.is_empty() && dir.name != ram.name {
            if !self.perm(f, ram.parent, PWRITE) {
                return Err(EPERM);
            }
            if (0..self.nram)
                .any(|s| self.ram[s].busy && self.ram[s].parent == ram.parent && self.ram[s].name == dir.name)
            {
                return Err(EEXIST);
            }
        }
        // To change the mode, must be owner or group leader; each group
        // is its own leader.
        if dir.mode != u32::MAX && ram.perm != dir.mode && user != ram.user && user != ram.group {
            return Err(ENOTOWNER);
        }
        // To change the group, must be the owner (Plan 9's check of the
        // new group is commented out), or leader of both groups.
        if !dir.gid.is_empty() && ram.group != dir.gid && user != ram.user && !(user == ram.group && user == dir.gid) {
            return Err(ENOTOWNER);
        }
        let ram = &mut self.ram[r];
        if dir.mode != u32::MAX {
            // The directory bit cannot change.
            ram.perm = (dir.mode & !DMDIR) | (ram.perm & DMDIR);
        }
        if !dir.name.is_empty() {
            ram.name = dir.name.into();
        }
        if !dir.gid.is_empty() {
            ram.group = dir.gid.into();
        }
        if dir.length != u64::MAX && dir.length != ram.data.len() as u64 {
            let Ok(len) = usize::try_from(dir.length) else {
                return Err("out of memory");
            };
            ram.data.resize(len, 0);
        }
        let parent = ram.parent;
        self.ram[parent].mtime = now();
        Ok(Reply::Wstat)
    }

    /// Answers `t`, sent on `fid` if it is one: `None` for a message that
    /// is not a request.
    fn handle(&mut self, t: &Tmsg<'_>) -> Rep {
        let fid = match *t {
            Tmsg::Walk { fid, .. }
            | Tmsg::Open { fid, .. }
            | Tmsg::Create { fid, .. }
            | Tmsg::Read { fid, .. }
            | Tmsg::Write { fid, .. }
            | Tmsg::Clunk { fid }
            | Tmsg::Remove { fid }
            | Tmsg::Stat { fid }
            | Tmsg::Wstat { fid, .. } => Some(fid),
            Tmsg::Attach { fid, .. } => {
                let f = self.newfid(fid);
                return self.rattach_on(f, t);
            }
            _ => None,
        };
        let f = match fid {
            Some(fid) => {
                let f = self.newfid(fid);
                if self.fids[f].ram.is_none() {
                    return Err("fid not in use");
                }
                f
            }
            None => 0,
        };
        match *t {
            Tmsg::Version { msize, version } => self.rversion(msize, version),
            Tmsg::Auth { .. } => Err("ramfs: no authentication required"),
            Tmsg::Flush { .. } => Ok(Reply::Flush),
            Tmsg::Attach { .. } => unreachable!("handled above"),
            Tmsg::Walk { fid, newfid, wnames } => self.rwalk(f, fid, newfid, wnames.as_slice()),
            Tmsg::Open { mode, .. } => self.ropen(f, mode),
            Tmsg::Create { name, perm, mode, .. } => self.rcreate(f, name, perm, mode),
            Tmsg::Read { offset, count, .. } => self.rread(f, offset, count),
            Tmsg::Write { offset, data, .. } => self.rwrite(f, offset, data),
            Tmsg::Clunk { .. } => self.rclunk(f),
            Tmsg::Remove { .. } => self.rremove(f),
            Tmsg::Stat { .. } => self.rstat(f),
            Tmsg::Wstat { stat, .. } => self.rwstat(f, stat),
        }
    }

    fn rattach_on(&mut self, f: usize, t: &Tmsg<'_>) -> Rep {
        let Tmsg::Attach { uname, .. } = *t else {
            unreachable!("an attach")
        };
        self.rattach(f, uname)
    }
}

fn usage() -> ! {
    r::eprintln!("usage: ramfs [-Dipsu] [-m mountpoint] [-S srvname]");
    r::exits(Some(b"usage"))
}

/// Says `ramfs: s: error` and exits with `s` (Plan 9's `error`).
fn error(s: &str, e: &r::Error) -> ! {
    r::eprintln!("ramfs: {s}: {e}");
    r::exits(Some(s.as_bytes()))
}

/// Reads one 9P message from `fd` into `buf`; its length, or 0 at the
/// end of the input.
fn read9pmsg(fd: r::Fd, buf: &mut [u8]) -> Result<usize, r::Error> {
    let mut n = 0;
    while n < 4 {
        match r::read(fd, &mut buf[n..4])? {
            0 => return Ok(0),
            k => n += k,
        }
    }
    let size = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if size > buf.len() || size < 4 {
        return Err(r::Error::from_text(b"bad length in 9P2000 message header"));
    }
    while n < size {
        match r::read(fd, &mut buf[n..size])? {
            0 => return Err(r::Error::from_text(b"short 9P message")),
            k => n += k,
        }
    }
    Ok(size)
}

fn io(fs: &mut Ramfs, rfd: r::Fd, wfd: r::Fd, debug: bool) -> ! {
    let pid = r::getpid().unwrap_or(0);
    let mut mdata = vec![0u8; MDATA];
    let mut out = vec![0u8; MDATA + STATMAX];
    loop {
        let n = match read9pmsg(rfd, &mut mdata) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(e) => {
                let s = e.as_bytes();
                if s.is_empty() || s.windows(6).any(|w| w == b"hungup") {
                    r::exits(Some(b""));
                }
                error("mount read", &e);
            }
        };
        let Ok((tag, t)) = Tmsg::decode(&mdata[..n]) else {
            continue;
        };
        if debug {
            r::eprintln!("ramfs {pid}:<-{t:?}");
        }
        let rep = fs.handle(&t);
        let r = match &rep {
            Err(ename) => Rmsg::Error { ename },
            Ok(Reply::Version(msize, version)) => Rmsg::Version { msize: *msize, version },
            Ok(Reply::Flush) => Rmsg::Flush,
            Ok(Reply::Attach(qid)) => Rmsg::Attach { qid: *qid },
            Ok(Reply::Walk(q)) => Rmsg::Walk {
                wqids: Elems::new(q).unwrap_or_else(|| Elems::new(&[]).expect("empty")),
            },
            Ok(Reply::Open(qid, iounit)) => Rmsg::Open {
                qid: *qid,
                iounit: *iounit,
            },
            Ok(Reply::Create(qid, iounit)) => Rmsg::Create {
                qid: *qid,
                iounit: *iounit,
            },
            Ok(Reply::Read(data)) => Rmsg::Read { data },
            Ok(Reply::Write(count)) => Rmsg::Write { count: *count },
            Ok(Reply::Clunk) => Rmsg::Clunk,
            Ok(Reply::Remove) => Rmsg::Remove,
            Ok(Reply::Stat(stat)) => Rmsg::Stat { stat },
            Ok(Reply::Wstat) => Rmsg::Wstat,
        };
        if debug {
            r::eprintln!("ramfs {pid}:->{r:?}");
        }
        let n = r
            .encode(tag, &mut out[..fs.messagesize.max(IOHDRSZ)])
            .unwrap_or_else(|_| error("convS2M error on write", &r::Error::from_text(b"")));
        if r::write(wfd, &out[..n]) != Ok(n) {
            error("mount write", &r::Error::last());
        }
    }
}

extern "C" fn notifyf(_: *mut r::Ureg, msg: *const u8) {
    // SAFETY: the kernel passes the note's NUL-terminated text.
    let msg = unsafe { core::ffi::CStr::from_ptr(msg.cast()) };
    let how = if msg.to_bytes().starts_with(b"interrupt") {
        r::NoteHow::Cont
    } else {
        r::NoteHow::Dflt
    };
    let _ = r::noted(how);
}

#[allow(clippy::unnecessary_wraps)] // the signature `main!` wants
fn run(args: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(args.iter());
    let mut service: &[u8] = b"ramfs";
    let mut defmnt: Option<&[u8]> = Some(b"/tmp");
    let (mut stdio, mut private, mut debug, mut memlim) = (false, false, false, true);
    while let Some(c) = o.next() {
        match c {
            'i' => {
                defmnt = None;
                stdio = true;
            }
            'm' => defmnt = Some(o.arg().unwrap_or_else(|| usage())),
            'p' => private = true,
            's' => defmnt = None,
            'u' => memlim = false,
            'D' => debug = true,
            'S' => {
                defmnt = None;
                service = o.arg().unwrap_or_else(|| usage());
            }
            _ => usage(),
        }
    }
    let [p0, p1] = r::pipe().unwrap_or_else(|e| error("pipe failed", &e));
    let (mut rfd, mut wfd) = (0, 1);
    if !stdio {
        (rfd, wfd) = (p0, p0);
        if defmnt.is_none() {
            let name = [&b"#s/"[..], service].concat();
            let mode = OpenMode {
                access: Access::Write,
                flags: OpenFlags::RCLOSE,
            };
            let fd = r::cstr(&name)
                .and_then(|c| r::create(&c, mode, 0o666))
                .unwrap_or_else(|e| error("create failed", &e));
            if let Err(e) = r::fprint!(fd, "{p1}") {
                error("writing service file", &e);
            }
        }
    }
    let user = r::read_all(c"/dev/user").unwrap_or_else(|_| b"none".to_vec());
    let user = String::from_utf8_lossy(&user);
    let _ = r::notify(Some(notifyf));
    let mut fs = Ramfs::new(user.trim());
    fs.memlim = memlim;
    match r::rfork(F::FDG | F::PROC | F::NAMEG | F::NOTEG) {
        Err(e) => error("fork", &e),
        Ok(0) => {
            let _ = r::close(p1);
            if private {
                protect();
            }
            io(&mut fs, rfd, wfd, debug);
        }
        Ok(_) => {
            // Don't deadlock if the child fails.
            let _ = r::close(p0);
            if let Some(m) = defmnt {
                let m = r::cstr(m).unwrap_or_else(|e| error("mount failed", &e));
                if let Err(e) = r::mount(p1, None, &m, BindFlags::CREATE, c"") {
                    error("mount failed", &e);
                }
            }
        }
    }
    Ok(())
}

/// `-p`: asks for the server's memory to be kept from others.
fn protect() {
    let pid = r::getpid().unwrap_or(0);
    let mode = OpenMode {
        access: Access::Write,
        flags: OpenFlags::empty(),
    };
    let ctl = r::cstr(alloc::format!("/proc/{pid}/ctl").as_bytes()).and_then(|c| r::open(&c, mode));
    match ctl {
        Ok(fd) => {
            let _ = r::fprint!(fd, "noswap\n");
            let _ = r::fprint!(fd, "private\n");
            let _ = r::close(fd);
        }
        Err(_) => r::eprintln!("can't protect ramfs"),
    }
}
