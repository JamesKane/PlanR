//! arm64 exceptions: the `VBAR_EL1` vector table and the trap entry path.
//!
//! Each of the 16 vectors saves x0/x1, loads its index, and branches to a
//! common entry that saves the rest of the state into a [`TrapFrame`] and
//! calls [`trap`]. The index tells the handler where the exception came
//! from (current EL on `SP_EL0` or `SP_EL1`, lower EL in `AArch64` or `AArch32`)
//! and what it is (synchronous, IRQ, FIQ, `SError`).

use core::arch::{asm, global_asm};
use core::fmt;

use crate::port::trap;

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
}

const FRAME_BYTES: usize = 304;
const _: () = assert!(size_of::<TrapFrame>() == FRAME_BYTES);

global_asm!(
    ".section .text.vectors, \"ax\"",
    ".balign 2048",
    ".global planr_vectors",
    "planr_vectors:",
    ".set .Lvec, 0",
    ".rept 16",
    ".balign 128",
    "    sub sp, sp, #{size}",
    "    stp x0, x1, [sp]",
    "    mov x1, #.Lvec",
    "    b .Ltrap_entry",
    ".set .Lvec, .Lvec + 1",
    ".endr",
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
    "    mov x0, sp",
    "    bl {trap}",
    // The handler may have changed elr (to resume elsewhere).
    "    ldp x30, x2, [sp, #240]",
    "    ldr x3, [sp, #256]",
    "    msr elr_el1, x2",
    "    msr spsr_el1, x3",
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
    trap = sym trap,
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

/// Called from the entry path with the saved frame; returning resumes it.
extern "C" fn trap(f: &mut TrapFrame) {
    let ex = Exception::decode(f);
    let origin = match f.vector >> 2 {
        0 => Origin::KernelSp0,
        1 => Origin::Kernel,
        2 => Origin::User,
        _ => Origin::User32,
    };
    if origin != Origin::Kernel {
        trap::fatal(&format_args!("{ex} (from {origin:?})"), f, f.elr, f.x[29]);
    }
    match ex {
        // elr points at the brk itself; step over it.
        Exception::Irq => {
            if !super::interrupt() {
                trap::fatal(&format_args!("unhandled IRQ"), f, f.elr, f.x[29]);
            }
        }
        Exception::Breakpoint(_) => {
            trap::count_breakpoint();
            f.elr += 4;
        }
        Exception::DataAbort { .. } => match trap::fixup(f.elr) {
            Some(resume) => f.elr = resume,
            None => trap::fatal(&ex, f, f.elr, f.x[29]),
        },
        _ => trap::fatal(&ex, f, f.elr, f.x[29]),
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

unsafe extern "C" {
    fn planr_probe_read(addr: u64, out: *mut u64) -> bool;
}

/// Reads the word at `addr`, or `None` if the read faults.
pub(crate) fn probe_read(addr: u64) -> Option<u64> {
    let mut v = 0;
    // SAFETY: the only memory written is `v`; a fault at `addr` is fixed
    // up and reported as failure.
    unsafe { planr_probe_read(addr, &raw mut v) }.then_some(v)
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
