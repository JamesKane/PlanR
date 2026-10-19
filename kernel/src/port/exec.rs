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
use crate::port::clock;
use crate::port::error::{ENOMEM, Error, Result};
use crate::port::lock::SpinLock;
use crate::port::mm::PAGE;
use crate::port::pgrp::{Files, NameSpace};
use crate::port::proc::{self, PRI_NORMAL, Proc, Spawn};
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
    let p = proc::spawn_user(how, l.mem, files, l.pc, l.sp, l.sp).ok_or(ENOMEM)?;
    p.set_args(&argv);
    Ok(p)
}

/// How the boot process exited, once it has: its wait message, empty for
/// a normal exit.
pub(crate) type InitExit = Arc<SpinLock<Option<Vec<u8>>>>;

/// Starts `/boot/init` with `argv`, the first user process, as the child
/// of a kernel thread that awaits it, prints how it exited and fills in
/// the result.
pub(crate) fn start_init(argv: &'static [&'static str]) -> Result<InitExit> {
    let exit: InitExit = Arc::new(SpinLock::new(None));
    let e = exit.clone();
    let start: Arc<SpinLock<Option<Result<()>>>> = Arc::new(SpinLock::new(None));
    let started = start.clone();
    proc::spawn(Spawn::kproc("boot"), move || {
        let r = spawn("/boot/init", argv, PRI_NORMAL).map(|_| ());
        let ok = r.is_ok();
        *started.lock() = Some(r);
        if !ok {
            return;
        }
        let sl = proc::sleepable().expect("a thread may sleep");
        let msg = match proc::await_child(&sl) {
            Ok(Some(w)) => w.msg,
            _ => b"init: await found no child".to_vec(),
        };
        if msg.is_empty() {
            crate::println!("kernel: init exited normally");
        } else {
            crate::println!("kernel: init exited: {}", String::from_utf8_lossy(&msg));
        }
        *e.lock() = Some(msg);
    })
    .ok_or(ENOMEM)?;
    // Loading takes no time to speak of: it maps the file, it doesn't read it.
    clock::wait_until(|| start.lock().is_some(), 5 * SECOND);
    let r = start.lock().take();
    match r {
        Some(Ok(())) => Ok(exit),
        Some(Err(e)) => Err(e),
        None => Err(Error::from("init did not start")),
    }
}

const SECOND: u64 = 1_000_000_000;

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use crate::port::clock;
    use crate::port::ktest::{Fail, TestResult};
    use crate::port::mm;

    use super::SECOND;

    /// Runs `/boot/init -q`, which runs every test program; it must exit
    /// normally within 60 s.
    fn run_init() -> Result<(), Fail> {
        let exit = super::start_init(&["init", "-q"]).map_err(|_| "init would not start")?;
        if !clock::wait_until(|| exit.lock().is_some(), 60 * SECOND) {
            return Err("init did not finish within 60 s".into());
        }
        let msg = exit.lock().take().unwrap_or_default();
        if !msg.is_empty() {
            return Err("init exited with an error".into());
        }
        Ok(())
    }

    /// Running the programs again gives back every frame they took: their
    /// segments' frames and page tables all go back. Other CPUs free what
    /// their dead processes ran in a moment after the parent hears, so the
    /// count is given a little while to come back. A run that happens to
    /// have more processes alive at once than any before may grow a table
    /// for good (a frame, now and then), so it gets three tries: a leak
    /// loses frames on every one.
    pub(crate) fn memory_freed() -> TestResult {
        if !crate::port::devroot::has("boot/init") {
            return Err("no boot archive with init".into());
        }
        // Once first, so the heap has grown to what a run needs.
        run_init()?;
        let mut before = mm::frame_counts().0;
        for _ in 0..3 {
            run_init()?;
            if clock::wait_until(|| mm::frame_counts().0 >= before, 2 * SECOND) {
                return Ok(());
            }
            let after = mm::frame_counts().0;
            crate::println!("ktest: {before} free frames before, {after} after");
            before = after;
        }
        Err("programs' memory not freed".into())
    }
}
