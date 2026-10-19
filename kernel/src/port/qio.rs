//! I/O queues, Plan 9's `qio`: bytes passed from writers to readers in
//! [`Block`]s, with flow control.
//!
//! **Blocks.** A write of up to [`MAX_ATOMIC`] bytes is one block, and a
//! longer one is cut into blocks of that size. A read takes at most one
//! block, so it never returns more than one write's bytes: what is left
//! of the block stays for the next read, or, on a message queue, is
//! dropped.
//!
//! **Flow control.** A writer queues its block whatever the queue holds,
//! then waits while the queue is at its limit. Readers wake waiting
//! writers once it is below half the limit again.
//!
//! **Hanging up.** A queue that is hung up ([`Queue::hangup`]) takes no
//! more writes, which fail with its error, but its readers still read what
//! is queued and then get end of file. Reading past the end of file over
//! and over (more than three times) fails too, as in Plan 9.
//! [`Queue::close`] also throws away what is queued.
//!
//! Readers are served one at a time, as are writers, each under a
//! [`QLock`]; the queue itself is a spin lock held only to look and
//! change, never while copying a block in or out of user memory.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::port::error::{EHUNGUP, Error, Result};
use crate::port::lock::SpinLock;
use crate::port::proc::{Rendez, Sleepable};
use crate::port::qlock::QLock;

/// The most bytes one block holds, so the most a write passes whole.
pub(crate) const MAX_ATOMIC: usize = 64 * 1024;

/// Bytes in a queue: what is left of one write.
#[derive(Debug)]
pub(crate) struct Block {
    data: Vec<u8>,
    /// Where the unread bytes start.
    rp: usize,
}

impl Block {
    pub(crate) fn new(data: Vec<u8>) -> Block {
        Block { data, rp: 0 }
    }

    /// The unread bytes.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.data[self.rp..]
    }

    pub(crate) fn len(&self) -> usize {
        self.data.len() - self.rp
    }
}

/// A queue of blocks.
#[derive(Debug)]
pub(crate) struct Queue {
    st: SpinLock<State>,
    /// The bytes queued at which writers wait.
    limit: usize,
    /// A message queue: a read drops what of its block does not fit.
    msg: bool,
    /// Readers wait here for a block; writers, for room.
    readers: QLock<()>,
    writers: QLock<()>,
    rr: Rendez,
    wr: Rendez,
}

#[derive(Debug)]
struct State {
    blocks: VecDeque<Block>,
    /// The bytes queued.
    len: usize,
    closed: bool,
    /// Why, once closed; `None` for a plain hangup.
    err: Option<Error>,
    /// End-of-file reads since the queue was closed.
    eofs: u32,
    /// A writer waits for room.
    flow: bool,
}

impl Queue {
    pub(crate) const fn new(limit: usize, msg: bool) -> Queue {
        Queue {
            st: SpinLock::new(State {
                blocks: VecDeque::new(),
                len: 0,
                closed: false,
                err: None,
                eofs: 0,
                flow: false,
            }),
            limit,
            msg,
            readers: QLock::new(()),
            writers: QLock::new(()),
            rr: Rendez::new(),
            wr: Rendez::new(),
        }
    }

    /// The bytes queued.
    pub(crate) fn len(&self) -> usize {
        self.st.lock().len
    }

    /// Takes the next block, waiting for one; `None` at end of file.
    pub(crate) fn bread(&self, s: &Sleepable, max: usize) -> Result<Option<Block>> {
        let _r = self.readers.lock(s);
        self.rr.sleep(s, || {
            let st = self.st.lock();
            st.closed || !st.blocks.is_empty()
        });
        let mut st = self.st.lock();
        let Some(mut b) = st.blocks.pop_front() else {
            // Closed and empty.
            st.eofs += 1;
            return match &st.err {
                Some(e) => Err(e.clone()),
                None if st.eofs > 3 => Err(EHUNGUP.into()),
                None => Ok(None),
            };
        };
        st.len -= b.len();
        if b.len() > max {
            if !self.msg {
                let rest = Block::new(b.bytes()[max..].to_vec());
                st.len += rest.len();
                st.blocks.push_front(rest);
            }
            b.data.truncate(b.rp + max);
        }
        let wake = st.flow && st.len < self.limit / 2;
        if wake {
            st.flow = false;
        }
        drop(st);
        if wake {
            self.wr.wakeup_all();
        }
        Ok(Some(b))
    }

    /// Reads into `buf` from the next block, waiting for one; 0 at end of
    /// file.
    pub(crate) fn read(&self, s: &Sleepable, buf: &mut [u8]) -> Result<usize> {
        let Some(b) = self.bread(s, buf.len())? else {
            return Ok(0);
        };
        buf[..b.len()].copy_from_slice(b.bytes());
        Ok(b.len())
    }

    /// Queues `b`, then waits while the queue is full.
    pub(crate) fn bwrite(&self, s: &Sleepable, b: Block) -> Result<usize> {
        let n = b.len();
        let _w = self.writers.lock(s);
        {
            let mut st = self.st.lock();
            if st.closed {
                return Err(st.err.clone().unwrap_or_else(|| EHUNGUP.into()));
            }
            st.len += n;
            st.blocks.push_back(b);
        }
        self.rr.wakeup();
        // Queued already, so a wait cut short loses nothing.
        let room = || {
            let st = self.st.lock();
            st.closed || st.len < self.limit
        };
        while !room() {
            self.st.lock().flow = true;
            self.wr.sleep(s, room);
        }
        Ok(n)
    }

    /// Writes `buf`, a block at a time; a message queue takes only the
    /// first block's worth.
    pub(crate) fn write(&self, s: &Sleepable, buf: &[u8]) -> Result<usize> {
        for chunk in buf.chunks(MAX_ATOMIC) {
            self.bwrite(s, Block::new(chunk.to_vec()))?;
            if self.msg {
                break;
            }
        }
        if buf.is_empty() {
            self.bwrite(s, Block::new(Vec::new()))?;
        }
        Ok(buf.len())
    }

    /// Takes no more writes, failing them with `err` (or "i/o on hungup
    /// channel"); readers read what is left, then get end of file.
    pub(crate) fn hangup(&self, err: Option<Error>) {
        {
            let mut st = self.st.lock();
            st.closed = true;
            st.err = err.filter(|e| e.as_str() != EHUNGUP);
        }
        self.rr.wakeup_all();
        self.wr.wakeup_all();
    }

    /// Hangs up and throws away what is queued.
    pub(crate) fn close(&self) {
        let gone = {
            let mut st = self.st.lock();
            st.closed = true;
            st.err = None;
            st.flow = false;
            st.len = 0;
            core::mem::take(&mut st.blocks)
        };
        drop(gone);
        self.rr.wakeup_all();
        self.wr.wakeup_all();
    }

    /// Takes writes again, after a hangup or close.
    pub(crate) fn reopen(&self) {
        let mut st = self.st.lock();
        st.closed = false;
        st.err = None;
        st.eofs = 0;
    }
}
