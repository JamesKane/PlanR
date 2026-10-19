//! Loading programs: a static ELF file becomes a fresh [`Memory`] with
//! text, data, bss and stack segments, and its arguments on the stack
//! (ADR 0002's start-up layout). Nothing is copied but the arguments: the
//! segments' pages fill from the file as they are touched. A file in the
//! boot archive pages straight from where 9boot left it; any other is read
//! into memory first.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::{Access, OpenFlags, OpenMode};
use elf::{Elf, Machine};

use crate::port::chan::{self, Chan, last_elem};
use crate::port::error::{ENOMEM, Error, Result};
use crate::port::mm::PAGE;
use crate::port::pgrp::{Files, NameSpace};
use crate::port::proc::{self, Proc, Spawn};
use crate::port::segment::{Image, Kind, Memory, Segment};
use crate::port::sysfile::IO_MAX;
use crate::port::uaccess::USER_TOP;

#[cfg(target_arch = "x86_64")]
const MACHINE: Machine = Machine::X86_64;
#[cfg(target_arch = "aarch64")]
const MACHINE: Machine = Machine::Aarch64;

/// The stack segment: the top of the user half, this big.
pub(crate) const STACK_SIZE: u64 = 16 << 20;
const STACK_BASE: u64 = USER_TOP - STACK_SIZE;
/// Nothing loads below this, so null pointers and small offsets from them
/// always fault.
const LOWEST: u64 = 0x1_0000;
/// The most argument bytes (strings, NULs and pointers) exec takes.
pub(crate) const ARGS_MAX: usize = 64 * 1024;
/// The largest program that is read into memory to run.
const FILE_MAX: usize = 64 << 20;

pub(crate) const EBADEXEC: &str = "exec header invalid";
pub(crate) const E2BIG: &str = "argument list too long";

/// A program ready to run.
#[derive(Debug)]
pub(crate) struct Loaded {
    pub(crate) mem: Arc<Memory>,
    /// The last element of its name, for the process's name.
    pub(crate) name: String,
    pub(crate) pc: u64,
    pub(crate) sp: u64,
}

fn page_down(a: u64) -> u64 {
    a & !(PAGE - 1)
}

fn page_up(a: u64) -> Option<u64> {
    a.checked_next_multiple_of(PAGE)
}

/// The contents of `c`, opened for exec: in place if they lie in memory
/// for good, else read in.
fn image_of(c: &Chan) -> Result<Image> {
    if let Some(b) = c.dev.mapped(c) {
        return Ok(Image::mapped(b));
    }
    let mut data = Vec::new();
    loop {
        let at = data.len();
        if at >= FILE_MAX {
            return Err("exec file too large".into());
        }
        data.resize(at + IO_MAX, 0);
        let n = c.read(&mut data[at..], at as u64)?;
        data.truncate(at + n);
        if n == 0 {
            return Ok(Image::owned(data.into()));
        }
    }
}

/// Loads the program `name`, resolved in `ns`, with arguments `argv`.
pub(crate) fn load(ns: &NameSpace, name: &str, argv: &[Vec<u8>]) -> Result<Loaded> {
    let exec = OpenMode {
        access: Access::Exec,
        flags: OpenFlags::empty(),
    };
    let c = chan::open(&ns.names(), name, exec)?;
    if c.qid.is_dir() {
        return Err(EBADEXEC.into());
    }
    let image = image_of(&c)?;
    let prog_name = String::from(last_elem(&c.name));
    drop(c);
    let elf = Elf::parse(image.bytes(), MACHINE).map_err(|_| EBADEXEC)?;
    let mem = Memory::new().ok_or(ENOMEM)?;
    let mut writable = 0;
    for s in elf.segments() {
        let base = page_down(s.vaddr);
        let file_top = page_up(s.vaddr + s.filesz).ok_or(EBADEXEC)?;
        let top = page_up(s.vaddr + s.memsz).ok_or(EBADEXEC)?;
        if base < LOWEST || top > STACK_BASE || s.perms.write && s.perms.exec {
            return Err(EBADEXEC.into());
        }
        #[allow(clippy::cast_possible_truncation)] // checked against the file in `parse`
        let file = image.slice(s.offset as usize, s.filesz as usize);
        let add = |kind, base, top| {
            mem.add(Segment::new(kind, base, top, file.clone(), s.vaddr))
                .map_err(|_| Error::from(EBADEXEC))
        };
        if s.perms.write {
            // Plan 9's split: file pages in data, the rest in the bss,
            // which brk moves. One of each.
            writable += 1;
            add(Kind::Data, base, file_top)?;
            add(Kind::Bss, file_top, top)?;
        } else {
            add(Kind::Text, base, top)?;
        }
    }
    if writable > 1 {
        return Err(EBADEXEC.into());
    }
    if writable == 0 {
        // An empty bss after the highest segment, for brk.
        let end = mem.segment_end();
        mem.add(Segment::new(Kind::Bss, end, end, Image::NONE, 0))
            .map_err(|_| EBADEXEC)?;
    }
    if mem.kind_at(elf.entry()) != Some(Kind::Text) {
        return Err(EBADEXEC.into());
    }
    mem.add(Segment::new(Kind::Stack, STACK_BASE, USER_TOP, Image::NONE, 0))
        .map_err(|_| EBADEXEC)?;
    let sp = push_args(&mem, argv)?;
    Ok(Loaded {
        mem: Arc::new(mem),
        name: prog_name,
        pc: elf.entry(),
        sp,
    })
}

/// Writes `argv` at the top of the stack as ADR 0002 lays it out, and
/// returns the stack pointer.
fn push_args(mem: &Memory, argv: &[Vec<u8>]) -> Result<u64> {
    let strings: usize = argv.iter().map(|a| a.len() + 1).sum();
    let pointers = (argv.len() + 2) * 8;
    if strings + pointers + 16 > ARGS_MAX {
        return Err(E2BIG.into());
    }
    let strings_at = USER_TOP - strings as u64;
    let sp = (strings_at - pointers as u64) & !15;
    #[allow(clippy::cast_possible_truncation)] // under ARGS_MAX
    let mut block = alloc::vec![0u8; (USER_TOP - sp) as usize];
    block[..8].copy_from_slice(&(argv.len() as u64).to_le_bytes());
    let mut at = strings_at;
    #[allow(clippy::cast_possible_truncation)] // as above
    for (i, a) in argv.iter().enumerate() {
        block[8 + i * 8..16 + i * 8].copy_from_slice(&at.to_le_bytes());
        let o = (at - sp) as usize;
        block[o..o + a.len()].copy_from_slice(a);
        at += a.len() as u64 + 1;
    }
    mem.write(sp, &block).map_err(|_| ENOMEM)?;
    Ok(sp)
}

/// Starts the program `name` as a new process, a child of the calling
/// thread, in a fresh name space ([`Files::boot`]).
pub(crate) fn spawn(name: &str, argv: &[&str], pri: usize) -> Result<Arc<Proc>> {
    let argv: Vec<Vec<u8>> = argv.iter().map(|a| a.as_bytes().to_vec()).collect();
    let files = Files::boot()?;
    let l = load(&NameSpace::of(&files), name, &argv)?;
    let how = Spawn {
        name: &l.name,
        pri,
        wired: None,
    };
    proc::spawn_user(how, l.mem, files, l.pc, l.sp, l.sp).ok_or_else(|| ENOMEM.into())
}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use alloc::string::String;
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicBool, Ordering};

    use crate::port::clock;
    use crate::port::ktest::{Fail, TestResult};
    use crate::port::lock::SpinLock;
    use crate::port::mm;
    use crate::port::proc::{self, PRI_NORMAL, Spawn, Wait};

    const SECOND: u64 = 1_000_000_000;

    /// What a program should exit with: the status after its name and pid,
    /// exactly or as a prefix.
    #[derive(Clone, Copy)]
    enum Want {
        Normal,
        Status(&'static str),
        Trap,
    }

    const RUNS: &[(&str, &[&str], Want)] = &[
        ("/boot/tmem", &["tmem"], Want::Normal),
        ("/boot/targs", &["targs", "one", "two"], Want::Status("targs one two")),
        ("/boot/texec", &["texec"], Want::Status("targs from exec")),
        ("/boot/tfault", &["tfault", "null"], Want::Trap),
        ("/boot/tfault", &["tfault", "text"], Want::Trap),
        ("/boot/tfault", &["tfault", "brk"], Want::Trap),
        ("/boot/tfault", &["tfault", "stack"], Want::Trap),
        ("/boot/tfile", &["tfile"], Want::Status("targs from env")),
        ("/boot/tproc", &["tproc"], Want::Normal),
    ];

    /// Each child's pid and its wait message, as `await` formats it.
    type Waits = Vec<(u64, Vec<u8>)>;

    /// Runs every program in [`RUNS`] at once as children of a kernel
    /// thread, which awaits them all, then checks there are no more.
    /// Returns the wait messages, as `await` formats them, by pid, and the
    /// pids started.
    fn run_all() -> Result<(Waits, Vec<u64>), Fail> {
        type Out = (Waits, Vec<u64>, Result<(), &'static str>);
        let out: Arc<SpinLock<Option<Out>>> = Arc::new(SpinLock::new(None));
        let done = Arc::new(AtomicBool::new(false));
        let (o, d) = (out.clone(), done.clone());
        proc::spawn(Spawn::kproc("execpar"), move || {
            let mut pids = Vec::new();
            let mut r = Ok(());
            for &(path, argv, _) in RUNS {
                match super::spawn(path, argv, PRI_NORMAL) {
                    Ok(p) => pids.push(p.pid()),
                    Err(e) => {
                        crate::println!("ktest: cannot run {path}: {e}");
                        r = Err("a program would not start");
                    }
                }
            }
            let sl = proc::sleepable().expect("a thread may sleep");
            let mut waits = Vec::new();
            for _ in 0..pids.len() {
                match proc::await_child(&sl) {
                    Some(w) => waits.push((w.pid, wait_text(&w))),
                    None => r = Err("await found no child"),
                }
            }
            if proc::await_child(&sl).is_some() {
                r = Err("await found a child too many");
            }
            *o.lock() = Some((waits, pids, r));
            d.store(true, Ordering::Release);
        })
        .ok_or("cannot spawn")?;
        if !clock::wait_until(|| done.load(Ordering::Acquire), 20 * SECOND) {
            return Err("programs did not finish within 20 s".into());
        }
        let (waits, pids, r) = out.lock().take().ok_or("no result")?;
        r?;
        Ok((waits, pids))
    }

    fn wait_text(w: &Wait) -> Vec<u8> {
        let mut buf = alloc::vec![0u8; 256];
        let n = abi::format_waitmsg(w.pid, w.times, &w.msg, &mut buf);
        buf.truncate(n);
        buf
    }

    /// Programs from the boot archive run, exit and are awaited: data,
    /// bss, stack and brk pages appear on first touch, arguments arrive,
    /// exec replaces a program, faults end one with a trap status, the
    /// file calls work on every device (`tfile`), and processes fork,
    /// share or copy their memory and groups, and talk through pipes and
    /// `#s` (`tproc`).
    pub(crate) fn programs_run() -> TestResult {
        if !crate::port::devroot::has("boot/tmem") {
            return Err("no boot archive with the test programs".into());
        }
        let (waits, pids) = run_all()?;
        for (i, &(_, _, want)) in RUNS.iter().enumerate() {
            let pid = pids[i];
            let text = &waits
                .iter()
                .find(|w| w.0 == pid)
                .ok_or("a child's exit went unreported")?
                .1;
            let mut msg = [0u8; 256];
            let (wpid, _, n) = abi::parse_waitmsg(text, &mut msg).ok_or("bad wait message")?;
            let msg = &msg[..n];
            if wpid != pid {
                return Err("wait message has the wrong pid".into());
            }
            // "name pid: status"
            let status = msg
                .iter()
                .position(|&b| b == b':')
                .map_or(&b""[..], |c| msg.get(c + 2..).unwrap_or(b""));
            let prefix = alloc::format!(" {pid}: ");
            let named = msg.is_empty() || msg.windows(prefix.len()).any(|w| w == prefix.as_bytes());
            let ok = named
                && match want {
                    Want::Normal => msg.is_empty(),
                    Want::Status(s) => status == s.as_bytes(),
                    Want::Trap => status.starts_with(b"sys: trap: "),
                };
            if !ok {
                crate::println!(
                    "ktest: {} exited with {:?}",
                    RUNS[i].1.join(" "),
                    String::from_utf8_lossy(msg)
                );
                return Err("a program exited with the wrong status".into());
            }
        }
        Ok(())
    }

    /// Running the programs again gives back every frame they took: their
    /// segments' frames and page tables all go back. Other CPUs free what
    /// their dead processes ran in a moment after the parent hears, so the
    /// count is given a little while to come back.
    pub(crate) fn memory_freed() -> TestResult {
        // Once first, so the heap has grown to what a run needs.
        run_all()?;
        let before = mm::frame_counts().0;
        run_all()?;
        if !clock::wait_until(|| mm::frame_counts().0 >= before, 2 * SECOND) {
            let after = mm::frame_counts().0;
            crate::println!("ktest: {before} free frames before, {after} after");
            return Err("programs' memory not freed".into());
        }
        Ok(())
    }
}
