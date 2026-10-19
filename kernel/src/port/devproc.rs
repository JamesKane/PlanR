//! `#p`, the processes, as Plan 9's `devproc`: a directory per process,
//! named by its pid, of files that show it and control it.
//!
//! | file | |
//! |---|---|
//! | `args` | its arguments |
//! | `ctl` | `kill` ends it; `pri n` sets its priority |
//! | `fd` | its current directory, then a line per open descriptor |
//! | `mem` | its memory, read at the address as offset |
//! | `note` | a note written here is posted to it |
//! | `noteid` | its note group; writing another process's joins that |
//! | `notepg` | a note written here goes to its note group |
//! | `ns` | its name space, as `bind` and `mount` commands, one a read |
//! | `segment` | its segments |
//! | `status` | its name, user, state, times and memory, as Plan 9 lays them out |
//! | `wait` | a read waits for a child of it to exit, as `await` does |
//!
//! Every operation finds the process by pid again, and fails with
//! "process exited" once it has.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use abi::{Access, BindFlags, DMDIR, OpenMode, QTDIR, QTFILE, Qid};

use crate::port::chan::Chan;
use crate::port::clock;
use crate::port::dev::{self, Dev, EVE, Entry};
use crate::port::devmnt;
use crate::port::devsrv;
use crate::port::error::{EBADARG, EBADCTL, EBADSPEC, EINTR, ENOMEM, ENONEXIST, EPERM, EPROCDIED, Error, Result};
use crate::port::note;
use crate::port::proc::{self, Proc, State};

/// The low bits of a qid's path say which file; the rest are the pid.
const QSHIFT: u32 = 5;

/// A process's files, by their number less one.
const FILES: &[(&str, u32)] = &[
    ("args", 0o444),
    ("ctl", 0o200),
    ("fd", 0o444),
    ("mem", 0o400),
    ("note", 0o222),
    ("noteid", 0o664),
    ("notepg", 0o200),
    ("ns", 0o444),
    ("segment", 0o444),
    ("status", 0o444),
    ("wait", 0o400),
];

/// The file numbers, in `FILES`' order, plus one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum File {
    Args = 1,
    Ctl,
    Fd,
    Mem,
    Note,
    Noteid,
    Notepg,
    Ns,
    Segment,
    Status,
    Wait,
}

impl File {
    fn from_path(path: u64) -> Option<File> {
        const ALL: [File; 11] = [
            File::Args,
            File::Ctl,
            File::Fd,
            File::Mem,
            File::Note,
            File::Noteid,
            File::Notepg,
            File::Ns,
            File::Segment,
            File::Status,
            File::Wait,
        ];
        let i = usize::try_from(path & ((1 << QSHIFT) - 1)).ok()?;
        ALL.get(i.checked_sub(1)?).copied()
    }
}

const ROOT: Qid = Qid {
    path: 0,
    vers: 0,
    ty: QTDIR,
};

fn pid_of(c: &Chan) -> u64 {
    c.qid.path >> QSHIFT
}

fn dir_qid(pid: u64) -> Qid {
    Qid {
        path: pid << QSHIFT,
        vers: 0,
        ty: QTDIR,
    }
}

fn file_qid(pid: u64, i: usize) -> Qid {
    Qid {
        path: pid << QSHIFT | (i as u64 + 1),
        vers: 0,
        ty: QTFILE,
    }
}

/// The process `c` belongs to.
fn proc_of(c: &Chan) -> Result<Arc<Proc>> {
    proc::find(pid_of(c)).ok_or_else(|| EPROCDIED.into())
}

fn root_gen(pids: &[u64], i: usize) -> Option<Entry> {
    let pid = *pids.get(i)?;
    Some(Entry::new(&alloc::format!("{pid}"), dir_qid(pid), 0, DMDIR | 0o555))
}

fn proc_gen(c: &Chan, i: usize) -> Option<Entry> {
    let &(name, perm) = FILES.get(i)?;
    Some(Entry::new(name, file_qid(pid_of(c), i), 0, perm))
}

pub(crate) struct ProcDev;

pub(crate) static PROC: ProcDev = ProcDev;

impl Dev for ProcDev {
    fn dc(&self) -> char {
        'p'
    }

    fn name(&self) -> &'static str {
        "proc"
    }

    fn attach(&self, spec: &str) -> Result<Chan> {
        if !spec.is_empty() {
            return Err(EBADSPEC.into());
        }
        Ok(Chan::new(&PROC, 0, ROOT, String::from("#p")))
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Chan> {
        if c.qid.path == ROOT.path {
            let pid: u64 = name.parse().map_err(|_| ENONEXIST)?;
            if name != alloc::format!("{pid}") || proc::find(pid).is_none() {
                return Err(ENONEXIST.into());
            }
            return Ok(c.child(dir_qid(pid), name));
        }
        proc_of(c)?;
        dev::walk_gen(c, name, &proc_gen)
    }

    fn stat(&self, c: &Chan) -> Result<Vec<u8>> {
        let e = if c.qid.path == ROOT.path {
            Entry::new("#p", ROOT, 0, DMDIR | 0o555)
        } else if c.qid.is_dir() {
            proc_of(c)?;
            Entry::new(&alloc::format!("{}", pid_of(c)), c.qid, 0, DMDIR | 0o555)
        } else {
            proc_of(c)?;
            let i = File::from_path(c.qid.path).ok_or(ENONEXIST)? as usize - 1;
            Entry::new(FILES[i].0, c.qid, 0, FILES[i].1)
        };
        Ok(e.encode(self, c.devno))
    }

    fn open(&self, c: Chan, mode: OpenMode) -> Result<Arc<Chan>> {
        let perm = if c.qid.is_dir() {
            DMDIR | 0o555
        } else {
            FILES[File::from_path(c.qid.path).ok_or(ENONEXIST)? as usize - 1].1
        };
        if c.qid.path != ROOT.path {
            proc_of(&c)?;
        }
        if File::from_path(c.qid.path) == Some(File::Ns) {
            if mode.access != Access::Read {
                return Err(EPERM.into());
            }
            let c = c.with_aux(Arc::new(NsWalk::default()));
            return dev::open_checked(c, mode, perm);
        }
        dev::open_checked(c, mode, perm)
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        if c.qid.path == ROOT.path {
            let pids: Vec<u64> = proc::all().iter().map(|p| p.pid()).collect();
            return dev::dirread_gen(c, buf, offset, &|_, i| root_gen(&pids, i));
        }
        if c.qid.is_dir() {
            proc_of(c)?;
            return dev::dirread_gen(c, buf, offset, &proc_gen);
        }
        let p = proc_of(c)?;
        let text = match File::from_path(c.qid.path).ok_or(ENONEXIST)? {
            File::Args => p.args(),
            File::Fd => fds(&p)?,
            File::Mem => return mem(&p, buf, offset),
            File::Noteid => alloc::format!("{}", p.noteid()).into_bytes(),
            File::Ns => {
                let mut text = ns(&p, c)?;
                text.truncate(buf.len());
                buf[..text.len()].copy_from_slice(&text);
                return Ok(text.len());
            }
            File::Segment => segments(&p),
            File::Status => status(&p).into_bytes(),
            File::Wait => return wait(&p, buf),
            File::Ctl | File::Note | File::Notepg => return Err(EPERM.into()),
        };
        Ok(dev::read_bytes(&text, buf, offset))
    }

    fn write(&self, c: &Chan, buf: &[u8], _offset: u64) -> Result<usize> {
        if c.qid.is_dir() {
            return Err(EPERM.into());
        }
        let p = proc_of(c)?;
        let text = buf.strip_suffix(b"\n").unwrap_or(buf);
        match File::from_path(c.qid.path).ok_or(ENONEXIST)? {
            File::Ctl => {
                let words: Vec<&[u8]> = text
                    .split(|&b| b == b' ' || b == b'\t')
                    .filter(|w| !w.is_empty())
                    .collect();
                match words.as_slice() {
                    [b"kill"] => {
                        if !note::kill(&p) {
                            return Err(EPERM.into());
                        }
                    }
                    // Every process is eve's, who may set any priority.
                    [b"pri", n] => p.set_pri(usize::try_from(atoi(n).max(0)).unwrap_or(0)),
                    _ => return Err(EBADCTL.into()),
                }
            }
            File::Note => {
                if !note::post(&p, buf, note::From::User) {
                    return Err("note not posted".into());
                }
            }
            File::Notepg => note::post_group(p.noteid(), buf),
            File::Noteid => {
                let id: u64 = core::str::from_utf8(text)
                    .ok()
                    .and_then(|t| t.trim().parse().ok())
                    .ok_or(EBADARG)?;
                if !proc::all().iter().any(|q| q.noteid() == id && q.is_user()) {
                    return Err(EBADARG.into());
                }
                p.set_noteid(id);
            }
            _ => return Err(EPERM.into()),
        }
        Ok(buf.len())
    }
}

/// Plan 9's `atoi`: an optional sign and the digits that follow, else 0.
fn atoi(t: &[u8]) -> i64 {
    let (neg, digits) = match t.split_first() {
        Some((b'-', rest)) => (true, rest),
        Some((b'+', rest)) => (false, rest),
        _ => (false, t),
    };
    let n = digits
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .fold(0i64, |n, &b| n.saturating_mul(10).saturating_add(i64::from(b - b'0')));
    if neg { -n } else { n }
}

/// `status`: Plan 9's layout, fixed-width fields.
fn status(p: &Proc) -> String {
    let state = match p.state() {
        State::Ready => "Ready",
        State::Running => "Running",
        State::Sleeping => "Wakeme",
        State::Dead => "Moribund",
    };
    let real = (clock::now_ns() - p.started_ns()) / 1_000_000;
    let kb = p.memory().map_or(0, |m| {
        m.info()
            .iter()
            .filter(|s| s.kind != crate::port::segment::Kind::Stack)
            .map(|s| s.top - s.base)
            .sum::<u64>()
            >> 10
    });
    let mut name = p.name();
    name.truncate(27);
    let mut s = alloc::format!("{name:<28}{EVE:<28}{state:<12}");
    for n in [0, 0, real, 0, 0, 0, kb, p.pri() as u64, p.pri() as u64] {
        let _ = write!(s, "{n:>11} ");
    }
    s
}

/// `fd`: the current directory, then a line per descriptor.
fn fds(p: &Proc) -> Result<Vec<u8>> {
    let (dot, fgrp) = {
        let f = p.files().lock();
        let f = f.as_ref().ok_or(EPROCDIED)?;
        (f.dot.name.clone(), f.fgrp.clone())
    };
    let mut s = alloc::format!("{dot}\n");
    for (fd, c) in fgrp.list() {
        let mode = match c.mode.map(|m| m.access) {
            Some(Access::Write) => "w ",
            Some(Access::ReadWrite) => "rw",
            _ => "r ",
        };
        let _ = writeln!(
            s,
            "{fd:3} {mode} {} {:4} ({:016x} {} {:02x}) {:5} {:8} {}",
            c.dev.dc(),
            c.devno,
            c.qid.path,
            c.qid.vers,
            c.qid.ty,
            0,
            c.offset.load(core::sync::atomic::Ordering::Relaxed),
            c.name
        );
    }
    Ok(s.into_bytes())
}

/// Where a reader of `ns` has got to (Plan 9's `Mntwalk`): the id of
/// the last mount it was given, and whether it has had the `cd` line.
#[derive(Debug, Default)]
struct NsWalk {
    last: AtomicU64,
    cd_done: AtomicBool,
}

/// Plan 9's `int2flag`: a mount's flags as `bind` takes them.
fn flag_text(f: BindFlags) -> String {
    if f.bits() == 0 {
        return String::new();
    }
    let mut s = String::from("-");
    for (bit, c) in [
        (BindFlags::AFTER, 'a'),
        (BindFlags::BEFORE, 'b'),
        (BindFlags::CREATE, 'c'),
        (BindFlags::CACHE, 'C'),
    ] {
        if f.contains(bit) {
            s.push(c);
        }
    }
    s
}

/// `ns`: each read gives the next mount in the order they were made, as
/// a `bind` or `mount` command, then `cd` and the current directory,
/// then nothing; as Plan 9's.
fn ns(p: &Proc, c: &Chan) -> Result<Vec<u8>> {
    let walk = c.aux::<NsWalk>().ok_or(ENOMEM)?;
    if walk.cd_done.load(Ordering::Relaxed) {
        return Ok(Vec::new());
    }
    let (pgrp, dot) = {
        let f = p.files().lock();
        let f = f.as_ref().ok_or(EPROCDIED)?;
        (f.pgrp.clone(), f.dot.name.clone())
    };
    let Some(entry) = pgrp.next_entry(walk.last.load(Ordering::Relaxed)) else {
        walk.cd_done.store(true, Ordering::Relaxed);
        return Ok(alloc::format!("cd {dot}\n").into_bytes());
    };
    let mount = &entry.mount;
    walk.last.store(mount.id, Ordering::Relaxed);
    let flag = flag_text(mount.flags);
    let line = match devmnt::transport(&mount.to).filter(|_| mount.to.name == "#M") {
        Some(t) => alloc::format!(
            "mount {flag} {} {} {}\n",
            devsrv::name_of(t).unwrap_or_else(|| t.name.clone()),
            entry.from,
            mount.spec.as_deref().unwrap_or("")
        ),
        None => alloc::format!("bind {flag} {} {}\n", mount.to.name, entry.from),
    };
    Ok(line.into_bytes())
}

/// `segment`: a line per segment.
fn segments(p: &Proc) -> Vec<u8> {
    let mut s = String::new();
    for g in p.memory().map(|m| m.info()).unwrap_or_default() {
        let _ = writeln!(
            s,
            "{:<6} {}{} {:016x} {:016x} {:4}",
            g.kind.name(),
            if g.ronly { 'R' } else { ' ' },
            if g.cexec { 'C' } else { ' ' },
            g.base,
            g.top,
            g.refs
        );
    }
    s.into_bytes()
}

/// `mem`: the bytes at address `offset`.
fn mem(p: &Proc, buf: &mut [u8], offset: u64) -> Result<usize> {
    let m = p.memory().ok_or(EPROCDIED)?;
    m.read(offset, buf).map_err(Error::from)?;
    Ok(buf.len())
}

/// `wait`: the next wait message of a child of `p`.
fn wait(p: &Proc, buf: &mut [u8]) -> Result<usize> {
    let s = proc::sleepable().ok_or(EINTR)?;
    let w = proc::await_of(p, &s)?.ok_or("no living children")?;
    Ok(abi::format_waitmsg(w.pid, w.times, &w.msg, buf))
}
