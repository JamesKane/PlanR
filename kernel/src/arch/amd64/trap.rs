//! amd64 traps: the GDT and TSS, the IDT, and the trap entry path.
//!
//! Every vector has a 16-byte stub that pushes a uniform frame (a zero
//! error code where the CPU pushes none, then the vector number) and jumps
//! to a common entry, which saves the general registers and calls [`trap`].
//! Double faults, NMIs and machine checks run on their own IST stacks, so a
//! kernel stack overflow still produces a report.

use core::arch::{asm, global_asm};
use core::cell::UnsafeCell;
use core::fmt;

use crate::port::trap;

const KERNEL_CS: u16 = 0x08;
const KERNEL_DS: u16 = 0x10;
const TSS_SEL: u16 = 0x28;

/// GDT layout. User data sits below user code so that `SYSRET` (CS =
/// STAR+16, SS = STAR+8) works with STAR[63:48] = `KERNEL_DS` later.
const GDT: [u64; 5] = [
    0,
    0x00af_9a00_0000_ffff, // 0x08 kernel code: 64-bit, DPL 0
    0x00cf_9200_0000_ffff, // 0x10 kernel data
    0x00cf_f200_0000_ffff, // 0x18 user data: DPL 3
    0x00af_fa00_0000_ffff, // 0x20 user code: 64-bit, DPL 3
];

const IST_BYTES: usize = 16 * 1024;
/// IST slots, numbered from 1 as the IDT entry encodes them.
const IST_DOUBLE_FAULT: u8 = 1;
const IST_NMI: u8 = 2;
const IST_MACHINE_CHECK: u8 = 3;

/// The 64-bit task state segment: only the stack pointers matter.
#[repr(C, packed(4))]
struct Tss {
    _reserved0: u32,
    rsp: [u64; 3],
    _reserved1: u64,
    ist: [u64; 7],
    _reserved2: u64,
    _reserved3: u16,
    iomap_base: u16,
}

#[repr(C, align(16))]
struct Stack([u8; IST_BYTES]);

/// One CPU's descriptor tables. Only the boot CPU's exist for now; SMP
/// gives each CPU its own.
#[repr(C, align(16))]
struct CpuTables {
    gdt: [u64; GDT.len() + 2],
    tss: Tss,
    ist: [Stack; 3],
}

/// A 16-byte IDT gate.
#[repr(C)]
#[derive(Clone, Copy)]
struct Gate {
    offset_lo: u16,
    selector: u16,
    ist: u8,
    attr: u8,
    offset_mid: u16,
    offset_hi: u32,
    _zero: u32,
}

/// Present, DPL 0, 64-bit interrupt gate (IF cleared on entry).
const INTERRUPT_GATE: u8 = 0x8e;

impl Gate {
    const MISSING: Gate = Gate {
        offset_lo: 0,
        selector: 0,
        ist: 0,
        attr: 0,
        offset_mid: 0,
        offset_hi: 0,
        _zero: 0,
    };

    #[allow(clippy::cast_possible_truncation)] // splitting the address into fields
    fn new(handler: u64, ist: u8) -> Gate {
        Gate {
            offset_lo: handler as u16,
            selector: KERNEL_CS,
            ist,
            attr: INTERRUPT_GATE,
            offset_mid: (handler >> 16) as u16,
            offset_hi: (handler >> 32) as u32,
            _zero: 0,
        }
    }
}

/// Interior-mutable static storage, written once during bring-up.
struct Boot<T>(UnsafeCell<T>);

// SAFETY: written only by `init` on the boot CPU before interrupts are
// enabled; read-only (by the CPU) afterwards.
unsafe impl<T> Sync for Boot<T> {}

static CPU0: Boot<CpuTables> = Boot(UnsafeCell::new(CpuTables {
    gdt: [0; GDT.len() + 2],
    tss: Tss {
        _reserved0: 0,
        rsp: [0; 3],
        _reserved1: 0,
        ist: [0; 7],
        _reserved2: 0,
        _reserved3: 0,
        iomap_base: 0,
    },
    ist: [const { Stack([0; IST_BYTES]) }; 3],
}));

static IDT: Boot<[Gate; 256]> = Boot(UnsafeCell::new([Gate::MISSING; 256]));

/// Operand of `lgdt`/`lidt`.
#[repr(C, packed)]
struct Pointer {
    limit: u16,
    base: u64,
}

/// The two GDT slots of a 64-bit TSS descriptor.
fn tss_descriptor(base: u64) -> [u64; 2] {
    let limit = (size_of::<Tss>() - 1) as u64;
    let low = (limit & 0xffff)
        | ((base & 0xff_ffff) << 16)
        | (0x89 << 40) // present, 64-bit TSS (available)
        | ((limit >> 16) << 48)
        | (((base >> 24) & 0xff) << 56);
    [low, base >> 32]
}

unsafe extern "C" {
    /// The 256 entry stubs, 16 bytes apart (below).
    static planr_trap_vectors: [u8; 256 * 16];
}

/// Installs the GDT, TSS and IDT on the boot CPU.
#[allow(clippy::cast_possible_truncation)] // table sizes are small constants
pub(crate) fn init() {
    let vectors = (&raw const planr_trap_vectors) as u64;
    // SAFETY: runs once on the boot CPU with interrupts disabled, before
    // anything else uses these tables.
    unsafe {
        let t = &mut *CPU0.0.get();
        let top: [u64; 3] = core::array::from_fn(|i| (&raw const t.ist[i]) as u64 + IST_BYTES as u64);
        t.tss = Tss {
            _reserved0: 0,
            rsp: [0; 3],
            _reserved1: 0,
            ist: [top[0], top[1], top[2], 0, 0, 0, 0],
            _reserved2: 0,
            _reserved3: 0,
            iomap_base: size_of::<Tss>() as u16, // no I/O permission bitmap
        };
        t.gdt[..GDT.len()].copy_from_slice(&GDT);
        t.gdt[GDT.len()..].copy_from_slice(&tss_descriptor((&raw const t.tss) as u64));

        let idt = &mut *IDT.0.get();
        for (v, gate) in idt.iter_mut().enumerate() {
            let ist = match v {
                2 => IST_NMI,
                8 => IST_DOUBLE_FAULT,
                18 => IST_MACHINE_CHECK,
                _ => 0,
            };
            *gate = Gate::new(vectors + 16 * v as u64, ist);
        }

        let gdtr = Pointer {
            limit: (size_of_val(&t.gdt) - 1) as u16,
            base: (&raw const t.gdt) as u64,
        };
        let idtr = Pointer {
            limit: (size_of::<[Gate; 256]>() - 1) as u16,
            base: idt.as_ptr() as u64,
        };
        asm!(
            "lgdt [{gdtr}]",
            // Reload CS with a far return, then the data segments.
            "push {cs}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ss, {ds:x}",
            "mov ds, {ds:x}",
            "mov es, {ds:x}",
            "mov fs, {zero:x}",
            "mov gs, {zero:x}",
            "ltr {tss:x}",
            "lidt [{idtr}]",
            gdtr = in(reg) &raw const gdtr,
            idtr = in(reg) &raw const idtr,
            cs = in(reg) u64::from(KERNEL_CS),
            ds = in(reg) u64::from(KERNEL_DS),
            zero = in(reg) 0u64,
            tss = in(reg) u64::from(TSS_SEL),
            tmp = out(reg) _,
        );
    }
}

// Entry stubs. Vectors 8, 10-14, 17, 21, 29 and 30 push an error code;
// the rest get a zero so every frame has the same shape.
global_asm!(
    r#"
    .section .text.trap, "ax"
    .balign 16
    .global planr_trap_vectors
planr_trap_vectors:
    .set .Lvec, 0
    .rept 256
    .balign 16
    .if .Lvec == 8 || (.Lvec >= 10 && .Lvec <= 14) || .Lvec == 17 || .Lvec == 21 || .Lvec == 29 || .Lvec == 30
    .else
    pushq $0
    .endif
    pushq $.Lvec
    jmp .Ltrap_entry
    .set .Lvec, .Lvec + 1
    .endr

.Ltrap_entry:
    pushq %rax
    pushq %rbx
    pushq %rcx
    pushq %rdx
    pushq %rsi
    pushq %rdi
    pushq %rbp
    pushq %r8
    pushq %r9
    pushq %r10
    pushq %r11
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    // The CPU aligned rsp to 16 before pushing its 5 words; with the error
    // code, vector and 15 registers the frame is 176 bytes, so rsp is
    // aligned for the call.
    movq %rsp, %rdi
    cld
    call {trap}
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %r11
    popq %r10
    popq %r9
    popq %r8
    popq %rbp
    popq %rdi
    popq %rsi
    popq %rdx
    popq %rcx
    popq %rbx
    popq %rax
    addq $16, %rsp
    iretq
    "#,
    trap = sym trap,
    options(att_syntax),
);

/// The saved state of the interrupted code, as the entry path builds it.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct TrapFrame {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    r11: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rbp: u64,
    rdi: u64,
    rsi: u64,
    rdx: u64,
    rcx: u64,
    rbx: u64,
    rax: u64,
    vector: u64,
    error: u64,
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
}

impl fmt::Display for TrapFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let regs = [
            ("rax", self.rax),
            ("rbx", self.rbx),
            ("rcx", self.rcx),
            ("rdx", self.rdx),
            ("rsi", self.rsi),
            ("rdi", self.rdi),
            ("rbp", self.rbp),
            ("rsp", self.rsp),
            ("r8", self.r8),
            ("r9", self.r9),
            ("r10", self.r10),
            ("r11", self.r11),
            ("r12", self.r12),
            ("r13", self.r13),
            ("r14", self.r14),
            ("r15", self.r15),
        ];
        for row in regs.chunks(4) {
            for (name, v) in row {
                write!(f, "{name:>4} {v:016x} ")?;
            }
            writeln!(f)?;
        }
        write!(
            f,
            " rip {:016x}  cs {:04x} rflags {:08x}  ss {:04x}",
            self.rip, self.cs, self.rflags, self.ss
        )
    }
}

/// Page-fault error code bits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PfError(u64);

impl fmt::Display for PfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let e = self.0;
        let cause = if e & 1 == 0 { "not-present" } else { "protection" };
        let access = if e & (1 << 4) != 0 {
            "exec"
        } else if e & (1 << 1) != 0 {
            "write"
        } else {
            "read"
        };
        let mode = if e & (1 << 2) != 0 { "user" } else { "kernel" };
        write!(f, "{cause} {access}, {mode} mode")?;
        if e & (1 << 3) != 0 {
            f.write_str(", reserved bit set")?;
        }
        if e & (1 << 5) != 0 {
            f.write_str(", protection key")?;
        }
        if e & (1 << 6) != 0 {
            f.write_str(", shadow stack")?;
        }
        Ok(())
    }
}

/// A decoded trap.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Exception {
    DivideError,
    Debug,
    Nmi,
    Breakpoint,
    Overflow,
    BoundRange,
    InvalidOpcode,
    DeviceNotAvailable,
    DoubleFault,
    InvalidTss(u64),
    SegmentNotPresent(u64),
    StackFault(u64),
    GeneralProtection(u64),
    PageFault {
        addr: u64,
        error: PfError,
    },
    X87,
    AlignmentCheck,
    MachineCheck,
    Simd,
    Virtualization,
    ControlProtection(u64),
    Reserved(u8),
    /// Vectors 32-255: external and inter-processor interrupts.
    Interrupt(u8),
}

impl Exception {
    #[allow(clippy::cast_possible_truncation)] // vectors are < 256
    fn decode(f: &TrapFrame) -> Exception {
        let e = f.error;
        match f.vector as u8 {
            0 => Exception::DivideError,
            1 => Exception::Debug,
            2 => Exception::Nmi,
            3 => Exception::Breakpoint,
            4 => Exception::Overflow,
            5 => Exception::BoundRange,
            6 => Exception::InvalidOpcode,
            7 => Exception::DeviceNotAvailable,
            8 => Exception::DoubleFault,
            10 => Exception::InvalidTss(e),
            11 => Exception::SegmentNotPresent(e),
            12 => Exception::StackFault(e),
            13 => Exception::GeneralProtection(e),
            14 => Exception::PageFault {
                addr: cr2(),
                error: PfError(e),
            },
            16 => Exception::X87,
            17 => Exception::AlignmentCheck,
            18 => Exception::MachineCheck,
            19 => Exception::Simd,
            20 => Exception::Virtualization,
            21 => Exception::ControlProtection(e),
            v @ 0..32 => Exception::Reserved(v),
            v => Exception::Interrupt(v),
        }
    }
}

impl fmt::Display for Exception {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exception::DivideError => f.write_str("divide error"),
            Exception::Debug => f.write_str("debug exception"),
            Exception::Nmi => f.write_str("non-maskable interrupt"),
            Exception::Breakpoint => f.write_str("breakpoint"),
            Exception::Overflow => f.write_str("overflow"),
            Exception::BoundRange => f.write_str("bound range exceeded"),
            Exception::InvalidOpcode => f.write_str("invalid opcode"),
            Exception::DeviceNotAvailable => f.write_str("device not available"),
            Exception::DoubleFault => f.write_str("double fault"),
            Exception::InvalidTss(e) => write!(f, "invalid TSS, selector {e:#x}"),
            Exception::SegmentNotPresent(e) => write!(f, "segment not present, selector {e:#x}"),
            Exception::StackFault(e) => write!(f, "stack fault, error {e:#x}"),
            Exception::GeneralProtection(e) => write!(f, "general protection fault, error {e:#x}"),
            Exception::PageFault { addr, error } => write!(f, "page fault: {error}, address {addr:#x}"),
            Exception::X87 => f.write_str("x87 floating-point exception"),
            Exception::AlignmentCheck => f.write_str("alignment check"),
            Exception::MachineCheck => f.write_str("machine check"),
            Exception::Simd => f.write_str("SIMD floating-point exception"),
            Exception::Virtualization => f.write_str("virtualization exception"),
            Exception::ControlProtection(e) => write!(f, "control protection fault, error {e:#x}"),
            Exception::Reserved(v) => write!(f, "reserved exception {v}"),
            Exception::Interrupt(v) => write!(f, "unexpected interrupt, vector {v}"),
        }
    }
}

fn cr2() -> u64 {
    let v: u64;
    // SAFETY: reading CR2 has no side effects.
    unsafe { asm!("mov {}, cr2", out(reg) v, options(nomem, nostack)) };
    v
}

/// Called from the entry path with the saved frame; returning resumes it.
extern "C" fn trap(f: &mut TrapFrame) {
    let ex = Exception::decode(f);
    match ex {
        // int3 leaves rip after itself, so returning resumes.
        Exception::Breakpoint => trap::count_breakpoint(),
        Exception::PageFault { .. } | Exception::GeneralProtection(_) => match trap::fixup(f.rip) {
            Some(resume) => f.rip = resume,
            None => trap::fatal(&ex, f, f.rip, f.rbp),
        },
        _ => trap::fatal(&ex, f, f.rip, f.rbp),
    }
}

// Reads a word, recovering from any fault through a fixup entry.
global_asm!(
    ".section .text.probe, \"ax\"",
    ".global planr_probe_read",
    "planr_probe_read:",
    "2:  mov rax, [rdi]",
    "    mov [rsi], rax",
    "    mov eax, 1",
    "    ret",
    "3:  xor eax, eax",
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
    // SAFETY: the trap handler counts it and resumes.
    unsafe { asm!("int3", options(nomem, nostack)) };
}

pub(crate) fn frame_pointer() -> u64 {
    let fp: u64;
    // SAFETY: reads a register.
    unsafe { asm!("mov {}, rbp", out(reg) fp, options(nomem, nostack)) };
    fp
}
