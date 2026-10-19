//! System calls (ADR 0002, `lib/abi`): the dispatcher the architecture's
//! entry calls, the calls themselves, and what becomes of a user thread
//! whose code traps.
//!
//! A call runs on the calling thread's kernel stack with interrupts on, so
//! it may sleep. A failure sets the thread's error text, which `errstr`
//! fetches, and returns the error word.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use abi::{ERRMAX, Failed, Sys};

use crate::port::proc::{self, Proc};
use crate::port::uaccess::{self, BadAddress};
use crate::port::{clock, mm};
use crate::println;

/// Why a call failed: its error text.
#[derive(Debug)]
struct Error(String);

impl From<&str> for Error {
    fn from(s: &str) -> Error {
        Error(String::from(s))
    }
}

impl From<BadAddress> for Error {
    fn from(_: BadAddress) -> Error {
        Error::from(EBADADDR)
    }
}

const EBADSYS: &str = "bad system call";
const EBADARG: &str = "bad arg in system call";
const EBADADDR: &str = "bad address in system call";
const ENOTYET: &str = "system call not yet implemented";

type SysResult = Result<u64, Error>;

/// Runs system call `number` for the calling thread and returns the
/// register pair its caller sees.
pub(crate) fn dispatch(number: u64, args: [u64; abi::MAX_ARGS]) -> [u64; 2] {
    let me = proc::current().expect("a system call comes from a thread");
    let r = match Sys::from_raw(number) {
        Some(Sys::Errstr) => errstr(&me, args[0], args[1]),
        Some(Sys::Exits) => exits(args[0]),
        Some(Sys::Settls) => settls(args[0]),
        Some(Sys::Sleep) => sleep(args[0]),
        Some(_) => Err(Error::from(ENOTYET)),
        None => Err(Error::from(EBADSYS)),
    };
    abi::encode(r.map_err(|Error(text)| {
        let mut e = text.into_bytes();
        e.truncate(ERRMAX - 1);
        *me.errstr().lock() = e;
        Failed
    }))
}

/// `errstr(buf, n)`: swaps the thread's error text with the string in
/// `buf`, both cut to fit `n` bytes with the NUL.
fn errstr(me: &Proc, buf: u64, n: u64) -> SysResult {
    let n = usize::try_from(n).map_err(|_| EBADARG)?.min(ERRMAX);
    if n == 0 {
        return Err(Error::from(EBADARG));
    }
    let new = uaccess::copy_in_str(buf, n - 1)?;
    let mut old = core::mem::replace(&mut *me.errstr().lock(), new);
    old.truncate(n - 1);
    old.push(0);
    if let Err(e) = uaccess::copy_out(buf, &old) {
        // Leave the text as it was: the caller never saw it.
        old.pop();
        *me.errstr().lock() = old;
        return Err(e.into());
    }
    Ok(0)
}

/// `exits(msg)`: ends the thread with `msg` as its status; a null `msg`
/// (or an empty one) is a normal exit.
fn exits(msg: u64) -> SysResult {
    let status = if msg == 0 {
        Vec::new()
    } else {
        uaccess::copy_in_str(msg, ERRMAX - 1).unwrap_or_else(|_| b"exits: bad address".to_vec())
    };
    proc::exits(status)
}

/// `settls(addr)`: sets the thread pointer.
fn settls(addr: u64) -> SysResult {
    uaccess::check(addr, 0).map_err(|_| EBADARG)?;
    proc::set_tls(addr);
    Ok(0)
}

/// `sleep(ms)`: gives up the processor for at least `ms` milliseconds;
/// `sleep(0)` just yields. Until timed sleep arrives with `alarm`, a
/// longer sleep yields repeatedly until the time has passed.
fn sleep(ms: u64) -> SysResult {
    let end = clock::now_ns().saturating_add(ms.saturating_mul(1_000_000));
    loop {
        proc::yield_now();
        if clock::now_ns() >= end {
            return Ok(0);
        }
        crate::arch::idle();
    }
}

/// Called when user code traps for any reason but a system call or an
/// interrupt: ends the thread, as Plan 9 would once the note went
/// unhandled. Interrupts are off.
pub(crate) fn user_trap(what: &dyn fmt::Display, pc: u64) -> ! {
    let msg = alloc::format!("sys: trap: {what} pc={pc:#x}");
    if let Some(me) = proc::current() {
        println!("{} {}: {msg}", me.pid(), me.name());
    }
    let mut status = msg.into_bytes();
    status.truncate(ERRMAX - 1);
    proc::exits(status)
}

/// A fresh user address space for tests, with the pages it maps.
#[derive(Debug)]
pub(crate) struct TestSpace {
    pub(crate) space: alloc::sync::Arc<crate::port::mmu::Space>,
    pages: Vec<(u64, u64)>,
}

impl TestSpace {
    pub(crate) fn new() -> Option<TestSpace> {
        Some(TestSpace {
            space: alloc::sync::Arc::new(crate::port::mmu::Space::new()?),
            pages: Vec::new(),
        })
    }

    /// Maps a new zeroed page at `va` and returns its frame.
    pub(crate) fn page(&mut self, va: u64, kind: mm::MapKind) -> Option<u64> {
        let frame = mm::alloc_frame()?;
        if self.space.map(va, frame, kind).is_err() {
            // SAFETY: never mapped.
            unsafe { mm::free_frames(frame, 0) };
            return None;
        }
        self.pages.push((va, frame));
        Some(frame)
    }
}

impl Drop for TestSpace {
    fn drop(&mut self) {
        for &(va, frame) in &self.pages {
            if self.space.unmap(va) == Some(frame) {
                // SAFETY: unmapped and flushed from every CPU.
                unsafe { mm::free_frames(frame, 0) };
            }
        }
    }
}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use alloc::sync::Arc;

    use bootinfo::phys_to_virt;

    use super::TestSpace;
    use crate::arch;
    use crate::port::clock;
    use crate::port::cpu;
    use crate::port::ktest::TestResult;
    use crate::port::mm::{MapKind, PAGE};
    use crate::port::proc::{PRI_NORMAL, Proc, Spawn, State, spawn_user};

    const SECOND: u64 = 1_000_000_000;
    const TEXT: u64 = 0x0000_0000_0040_0000;
    const DATA: u64 = 0x0000_0000_1000_0000;
    const STACK: u64 = 0x0000_0000_7fff_0000;

    /// The test program's data page, as words (see
    /// `arch::context::ktests::USER_TEST`).
    const TAG: usize = 0;
    const MODE: usize = 1;
    const SETTLS_ERR: usize = 2;
    const STATUS: usize = 3;
    const ERRSTR_VALUE: usize = 4;
    const ERRSTR_ERR: usize = 5;
    const BADSYS_ERR: usize = 6;
    /// The byte offset of the `errstr` buffer.
    const BUF: usize = 0x100;
    const FROM_USER: &[u8] = b"from user\0";

    /// A user thread running the test program, and its data page.
    struct Run {
        p: Arc<Proc>,
        data: *mut u64,
        _space: TestSpace,
    }

    impl Run {
        fn word(&self, i: usize) -> u64 {
            // SAFETY: the data page is ours, in the direct map, and its
            // thread is dead.
            unsafe { self.data.add(i).read_volatile() }
        }

        fn buf(&self) -> &[u8] {
            // SAFETY: as above.
            let b = unsafe { core::slice::from_raw_parts(self.data.cast::<u8>().add(BUF), 128) };
            &b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]
        }
    }

    /// Starts the test program in a space of its own, on this CPU.
    fn start(tag: u64, mode: u64) -> Result<Run, &'static str> {
        let program = arch::context::ktests::user_test();
        let mut s = TestSpace::new().ok_or("no memory for a space")?;
        let text = s.page(TEXT, MapKind::UserText).ok_or("cannot map text")?;
        let data = s.page(DATA, MapKind::UserReadWrite).ok_or("cannot map data")?;
        s.page(STACK, MapKind::UserReadWrite).ok_or("cannot map a stack")?;
        let text_va = phys_to_virt(text);
        let data = phys_to_virt(data) as *mut u64;
        // SAFETY: both frames are new, ours, and in the direct map.
        unsafe {
            core::ptr::copy_nonoverlapping(program.as_ptr(), text_va as *mut u8, program.len());
            data.add(TAG).write(tag);
            data.add(MODE).write(mode);
            core::ptr::copy_nonoverlapping(FROM_USER.as_ptr(), data.cast::<u8>().add(BUF), FROM_USER.len());
        }
        arch::mmu::sync_icache(text_va, program.len() as u64);
        let how = Spawn {
            name: "utest",
            pri: PRI_NORMAL,
            wired: Some(cpu::current().index()),
        };
        let p = spawn_user(how, s.space.clone(), TEXT, STACK + PAGE, DATA).ok_or("cannot spawn")?;
        Ok(Run { p, data, _space: s })
    }

    fn wait_dead(runs: &[&Run]) -> TestResult {
        if clock::wait_until(|| runs.iter().all(|r| r.p.state() == State::Dead), 5 * SECOND) {
            Ok(())
        } else {
            Err("user threads did not exit within 5 s".into())
        }
    }

    /// Two user threads on one CPU make system calls, yielding to each
    /// other: each keeps its own registers, FP/SIMD state and thread
    /// pointer; `errstr` swaps text both ways; an unknown call fails; and
    /// `exits` ends them.
    pub(crate) fn user_threads_call() -> TestResult {
        let pl = arch::splhi();
        let runs = (start(0x1111_2222_3333_4444, 0), start(0x5555_6666_7777_8888, 0));
        arch::splx(pl);
        let (a, b) = (runs.0?, runs.1?);
        wait_dead(&[&a, &b])?;
        for r in [&a, &b] {
            match r.word(STATUS) {
                1 => {}
                2 => return Err("FP/SIMD state or thread pointer lost across a switch".into()),
                3 => return Err("a register lost across a system call".into()),
                _ => return Err("the program did not finish".into()),
            }
            if r.word(SETTLS_ERR) != 0 {
                return Err("settls failed".into());
            }
            if r.word(BADSYS_ERR) == 0 {
                return Err("an unknown call succeeded".into());
            }
            if r.word(ERRSTR_ERR) != 0 || r.word(ERRSTR_VALUE) != 0 {
                return Err("errstr failed".into());
            }
            if r.buf() != super::EBADSYS.as_bytes() {
                return Err("errstr did not return the last error".into());
            }
            if r.p.errstr().lock().as_slice() != &FROM_USER[..FROM_USER.len() - 1] {
                return Err("errstr did not take the user's text".into());
            }
            if r.p.status().is_none_or(|s| !s.is_empty()) {
                return Err("wrong exit status".into());
            }
        }
        Ok(())
    }

    /// A user thread that reads address 0 is ended, with a status saying
    /// why, and the kernel carries on.
    pub(crate) fn user_fault_kills() -> TestResult {
        let r = start(1, 1)?;
        wait_dead(&[&r])?;
        let status = r.p.status().ok_or("no exit status")?;
        if !status.starts_with(b"sys: trap: ") {
            return Err("the status does not name the trap".into());
        }
        if r.word(STATUS) != 0 {
            return Err("the program ran on past the fault".into());
        }
        Ok(())
    }
}
