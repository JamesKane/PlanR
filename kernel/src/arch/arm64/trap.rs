//! arm64 exceptions: the `VBAR_EL1` vector table and the trap entry path.
//!
//! Each of the 16 vectors saves x0/x1, loads its index, and branches to a
//! common entry that saves the rest of the state into a [`TrapFrame`] and
//! calls [`trap`]. The index tells the handler where the exception came
//! from (current EL on `SP_EL0` or `SP_EL1`, lower EL in `AArch64` or `AArch32`)
//! and what it is (synchronous, IRQ, FIQ, `SError`).
//!
//! The kernel runs on `SP_EL1`, so an exception taken because the stack ran
//! into its guard would fault again saving the frame, and again, forever.
//! There is no IST to switch stacks for us, so each vector checks first,
//! without touching memory: kernel stacks are laid out so that a stack
//! pointer within one has bit 16 clear (see `port/mm.rs`). If the frame's
//! address has it set, the vector switches to this CPU's overflow stack
//! before saving anything, and the handler reports the overflow.

use core::arch::{asm, global_asm};
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::port::cpu::MAXCPU;
use crate::port::segment::{self, Access};
use crate::port::{mm, trap};

/// The saved state of the interrupted code, as the entry path builds it.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct TrapFrame {
    x: [u64; 31],
    elr: u64,
    spsr: u64,
    esr: u64,
    far: u64,
    sp: u64,
    /// Vector index, 0-15.
    vector: u64,
    _pad: u64,
    /// The FP/SIMD registers: the kernel target uses them, so a handler
    /// (or, once threads are preempted, another thread) would otherwise
    /// clobber the interrupted code's.
    q: [u128; 32],
    fpcr: u64,
    fpsr: u64,
}

/// Where [`TrapFrame::q`] starts.
const FP_OFFSET: usize = 304;
const FRAME_BYTES: usize = FP_OFFSET + 32 * 16 + 16;
const _: () = assert!(core::mem::offset_of!(TrapFrame, q) == FP_OFFSET);
const _: () = assert!(size_of::<TrapFrame>() == FRAME_BYTES);

/// The `vector` a frame built on the overflow stack carries: which vector
/// was taken is lost, but an overflow is a synchronous abort.
const OVERFLOW_VECTOR: u64 = 16;
/// The bit the vectors test; see `port/mm.rs`.
const STACK_BIT: u32 = mm::STACK_SLOT.trailing_zeros() - 1;
const _: () = assert!(mm::STACK_PAGES * mm::PAGE == 1 << STACK_BIT);

/// A stack for reporting an overflow, placed like a kernel stack: the lower
/// half of a slot of its own, so that a stack pointer within it has bit 16
/// clear and a fault taken while reporting is handled normally.
#[repr(C, align(131072))]
struct OverflowStack([u8; OVERFLOW_BYTES]);
const OVERFLOW_BYTES: usize = 64 * 1024;
const _: () = assert!(align_of::<OverflowStack>() as u64 == mm::STACK_SLOT);
const _: () = assert!(OVERFLOW_BYTES as u64 == mm::STACK_SLOT / 2);

/// The boot CPU's overflow stack, ready before anything can trap.
static mut BOOT_OVERFLOW_STACK: OverflowStack = OverflowStack([0; OVERFLOW_BYTES]);

/// The top of each other CPU's overflow stack, set before it starts.
static OVERFLOW_STACKS: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];

/// Gives CPU `index` an overflow stack; called before it starts.
pub(super) fn prepare(index: usize) -> Result<(), &'static str> {
    let top = mm::alloc_stack(mm::STACK_PAGES).ok_or("no memory for an overflow stack")?;
    OVERFLOW_STACKS
        .get(index)
        .ok_or("CPU index out of range")?
        .store(top, Ordering::Release);
    Ok(())
}

// Each vector checks bit 16 of the frame address with only x0 to hand, by
// stashing x0 in sp: sp + x0 - x0 recovers either.
global_asm!(
    ".section .text.vectors, \"ax\"",
    ".balign 2048",
    ".global planr_vectors",
    "planr_vectors:",
    ".set .Lvec, 0",
    ".rept 16",
    ".balign 128",
    "    sub sp, sp, #{size}",
    "    add sp, sp, x0",      // sp = frame + x0
    "    sub x0, sp, x0",      // x0 = frame
    "    tbnz x0, #{bit}, .Lstack_overflow",
    "    sub x0, sp, x0",      // x0 = x0
    "    sub sp, sp, x0",      // sp = frame
    "    stp x0, x1, [sp]",
    "    mov x1, #.Lvec",
    "    b .Ltrap_entry",
    ".set .Lvec, .Lvec + 1",
    ".endr",
    "",
    // x0 = the frame address, sp = frame + x0. The interrupted x0 and the
    // frame address go to the EL0 thread registers, which nothing at EL1
    // reads. That loses the running user thread's TPIDR_EL0, but the
    // report is fatal, so nothing runs after it.
    ".Lstack_overflow:",
    "    msr tpidr_el0, x0",
    "    sub x0, sp, x0",
    "    msr tpidrro_el0, x0",
    // sp = the overflow stack of CPU TPIDR_EL1, the boot CPU's static one
    // for CPU 0, an index out of range (TPIDR_EL1 not yet set), or a CPU
    // without one.
    "    mrs x0, tpidr_el1",
    "    cmp x0, #{maxcpu}",
    "    b.hs 1f",
    "    lsl x0, x0, #3",
    "    mov sp, x0",
    "    adrp x0, {stacks}",
    "    add x0, x0, :lo12:{stacks}",
    "    add x0, sp, x0",
    "    ldr x0, [x0]",
    "    cbnz x0, 2f",
    "1:  adrp x0, {boot_stack}",
    "    add x0, x0, :lo12:{boot_stack}",
    "    add x0, x0, #{half_k}, lsl #12",
    "2:  mov sp, x0",
    "    sub sp, sp, #{size}",
    "    mrs x0, tpidrro_el0",
    "    stp x0, x1, [sp]",
    "    mov x1, #{overflow}",
    "",
    ".Ltrap_entry:",
    "    stp x2, x3, [sp, #16]",
    "    stp x4, x5, [sp, #32]",
    "    stp x6, x7, [sp, #48]",
    "    stp x8, x9, [sp, #64]",
    "    stp x10, x11, [sp, #80]",
    "    stp x12, x13, [sp, #96]",
    "    stp x14, x15, [sp, #112]",
    "    stp x16, x17, [sp, #128]",
    "    stp x18, x19, [sp, #144]",
    "    stp x20, x21, [sp, #160]",
    "    stp x22, x23, [sp, #176]",
    "    stp x24, x25, [sp, #192]",
    "    stp x26, x27, [sp, #208]",
    "    stp x28, x29, [sp, #224]",
    "    mrs x2, elr_el1",
    "    stp x30, x2, [sp, #240]",
    "    mrs x3, spsr_el1",
    "    mrs x4, esr_el1",
    "    stp x3, x4, [sp, #256]",
    "    mrs x5, far_el1",
    "    add x6, sp, #{size}",
    "    stp x5, x6, [sp, #272]",
    "    str x1, [sp, #288]",
    "    add x0, sp, #{fp}",
    "    stp q0, q1, [x0, #0]",
    "    stp q2, q3, [x0, #32]",
    "    stp q4, q5, [x0, #64]",
    "    stp q6, q7, [x0, #96]",
    "    stp q8, q9, [x0, #128]",
    "    stp q10, q11, [x0, #160]",
    "    stp q12, q13, [x0, #192]",
    "    stp q14, q15, [x0, #224]",
    "    stp q16, q17, [x0, #256]",
    "    stp q18, q19, [x0, #288]",
    "    stp q20, q21, [x0, #320]",
    "    stp q22, q23, [x0, #352]",
    "    stp q24, q25, [x0, #384]",
    "    stp q26, q27, [x0, #416]",
    "    stp q28, q29, [x0, #448]",
    "    stp q30, q31, [x0, #480]",
    "    mrs x2, fpcr",
    "    mrs x3, fpsr",
    "    str x2, [x0, #512]",
    "    str x3, [x0, #520]",
    "    mov x0, sp",
    "    bl {trap}",
    // Also where a child of rfork first leaves for EL0, with its frame at
    // the top of its stack (`resume_user`). The handler may have changed
    // elr (to resume elsewhere).
    ".global planr_trap_return",
    "planr_trap_return:",
    "    ldp x30, x2, [sp, #240]",
    "    ldr x3, [sp, #256]",
    "    msr elr_el1, x2",
    "    msr spsr_el1, x3",
    "    add x0, sp, #{fp}",
    "    ldp q0, q1, [x0, #0]",
    "    ldp q2, q3, [x0, #32]",
    "    ldp q4, q5, [x0, #64]",
    "    ldp q6, q7, [x0, #96]",
    "    ldp q8, q9, [x0, #128]",
    "    ldp q10, q11, [x0, #160]",
    "    ldp q12, q13, [x0, #192]",
    "    ldp q14, q15, [x0, #224]",
    "    ldp q16, q17, [x0, #256]",
    "    ldp q18, q19, [x0, #288]",
    "    ldp q20, q21, [x0, #320]",
    "    ldp q22, q23, [x0, #352]",
    "    ldp q24, q25, [x0, #384]",
    "    ldp q26, q27, [x0, #416]",
    "    ldp q28, q29, [x0, #448]",
    "    ldp q30, q31, [x0, #480]",
    "    ldr x2, [x0, #512]",
    "    ldr x3, [x0, #520]",
    "    msr fpcr, x2",
    "    msr fpsr, x3",
    "    ldp x0, x1, [sp]",
    "    ldp x2, x3, [sp, #16]",
    "    ldp x4, x5, [sp, #32]",
    "    ldp x6, x7, [sp, #48]",
    "    ldp x8, x9, [sp, #64]",
    "    ldp x10, x11, [sp, #80]",
    "    ldp x12, x13, [sp, #96]",
    "    ldp x14, x15, [sp, #112]",
    "    ldp x16, x17, [sp, #128]",
    "    ldp x18, x19, [sp, #144]",
    "    ldp x20, x21, [sp, #160]",
    "    ldp x22, x23, [sp, #176]",
    "    ldp x24, x25, [sp, #192]",
    "    ldp x26, x27, [sp, #208]",
    "    ldp x28, x29, [sp, #224]",
    "    add sp, sp, #{size}",
    "    eret",
    size = const FRAME_BYTES,
    fp = const FP_OFFSET,
    trap = sym trap,
    bit = const STACK_BIT,
    maxcpu = const MAXCPU,
    stacks = sym OVERFLOW_STACKS,
    boot_stack = sym BOOT_OVERFLOW_STACK,
    half_k = const mm::STACK_SLOT / 2 / 4096,
    overflow = const OVERFLOW_VECTOR,
);

unsafe extern "C" {
    static planr_vectors: [u8; 2048];
}

/// Installs the vector table and unmasks `SError`, so asynchronous aborts are
/// reported rather than left pending.
pub(crate) fn init() {
    let base = (&raw const planr_vectors) as u64;
    // SAFETY: the table is complete and 2 KiB aligned; the isb makes the
    // new VBAR visible before any exception can be taken.
    unsafe { asm!("msr vbar_el1, {}", "isb", "msr daifclr, #4", in(reg) base, options(nostack)) };
}

/// Fault status codes (`ESR_ELx.ISS.{I,D}FSC`).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Fault {
    AddressSize(u8),
    Translation(u8),
    AccessFlag(u8),
    Permission(u8),
    SyncExternal,
    Alignment,
    TlbConflict,
    Other(u8),
}

impl Fault {
    fn decode(fsc: u8) -> Fault {
        let level = fsc & 3;
        match fsc {
            0b00_0000..=0b00_0011 => Fault::AddressSize(level),
            0b00_0100..=0b00_0111 => Fault::Translation(level),
            0b00_1000..=0b00_1011 => Fault::AccessFlag(level),
            0b00_1100..=0b00_1111 => Fault::Permission(level),
            0b01_0000 => Fault::SyncExternal,
            0b10_0001 => Fault::Alignment,
            0b11_0000 => Fault::TlbConflict,
            other => Fault::Other(other),
        }
    }
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fault::AddressSize(l) => write!(f, "address size fault (level {l})"),
            Fault::Translation(l) => write!(f, "translation fault (level {l})"),
            Fault::AccessFlag(l) => write!(f, "access flag fault (level {l})"),
            Fault::Permission(l) => write!(f, "permission fault (level {l})"),
            Fault::SyncExternal => f.write_str("synchronous external abort"),
            Fault::Alignment => f.write_str("alignment fault"),
            Fault::TlbConflict => f.write_str("TLB conflict"),
            Fault::Other(c) => write!(f, "fault status {c:#x}"),
        }
    }
}

/// Where an exception was taken from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// EL1 using `SP_EL0`: the kernel never runs like this.
    KernelSp0,
    Kernel,
    User,
    User32,
}

/// A decoded exception.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Exception {
    Undefined,
    WaitTrapped,
    FpAccess,
    BranchTarget,
    IllegalState,
    Svc(u16),
    Hvc(u16),
    Smc(u16),
    SysReg,
    PacFailure,
    InstructionAbort { addr: u64, fault: Fault },
    DataAbort { addr: u64, fault: Fault, write: bool },
    PcAlignment(u64),
    SpAlignment,
    Debug(u8),
    Breakpoint(u16),
    Irq,
    Fiq,
    SError(u32),
    Other { ec: u8, iss: u32 },
}

impl Exception {
    #[allow(clippy::cast_possible_truncation)] // ESR fields
    fn decode(f: &TrapFrame) -> Exception {
        let ec = ((f.esr >> 26) & 0x3f) as u8;
        let iss = (f.esr & 0x01ff_ffff) as u32;
        let imm = iss as u16;
        let fault = Fault::decode((iss & 0x3f) as u8);
        match f.vector & 3 {
            1 => return Exception::Irq,
            2 => return Exception::Fiq,
            3 => return Exception::SError(iss),
            _ => {}
        }
        match ec {
            0x00 => Exception::Undefined,
            0x01 => Exception::WaitTrapped,
            0x07 => Exception::FpAccess,
            0x0d => Exception::BranchTarget,
            0x0e => Exception::IllegalState,
            0x15 => Exception::Svc(imm),
            0x16 => Exception::Hvc(imm),
            0x17 => Exception::Smc(imm),
            0x18 => Exception::SysReg,
            0x1c => Exception::PacFailure,
            0x20 | 0x21 => Exception::InstructionAbort { addr: f.far, fault },
            0x22 => Exception::PcAlignment(f.far),
            0x24 | 0x25 => Exception::DataAbort {
                addr: f.far,
                fault,
                write: iss & (1 << 6) != 0,
            },
            0x26 => Exception::SpAlignment,
            0x30..=0x35 => Exception::Debug(ec),
            0x3c => Exception::Breakpoint(imm),
            _ => Exception::Other { ec, iss },
        }
    }
}

impl fmt::Display for Exception {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exception::Undefined => f.write_str("undefined instruction"),
            Exception::WaitTrapped => f.write_str("trapped WFI/WFE"),
            Exception::FpAccess => f.write_str("FP/SIMD access trapped"),
            Exception::BranchTarget => f.write_str("branch target exception"),
            Exception::IllegalState => f.write_str("illegal execution state"),
            Exception::Svc(i) => write!(f, "svc #{i}"),
            Exception::Hvc(i) => write!(f, "hvc #{i}"),
            Exception::Smc(i) => write!(f, "smc #{i}"),
            Exception::SysReg => f.write_str("trapped system register access"),
            Exception::PacFailure => f.write_str("pointer authentication failure"),
            Exception::InstructionAbort { addr, fault } => {
                write!(f, "instruction abort: {fault}, address {addr:#x}")
            }
            Exception::DataAbort { addr, fault, write } => {
                let access = if *write { "write" } else { "read" };
                write!(f, "data abort: {fault}, {access}, address {addr:#x}")
            }
            Exception::PcAlignment(a) => write!(f, "PC alignment fault, address {a:#x}"),
            Exception::SpAlignment => f.write_str("SP alignment fault"),
            Exception::Debug(ec) => write!(f, "debug exception (class {ec:#x})"),
            Exception::Breakpoint(i) => write!(f, "brk #{i}"),
            Exception::Irq => f.write_str("unexpected IRQ"),
            Exception::Fiq => f.write_str("unexpected FIQ"),
            Exception::SError(iss) => write!(f, "SError, syndrome {iss:#x}"),
            Exception::Other { ec, iss } => write!(f, "exception class {ec:#x}, syndrome {iss:#x}"),
        }
    }
}

impl fmt::Display for TrapFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, v) in self.x.iter().enumerate() {
            match i {
                29 => write!(f, " fp  {v:016x} ")?,
                30 => write!(f, " lr  {v:016x} ")?,
                _ => write!(f, " x{i:<2} {v:016x} ")?,
            }
            if i % 4 == 3 {
                writeln!(f)?;
            }
        }
        writeln!(f)?;
        write!(
            f,
            "  sp {:016x}  elr {:016x}  spsr {:08x}  esr {:08x}  far {:016x}",
            self.sp, self.elr, self.spsr, self.esr, self.far
        )
    }
}

/// Reports a trap whose frame address failed the vectors' bit-16 check.
/// The frame is on the overflow stack, and the interrupted stack pointer
/// went to `TPIDR_EL0`.
fn overflow(f: &mut TrapFrame) -> ! {
    let frame: u64;
    // SAFETY: reads the register the overflow path wrote.
    unsafe { asm!("mrs {}, tpidr_el0", out(reg) frame, options(nomem, nostack)) };
    f.sp = frame.wrapping_add(FRAME_BYTES as u64);
    // The vector is lost; an overflow is a synchronous exception from EL1.
    f.vector = 4;
    let ex = Exception::decode(f);
    // Bit 16 also fails on a stack not laid out like a kernel stack (9boot's,
    // before the boot CPU leaves it); there the frame would have fit.
    if probe_read(frame).is_some() {
        trap::fatal(
            &format_args!("{ex} (on a foreign stack, sp {:#x})", f.sp),
            f,
            f.elr,
            f.x[29],
        );
    }
    trap::fatal(
        &format_args!("kernel stack overflow: {ex}, sp {:#x}", f.sp),
        f,
        f.elr,
        f.x[29],
    )
}

/// True if `ex` was a translation fault on a page not yet mapped in a
/// segment of the running process, or a write to a copy-on-write page,
/// and the page is now mapped as the access needs: retry it.
fn demand_page(ex: Exception) -> bool {
    match ex {
        Exception::DataAbort {
            addr,
            fault: Fault::Translation(_),
            write,
        } => segment::fault(addr, if write { Access::Write } else { Access::Read }),
        Exception::DataAbort {
            addr,
            fault: Fault::Permission(_),
            write: true,
        } => segment::fault(addr, Access::Write),
        Exception::InstructionAbort {
            addr,
            fault: Fault::Translation(_),
        } => segment::fault(addr, Access::Exec),
        _ => false,
    }
}

/// Called from the entry path with the saved frame; returning resumes it.
extern "C" fn trap(f: &mut TrapFrame) {
    if f.vector == OVERFLOW_VECTOR {
        overflow(f);
    }
    let ex = Exception::decode(f);
    let origin = match f.vector >> 2 {
        0 => Origin::KernelSp0,
        1 => Origin::Kernel,
        2 => Origin::User,
        _ => Origin::User32,
    };
    match (origin, ex) {
        (Origin::Kernel | Origin::User, Exception::Irq) => {
            if !super::interrupt() {
                trap::fatal(&format_args!("unhandled IRQ"), f, f.elr, f.x[29]);
            }
            crate::port::proc::preempt();
        }
        // Not the user code's doing.
        (Origin::User, Exception::Fiq | Exception::SError(_)) | (Origin::KernelSp0 | Origin::User32, _) => {
            trap::fatal(&format_args!("{ex} (from {origin:?})"), f, f.elr, f.x[29]);
        }
        // elr is already past the svc.
        (Origin::User, Exception::Svc(0)) => syscall(f),
        // A page of a segment, touched for the first time.
        (Origin::User, _) if demand_page(ex) => {}
        (Origin::User, _) => crate::port::syscall::user_trap(&ex, f.elr),
        // elr points at the brk itself; step over it.
        (_, Exception::Breakpoint(_)) => {
            trap::count_breakpoint();
            f.elr += 4;
        }
        (_, Exception::DataAbort { .. }) => match trap::fixup(f.elr) {
            // A user copy on a page not yet touched: map it and retry the
            // load or store.
            Some(_) if demand_page(ex) => {}
            Some(resume) => f.elr = resume,
            None => trap::fatal(&ex, f, f.elr, f.x[29]),
        },
        _ => trap::fatal(&ex, f, f.elr, f.x[29]),
    }
}

/// A system call: the number in x8, arguments in x0-x5, results back in
/// x0 and x1. Runs with interrupts on.
fn syscall(f: &mut TrapFrame) {
    super::spllo();
    let args = [f.x[0], f.x[1], f.x[2], f.x[3], f.x[4], f.x[5]];
    let [value, error] = crate::port::syscall::dispatch(f.x[8], args);
    let _ = super::splhi();
    f.x[0] = value;
    f.x[1] = error;
}

/// Copies the user frame of the system call being made, at the top of the
/// kernel stack ending at `from`, to the top of the one ending at `to`,
/// made to return 0 with no error; returns the bytes it takes there.
///
/// # Safety
///
/// The calling thread is in a system call from EL0 on the stack ending at
/// `from`; the stack ending at `to` is mapped and unused.
pub(crate) unsafe fn fork_frame(from: u64, to: u64) -> u64 {
    let size = FRAME_BYTES as u64;
    let dst = (to - size) as *mut TrapFrame;
    // SAFETY: the caller's contract: an exception from EL0 is taken at the
    // top of the stack, and the other stack has room for the frame.
    let f = unsafe {
        core::ptr::copy_nonoverlapping((from - size) as *const TrapFrame, dst, 1);
        &mut *dst
    };
    f.x[0] = 0;
    f.x[1] = 0;
    f.sp = to;
    size
}

/// Leaves for EL0 through the trap exit, with the frame [`fork_frame`]
/// wrote at the top of the stack ending at `top`.
///
/// # Safety
///
/// `top` is the top of the calling thread's kernel stack, nothing on it
/// below the frame is needed any more, and the frame's addresses are in
/// the active space.
pub(crate) unsafe fn resume_user(top: u64) -> ! {
    // SAFETY: the caller's contract. With every exception masked, the exit
    // restores the frame and erets, leaving sp at `top` for the next
    // exception from EL0.
    unsafe {
        asm!(
            "msr daifset, #0xf",
            "mov sp, {}",
            "b planr_trap_return",
            in(reg) top - FRAME_BYTES as u64,
            options(noreturn),
        );
    }
}

/// Nothing per CPU: an exception from EL0 is taken on `SP_EL1` as it was
/// left.
#[derive(Debug)]
pub(crate) struct CpuLocal;

impl CpuLocal {
    pub(crate) const fn new() -> CpuLocal {
        CpuLocal
    }
}

// Reads a word, recovering from any fault through a fixup entry.
global_asm!(
    ".section .text.probe, \"ax\"",
    ".global planr_probe_read",
    "planr_probe_read:",
    "2:  ldr x2, [x0]",
    "    str x2, [x1]",
    "    mov w0, #1",
    "    ret",
    "3:  mov w0, #0",
    "    ret",
    ".pushsection .fixup, \"a\"",
    ".balign 8",
    ".quad 2b, 3b",
    ".popsection",
);

// The same for a word in a user page, inside a window with PAN clear when
// PAN is on. Exception entry sets PAN again (`SCTLR_EL1.SPAN` is clear) and
// eret restores the saved PSTATE, so a fault resumes at 4 with PAN clear;
// x3 survives the trap.
global_asm!(
    ".section .text.probe, \"ax\"",
    ".arch_extension pan",
    ".global planr_probe_user_read",
    "planr_probe_user_read:",
    "    adrp x3, {pan}",
    "    ldrb w3, [x3, :lo12:{pan}]",
    "    cbz w3, 2f",
    "    msr pan, #0",
    "2:  ldr x2, [x0]",
    "    cbz w3, 3f",
    "    msr pan, #1",
    "3:  str x2, [x1]",
    "    mov w0, #1",
    "    ret",
    "4:  cbz w3, 5f",
    "    msr pan, #1",
    "5:  mov w0, #0",
    "    ret",
    ".pushsection .fixup, \"a\"",
    ".balign 8",
    ".quad 2b, 4b",
    ".popsection",
    pan = sym super::protect::PAN,
);

// Byte copies with the unprivileged loads and stores, which access memory
// as EL0 would: PAN does not stop them, and they fault on kernel pages. A
// fault resumes at 9 with x2 counting the bytes left.
global_asm!(
    ".section .text.probe, \"ax\"",
    // planr_copy_in(dst: kernel, src: user, len)
    ".global planr_copy_in",
    "planr_copy_in:",
    "    cbz x2, 9f",
    "2:  ldtrb w3, [x1]",
    "    strb w3, [x0], #1",
    "    add x1, x1, #1",
    "    subs x2, x2, #1",
    "    b.ne 2b",
    "9:  mov x0, x2",
    "    ret",
    ".pushsection .fixup, \"a\"",
    ".balign 8",
    ".quad 2b, 9b",
    ".popsection",
    // planr_copy_out(dst: user, src: kernel, len)
    ".global planr_copy_out",
    "planr_copy_out:",
    "    cbz x2, 9f",
    "1:  ldrb w3, [x1], #1",
    "2:  sttrb w3, [x0]",
    "    add x0, x0, #1",
    "    subs x2, x2, #1",
    "    b.ne 1b",
    "9:  mov x0, x2",
    "    ret",
    ".pushsection .fixup, \"a\"",
    ".balign 8",
    ".quad 2b, 9b",
    ".popsection",
);

unsafe extern "C" {
    fn planr_probe_read(addr: u64, out: *mut u64) -> bool;
    fn planr_probe_user_read(addr: u64, out: *mut u64) -> bool;
    fn planr_copy_in(dst: *mut u8, src: u64, len: usize) -> usize;
    fn planr_copy_out(dst: u64, src: *const u8, len: usize) -> usize;
}

/// Copies `len` bytes from the user address `src` to `dst`, stopping at a
/// fault; returns how many were not copied. The caller checks that the
/// range is in the user half ([`crate::port::uaccess`]).
///
/// # Safety
///
/// `dst` is valid for `len` bytes of writes.
pub(crate) unsafe fn copy_in(dst: *mut u8, src: u64, len: usize) -> usize {
    // SAFETY: the caller's contract; a fault reading `src` is fixed up.
    unsafe { planr_copy_in(dst, src, len) }
}

/// Copies `len` bytes from `src` to the user address `dst`, stopping at a
/// fault; returns how many were not copied.
///
/// # Safety
///
/// `src` is valid for `len` bytes of reads.
pub(crate) unsafe fn copy_out(dst: u64, src: *const u8, len: usize) -> usize {
    // SAFETY: the caller's contract; a fault writing `dst` is fixed up.
    unsafe { planr_copy_out(dst, src, len) }
}

/// Reads the word at `addr`, or `None` if the read faults.
pub(crate) fn probe_read(addr: u64) -> Option<u64> {
    let mut v = 0;
    // SAFETY: the only memory written is `v`; a fault at `addr` is fixed
    // up and reported as failure.
    unsafe { planr_probe_read(addr, &raw mut v) }.then_some(v)
}

/// Reads the word at `addr`, a user address, or `None` if the read faults.
/// The one path through which the kernel may touch user memory.
pub(crate) fn probe_user_read(addr: u64) -> Option<u64> {
    let mut v = 0;
    // SAFETY: as for `probe_read`; PAN is set again on both exits.
    unsafe { planr_probe_user_read(addr, &raw mut v) }.then_some(v)
}

pub(crate) fn breakpoint() {
    // SAFETY: the trap handler counts it and steps over it.
    unsafe { asm!("brk #0", options(nomem, nostack)) };
}

pub(crate) fn frame_pointer() -> u64 {
    let fp: u64;
    // SAFETY: reads a register.
    unsafe { asm!("mov {}, x29", out(reg) fp, options(nomem, nostack)) };
    fp
}
