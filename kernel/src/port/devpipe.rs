//! `#|`, pipes, as Plan 9's `devpipe`: each attach makes a new pipe, a
//! directory of two files, `data` and `data1`. What is written to one is
//! read from the other, a write at a time ([`Queue`]). Closing the last
//! open chan of one end hangs up the pipe: the other end reads what is
//! left and then end of file, and its writes fail. Once both ends are
//! closed the pipe can be opened again.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use abi::{Access, DMDIR, OpenMode, QTDIR, QTFILE, Qid};

use crate::port::chan::Chan;
use crate::port::dev::{self, Dev, Entry};
use crate::port::error::{EBADARG, EBADSPEC, ENONEXIST, EPERM, Result};
use crate::port::lock::SpinLock;
use crate::port::proc;
use crate::port::qio::Queue;

/// What a pipe buffers each way before writers wait (Plan 9's
/// `conf.pipeqsize` on a multiprocessor).
const PIPE_BYTES: usize = 256 * 1024;

const QDIR: u64 = 0;
const QDATA0: u64 = 1;
const QDATA1: u64 = 2;

const PERM: u32 = 0o600;

/// A pipe: `q[0]` is read from `data` and written through `data1`, `q[1]`
/// the other way.
#[derive(Debug)]
struct Pipe {
    id: u64,
    q: [Queue; 2],
    /// Open chans on `data` and `data1`.
    open: SpinLock<[usize; 2]>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) struct PipeDev;

pub(crate) static PIPE: PipeDev = PipeDev;

/// The qid of file `kind` (a `Q` constant) of pipe `id`.
fn qid(id: u64, kind: u64) -> Qid {
    Qid {
        path: id << 2 | kind,
        vers: 0,
        ty: if kind == QDIR { QTDIR } else { QTFILE },
    }
}

fn kind(c: &Chan) -> u64 {
    c.qid.path & 3
}

fn pipe(c: &Chan) -> Result<&Pipe> {
    c.aux::<Pipe>().ok_or_else(|| ENONEXIST.into())
}

fn dirgen(c: &Chan, i: usize) -> Option<Entry> {
    let p = pipe(c).ok()?;
    let (name, kind) = [("data", QDATA0), ("data1", QDATA1)].get(i).copied()?;
    let len = p.q[usize::from(kind == QDATA1)].len() as u64;
    Some(Entry::new(name, qid(p.id, kind), len, PERM))
}

/// The side of the pipe a data file is: 0 for `data`, 1 for `data1`.
fn side(c: &Chan) -> usize {
    usize::from(kind(c) == QDATA1)
}

impl Dev for PipeDev {
    fn dc(&self) -> char {
        '|'
    }

    fn name(&self) -> &'static str {
        "pipe"
    }

    fn attach(&self, spec: &str) -> Result<Chan> {
        if !spec.is_empty() {
            return Err(EBADSPEC.into());
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let p = Pipe {
            id,
            q: [Queue::new(PIPE_BYTES, false), Queue::new(PIPE_BYTES, false)],
            open: SpinLock::new([0, 0]),
        };
        Ok(Chan::new(&PIPE, 0, qid(id, QDIR), String::from("#|")).with_aux(Arc::new(p)))
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Chan> {
        dev::walk_gen(c, name, &dirgen)
    }

    fn stat(&self, c: &Chan) -> Result<Vec<u8>> {
        let e = match kind(c) {
            QDIR => Entry::new(".", c.qid, 0, DMDIR | 0o555),
            _ => dirgen(c, side(c)).ok_or(ENONEXIST)?,
        };
        Ok(e.encode(self, c.devno))
    }

    fn open(&self, c: Chan, mode: OpenMode) -> Result<Arc<Chan>> {
        if kind(&c) == QDIR {
            if mode.access != Access::Read {
                return Err(EBADARG.into());
            }
            return dev::open_checked(c, mode, DMDIR | 0o555);
        }
        let c = dev::open_checked(c, mode, PERM)?;
        pipe(&c)?.open.lock()[side(&c)] += 1;
        Ok(c)
    }

    fn close(&self, c: &Chan) {
        let Ok(p) = pipe(c) else {
            return;
        };
        if kind(c) == QDIR {
            return;
        }
        let me = side(c);
        let mut open = p.open.lock();
        open[me] -= 1;
        if open[me] == 0 {
            // Readers of the other end read what is left, then end of
            // file; writers to this end, which nothing will read, fail.
            p.q[1 - me].hangup(None);
            p.q[me].close();
        }
        if *open == [0, 0] {
            p.q[0].reopen();
            p.q[1].reopen();
        }
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        if kind(c) == QDIR {
            return dev::dirread_gen(c, buf, offset, &dirgen);
        }
        let s = proc::sleepable().ok_or(EPERM)?;
        pipe(c)?.q[side(c)].read(&s, buf)
    }

    fn write(&self, c: &Chan, buf: &[u8], _offset: u64) -> Result<usize> {
        if kind(c) == QDIR {
            return Err(EPERM.into());
        }
        let s = proc::sleepable().ok_or(EPERM)?;
        // Plan 9 also posts the writer a "sys: write on closed pipe"
        // note here; notes arrive in slice 10.
        pipe(c)?.q[1 - side(c)].write(&s, buf)
    }
}
