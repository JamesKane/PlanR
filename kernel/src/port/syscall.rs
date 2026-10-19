//! System calls (ADR 0002, `lib/abi`): the dispatcher the architecture's
//! entry calls, the calls themselves, and what becomes of a user thread
//! whose code traps.
//!
//! A call runs on the calling thread's kernel stack with interrupts on, so
//! it may sleep. A failure sets the thread's error text, which `errstr`
//! fetches, and returns the error word. Calls that never return (`exits`,
//! `exec`) leave nothing on the stack that needs dropping: the dispatcher
//! holds no reference to the thread while a call runs. A call that blocks
//! may be cut short by a note, and fails with "interrupted".

use alloc::vec::Vec;
use core::fmt;

use abi::{ERRMAX, Failed, RforkFlags, SegAttr, Sys, Ureg};

use crate::port::error::{EBADARG, EINTR, ENOMEM, Error};
use crate::port::note::{self, From};
use crate::port::pgrp::{self, NameSpace};
use crate::port::proc::{self, Rendez, Woke};
use crate::port::segment::{Kind, SegError};
use crate::port::uaccess::{self, BadAddress};
use crate::port::{clock, devrtc, exec, sema, sysfile};

const EBADSYS: &str = "bad system call";
const ENOTYET: &str = "system call not yet implemented";
const ENOCHILD: &str = "no living children";
const ESOVERLAP: &str = "segments overlap";

type SysResult = Result<u64, Error>;

/// Runs system call `number` for the calling thread and returns the
/// register pair its caller sees, or `None` if the call replaced the
/// registers it returns with, `u` (`noted`).
pub(crate) fn dispatch(number: u64, args: [u64; abi::MAX_ARGS], u: &mut Ureg) -> Option<[u64; 2]> {
    let [a0, a1, a2, a3, ..] = args;
    let r = match Sys::from_raw(number) {
        Some(Sys::Noted) => match note::noted(a0, u) {
            Ok(()) => return None,
            Err(e) => Err(e),
        },
        Some(Sys::Open) => sysfile::open(a0, a1),
        Some(Sys::Create) => sysfile::create(a0, a1, a2),
        Some(Sys::Close) => sysfile::close(a0),
        Some(Sys::Read) => sysfile::read(a0, a1, a2, None),
        Some(Sys::Write) => sysfile::write(a0, a1, a2, None),
        Some(Sys::Pread) => sysfile::read(a0, a1, a2, Some(a3)),
        Some(Sys::Pwrite) => sysfile::write(a0, a1, a2, Some(a3)),
        Some(Sys::Seek) => sysfile::seek(a0, a1, a2),
        Some(Sys::Stat) => sysfile::stat(a0, a1, a2),
        Some(Sys::Fstat) => sysfile::fstat(a0, a1, a2),
        Some(Sys::Wstat) => sysfile::wstat(a0, a1, a2),
        Some(Sys::Fwstat) => sysfile::fwstat(a0, a1, a2),
        Some(Sys::Remove) => sysfile::remove(a0),
        Some(Sys::Fd2path) => sysfile::fd2path(a0, a1, a2),
        Some(Sys::Dup) => sysfile::dup(a0, a1),
        Some(Sys::Pipe) => sysfile::pipe(a0),
        Some(Sys::Chdir) => sysfile::chdir(a0),
        Some(Sys::Bind) => sysfile::bind(a0, a1, a2),
        Some(Sys::Mount) => sysfile::mount(a0, a1, a2, a3, args[4]),
        Some(Sys::Unmount) => sysfile::unmount(a0, a1),
        Some(Sys::Fversion) => sysfile::fversion(a0, a1, a2, a3),
        Some(Sys::Fauth) => sysfile::fauth(a0, a1),
        Some(Sys::Rfork) => rfork(a0),
        Some(Sys::Rendezvous) => rendezvous(a0, a1),
        Some(Sys::Errstr) => errstr(args[0], args[1]),
        Some(Sys::Exits) => exits(args[0]),
        Some(Sys::Exec) => exec(args[0], args[1]),
        Some(Sys::Await) => await_(args[0], args[1]),
        Some(Sys::Brk) => brk(args[0]),
        Some(Sys::Settls) => settls(args[0]),
        Some(Sys::Sleep) => sleep(args[0]),
        Some(Sys::Alarm) => Ok(alarm(a0)),
        Some(Sys::Nsec) => Ok(devrtc::nanoseconds()),
        Some(Sys::Notify) => note::set_handler(a0).map(|()| 0),
        Some(Sys::Semacquire) => sema::acquire(a0, a1 != 0, None).map(u64::from),
        Some(Sys::Tsemacquire) => tsemacquire(a0, a1),
        Some(Sys::Semrelease) => sema::release(a0, a1),
        Some(Sys::Segattach) => segattach(a0, a1, a2, a3),
        Some(Sys::Segdetach) => segdetach(a0),
        Some(Sys::Segfree) => segfree(a0, a1),
        Some(Sys::Segflush) => segflush(a0, a1),
        Some(_) => Err(Error::from(ENOTYET)),
        None => Err(Error::from(EBADSYS)),
    };
    Some(abi::encode(r.map_err(|e| {
        let mut e = e.as_str().as_bytes().to_vec();
        e.truncate(ERRMAX - 1);
        *me().errstr().lock() = e;
        Failed
    })))
}

fn me() -> alloc::sync::Arc<proc::Proc> {
    proc::current().expect("a system call comes from a thread")
}

/// `errstr(buf, n)`: swaps the thread's error text with the string in
/// `buf`, both cut to fit `n` bytes with the NUL.
fn errstr(buf: u64, n: u64) -> SysResult {
    let me = me();
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

/// `rfork(flags) -> pid`: a new process, which shares or copies each of
/// the caller's resources as `flags` say and returns 0; without `RFPROC`,
/// changes the caller's own instead.
fn rfork(flags: u64) -> SysResult {
    use RforkFlags as F;
    let flags = F::from_bits(flags).ok_or(EBADARG)?;
    let both = |a: F, b: F| flags.contains(a) && flags.contains(b);
    if both(F::FDG, F::CFDG) || both(F::NAMEG, F::CNAMEG) || both(F::ENVG, F::CENVG) {
        return Err(Error::from(EBADARG));
    }
    if !flags.contains(F::PROC) {
        if flags.contains(F::MEM) || flags.contains(F::NOWAIT) {
            return Err(Error::from(EBADARG));
        }
        let old = pgrp::with_files(|f| {
            let new = f.fork(flags);
            core::mem::replace(f, new)
        })?;
        drop(old);
        if flags.contains(F::NOTEG) {
            me().new_noteg();
        }
        return Ok(0);
    }
    let mem = me().memory().ok_or(EBADARG)?;
    let mem = alloc::sync::Arc::new(mem.fork(flags.contains(F::MEM)).ok_or(ENOMEM)?);
    let files = pgrp::with_files(|f| f.fork(flags))?;
    let child = proc::fork(mem, files, !flags.contains(F::NOWAIT), flags.contains(F::NOTEG)).ok_or(ENOMEM)?;
    Ok(child.pid())
}

/// `rendezvous(tag, value) -> value`: waits for another process of the
/// rendezvous group to call with `tag`, and returns its `value`.
fn rendezvous(tag: u64, value: u64) -> SysResult {
    let rgrp = pgrp::rgrp()?;
    let s = proc::sleepable().expect("a system call may sleep");
    rgrp.rendezvous(&s, tag, value)
}

/// `exec(name, argv)`: replaces the thread's program, closing the
/// descriptors marked close-on-exec.
fn exec(name: u64, argv: u64) -> SysResult {
    let name = sysfile::name_arg(name)?;
    let argv = read_argv(argv)?;
    let l = exec::load(&NameSpace::current()?, &name, &argv)?;
    if let Some(old) = me().memory() {
        l.mem.inherit(&old).map_err(|_| ESOVERLAP)?;
    }
    me().set_args(&argv);
    me().notes().lock().exec();
    drop((name, argv));
    drop(pgrp::fgrp()?.close_on_exec());
    proc::exec(l.mem, l.name, l.pc, l.sp)
}

/// The strings `argv` points at, up to its null pointer, within
/// [`exec::ARGS_MAX`]. A null `argv` is no arguments.
fn read_argv(argv: u64) -> Result<Vec<Vec<u8>>, Error> {
    let mut strings = Vec::new();
    // Each takes its NUL and its pointer; the null pointer ends the list.
    let mut room = exec::ARGS_MAX - 8;
    if argv == 0 {
        return Ok(strings);
    }
    for i in 0.. {
        let mut w = [0u8; 8];
        uaccess::copy_in(&mut w, argv.checked_add(i * 8).ok_or(BadAddress)?)?;
        let p = u64::from_ne_bytes(w);
        if p == 0 {
            break;
        }
        let a = uaccess::copy_in_str(p, room.saturating_sub(8))?;
        if a.len() + 9 > room {
            return Err(Error::from(exec::E2BIG));
        }
        room -= a.len() + 9;
        strings.push(a);
    }
    Ok(strings)
}

/// `await(buf, n)`: waits for a child to exit and returns its wait
/// message, cut to `n` bytes.
fn await_(buf: u64, n: u64) -> SysResult {
    let n = usize::try_from(n).map_err(|_| EBADARG)?.min(4 * ERRMAX);
    uaccess::check(buf, n)?;
    let s = proc::sleepable().expect("a system call may sleep");
    let w = proc::await_child(&s)?.ok_or(ENOCHILD)?;
    let mut text = alloc::vec![0u8; n];
    let len = abi::format_waitmsg(w.pid, w.times, &w.msg, &mut text);
    uaccess::copy_out(buf, &text[..len])?;
    Ok(len as u64)
}

/// `brk(addr)`: moves the end of the bss segment.
fn brk(addr: u64) -> SysResult {
    let mem = me().memory().ok_or(EBADARG)?;
    match mem.brk(addr) {
        Ok(()) => Ok(0),
        Err(SegError::BadAddress) => Err(Error::from(EBADARG)),
        Err(SegError::Overlap) => Err(Error::from(ESOVERLAP)),
    }
}

/// `settls(addr)`: sets the thread pointer.
fn settls(addr: u64) -> SysResult {
    uaccess::check(addr, 0).map_err(|_| EBADARG)?;
    proc::set_tls(addr);
    Ok(0)
}

/// The counter time `ms` milliseconds from now.
fn after_ms(ms: u64) -> u64 {
    clock::now_ns().saturating_add(ms.saturating_mul(1_000_000))
}

/// `sleep(ms)`: gives up the processor for at least `ms` milliseconds;
/// `sleep(0)` just yields.
fn sleep(ms: u64) -> SysResult {
    if ms == 0 {
        proc::yield_now();
        return Ok(0);
    }
    let s = proc::sleepable().expect("a system call may sleep");
    match Rendez::new().tsleep(&s, || false, after_ms(ms), true) {
        Woke::Interrupted => Err(Error::from(EINTR)),
        _ => Ok(0),
    }
}

/// `alarm(ms)`: posts the note `alarm` in `ms` milliseconds (none if 0),
/// in place of any alarm set before; returns the milliseconds that one
/// had left.
fn alarm(ms: u64) -> u64 {
    let now = clock::now_ns();
    let old = me().set_alarm(if ms == 0 { 0 } else { after_ms(ms) });
    old.saturating_sub(now).div_ceil(1_000_000)
}

/// `tsemacquire(addr, ms)`: `semacquire` waiting at most `ms`.
fn tsemacquire(addr: u64, ms: u64) -> SysResult {
    sema::acquire(addr, ms > 0, Some(after_ms(ms))).map(u64::from)
}

fn seg_error(e: SegError) -> Error {
    match e {
        SegError::BadAddress => Error::from(EBADARG),
        SegError::Overlap => Error::from(ESOVERLAP),
    }
}

/// `segattach(attr, class, va, len) -> va`.
fn segattach(attr: u64, class: u64, va: u64, len: u64) -> SysResult {
    let attr = SegAttr::from_bits(attr).ok_or(EBADARG)?;
    let kind = match sysfile::name_arg(class)?.as_str() {
        "shared" => Kind::Shared,
        "memory" => Kind::Memory,
        _ => return Err(Error::from("bad segment type")),
    };
    let mem = me().memory().ok_or(EBADARG)?;
    mem.attach(kind, attr, va, len).map_err(seg_error)
}

/// `segdetach(va)`.
fn segdetach(va: u64) -> SysResult {
    let mem = me().memory().ok_or(EBADARG)?;
    mem.detach(va).map(|()| 0).map_err(seg_error)
}

/// `segfree(va, len)`.
fn segfree(va: u64, len: u64) -> SysResult {
    let mem = me().memory().ok_or(EBADARG)?;
    mem.free(va, len).map(|()| 0).map_err(seg_error)
}

/// `segflush(va, len)`: nothing to do once the range is checked, since
/// only text is executable and the kernel fills it coherently.
fn segflush(va: u64, len: u64) -> SysResult {
    let mem = me().memory().ok_or(EBADARG)?;
    if !mem.covers(va, len) {
        return Err(Error::from(EBADARG));
    }
    Ok(0)
}

/// Called when user code traps for any reason but a system call or an
/// interrupt: posts the note that says so, which ends the thread unless
/// its handler takes it. Interrupts are off.
pub(crate) fn user_trap(what: &dyn fmt::Display, pc: u64) {
    let msg = alloc::format!("sys: trap: {what} pc={pc:#x}");
    if let Some(me) = proc::current() {
        note::post(&me, msg.as_bytes(), From::Kernel);
    }
}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use alloc::sync::Arc;

    use crate::arch;
    use crate::port::clock;
    use crate::port::cpu;
    use crate::port::ktest::TestResult;
    use crate::port::mm::PAGE;
    use crate::port::pgrp::Files;
    use crate::port::proc::{PRI_NORMAL, Proc, Spawn, State, spawn_user};
    use crate::port::segment::{Image, Kind, Memory, Segment};

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

    /// A user thread running the test program, and its memory, kept to
    /// look at once the thread is dead.
    struct Run {
        p: Arc<Proc>,
        mem: Arc<Memory>,
    }

    impl Run {
        fn word(&self, i: usize) -> u64 {
            let mut w = [0u8; 8];
            self.mem
                .read(DATA + i as u64 * 8, &mut w)
                .expect("the data page is there");
            u64::from_ne_bytes(w)
        }

        fn buf(&self) -> alloc::vec::Vec<u8> {
            let mut b = alloc::vec![0u8; 128];
            self.mem
                .read(DATA + BUF as u64, &mut b)
                .expect("the data page is there");
            b.truncate(b.iter().position(|&c| c == 0).unwrap_or(b.len()));
            b
        }
    }

    /// Starts the test program in memory of its own, on this CPU: one page
    /// each of text, data and stack, with the stack pointer `below` bytes
    /// under the top.
    fn start(tag: u64, mode: u64, below: u64) -> Result<Run, &'static str> {
        let program = arch::context::ktests::user_test();
        let mem = Memory::new().ok_or("no memory for a space")?;
        for (kind, at) in [(Kind::Text, TEXT), (Kind::Data, DATA), (Kind::Stack, STACK)] {
            mem.add(Segment::new(kind, at, at + PAGE, Image::NONE, 0))
                .map_err(|_| "segments overlap")?;
        }
        let (tag, mode) = (tag.to_ne_bytes(), mode.to_ne_bytes());
        let fill: [(u64, &[u8]); 4] = [
            (TEXT, program),
            (DATA + TAG as u64 * 8, &tag),
            (DATA + MODE as u64 * 8, &mode),
            (DATA + BUF as u64, FROM_USER),
        ];
        for (at, bytes) in fill {
            mem.write(at, bytes).map_err(|_| "cannot fill the program's memory")?;
        }
        let mem = Arc::new(mem);
        let how = Spawn {
            name: "utest",
            pri: PRI_NORMAL,
            wired: Some(cpu::current().index()),
        };
        let files = Files::boot().map_err(|_| "no name space")?;
        let p = spawn_user(how, mem.clone(), files, TEXT, STACK + PAGE - below, DATA).ok_or("cannot spawn")?;
        Ok(Run { p, mem })
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
        // Different stack pointers, so a thread that resumes with the
        // other's shows it.
        let runs = (start(0x1111_2222_3333_4444, 0, 0), start(0x5555_6666_7777_8888, 0, 64));
        arch::splx(pl);
        let (a, b) = (runs.0?, runs.1?);
        wait_dead(&[&a, &b])?;
        for r in [&a, &b] {
            match r.word(STATUS) {
                1 => {}
                2 => return Err("FP/SIMD state or thread pointer lost across a switch".into()),
                3 => return Err("a register (perhaps the stack pointer) lost across a system call".into()),
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
        let r = start(1, 1, 0)?;
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
