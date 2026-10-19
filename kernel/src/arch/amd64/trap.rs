//! amd64 traps: the GDT and TSS, the IDT, the trap entry path, and the
//! system call entry.
//!
//! Every vector has a 16-byte stub that pushes a uniform frame (a zero
//! error code where the CPU pushes none, then the vector number) and jumps
//! to a common entry, which saves the general registers and calls [`trap`].
//! Double faults, NMIs and machine checks run on their own IST stacks, so a
//! kernel stack overflow still produces a report.
//!
//! `syscall` enters at `planr_syscall_entry`, which builds the same frame
//! by hand on the thread's kernel stack and calls [`syscall`]. It returns
//! with `sysret` when it can, else through the trap exit's `iretq`.
//!
//! **GS.** In the kernel the `GS` base points at the CPU's [`Cpu`]; in user
//! mode it is the user's. Every way in from user mode (`syscall`, or a
//! trap whose saved CS is ring 3) runs `swapgs` first, and every way out
//! runs it last. An NMI or machine check that lands between `syscall` and
//! its `swapgs` would find the user's GS; both are fatal for now, and the
//! report is the only thing that would go wrong.
//!
//! [`Cpu`]: crate::port::cpu::Cpu

use core::arch::{asm, global_asm};
use core::cell::UnsafeCell;
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::port::cpu::{self, Cpu, MAXCPU};
use crate::port::segment::{self, Access};
use crate::port::{trap, uaccess};

const KERNEL_CS: u16 = 0x08;
const KERNEL_DS: u16 = 0x10;
const TSS_SEL: u16 = 0x28;
/// User selectors, with RPL 3.
pub(super) const USER_SS: u64 = 0x18 | 3;
pub(super) const USER_CS: u64 = 0x20 | 3;
/// The flags user mode starts with: interrupts on (and the reserved bit).
pub(super) const USER_RFLAGS: u64 = 0x202;
/// The flags user mode may change: CF PF AF ZF SF TF DF OF AC ID.
const USER_FLAGS: u64 = 0x0024_0dd5;
/// The `vector` of a frame built by the system call entry.
const SYSCALL_VECTOR: u64 = 256;

const IA32_EFER: u32 = 0xc000_0080;
const EFER_SCE: u64 = 1 << 0;
const IA32_STAR: u32 = 0xc000_0081;
const IA32_LSTAR: u32 = 0xc000_0082;
const IA32_FMASK: u32 = 0xc000_0084;
/// Flags `syscall` clears: TF IF DF IOPL NT AC.
const SYSCALL_FMASK: u64 = 0x0004_7700;

/// GDT layout. User data sits below user code so that `SYSRET` (CS =
/// STAR+16, SS = STAR+8) works with STAR[63:48] = `KERNEL_DS | 3`.
const GDT: [u64; 5] = [
    0,
    0x00af_9a00_0000_ffff, // 0x08 kernel code: 64-bit, DPL 0
    0x00cf_9200_0000_ffff, // 0x10 kernel data
    0x00cf_f200_0000_ffff, // 0x18 user data: DPL 3
    0x00af_fa00_0000_ffff, // 0x20 user code: 64-bit, DPL 3
];

pub(crate) const IST_BYTES: usize = 16 * 1024;
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

/// One CPU's descriptor tables. The IDT is shared.
#[repr(C, align(16))]
struct CpuTables {
    gdt: [u64; GDT.len() + 2],
    tss: Tss,
}

impl CpuTables {
    const EMPTY: CpuTables = CpuTables {
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
    };
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

/// Interior-mutable static storage, written during bring-up.
struct Boot<T>(UnsafeCell<T>);

// SAFETY: the IDT and CPU 0's stacks are written only by `init` on the boot
// CPU before interrupts are enabled. CPU n's tables are written only by
// `prepare`, before CPU n starts, and afterwards only by CPU n itself (`ltr`
// marks its TSS descriptor busy).
unsafe impl<T> Sync for Boot<T> {}

static TABLES: Boot<[CpuTables; MAXCPU]> = Boot(UnsafeCell::new([const { CpuTables::EMPTY }; MAXCPU]));

/// The boot CPU's IST stacks, static because traps are set up before the
/// memory allocator. Other CPUs' come from [`crate::port::mm::alloc_stack`].
static CPU0_IST: Boot<[Stack; 3]> = Boot(UnsafeCell::new([const { Stack([0; IST_BYTES]) }; 3]));

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
    /// Where `syscall` enters (below).
    static planr_syscall_entry: u8;
}

/// Builds the IDT and installs it with CPU 0's GDT and TSS on the boot CPU.
#[allow(clippy::cast_possible_truncation)] // vector numbers are < 256
pub(crate) fn init() {
    let vectors = (&raw const planr_trap_vectors) as u64;
    // SAFETY: runs once on the boot CPU with interrupts disabled, before
    // anything else uses the IDT or CPU 0's stacks.
    let tops = unsafe {
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
        let stacks = &*CPU0_IST.0.get();
        core::array::from_fn(|i| (&raw const stacks[i]) as u64 + IST_BYTES as u64)
    };
    prepare(0, tops);
    load(0);
}

/// Fills in CPU `index`'s GDT and TSS; `ist` holds the tops of its
/// double-fault, NMI and machine-check stacks. Called before that CPU
/// starts.
#[allow(clippy::cast_possible_truncation)] // the TSS size is a small constant
pub(crate) fn prepare(index: usize, ist: [u64; 3]) {
    // SAFETY: CPU `index` is not running yet (or, for CPU 0, has not loaded
    // these tables), so nothing else uses them.
    let t = unsafe { &mut (*TABLES.0.get())[index] };
    t.tss = Tss {
        _reserved0: 0,
        rsp: [0; 3],
        _reserved1: 0,
        ist: [ist[0], ist[1], ist[2], 0, 0, 0, 0],
        _reserved2: 0,
        _reserved3: 0,
        iomap_base: size_of::<Tss>() as u16, // no I/O permission bitmap
    };
    t.gdt[..GDT.len()].copy_from_slice(&GDT);
    t.gdt[GDT.len()..].copy_from_slice(&tss_descriptor((&raw const t.tss) as u64));
}

/// Loads CPU `index`'s GDT and TSS and the shared IDT on the calling CPU.
#[allow(clippy::cast_possible_truncation)] // table sizes are small constants
pub(crate) fn load(index: usize) {
    // SAFETY: `prepare` filled these tables; only this CPU uses them.
    unsafe {
        let t = &(*TABLES.0.get())[index];
        let gdtr = Pointer {
            limit: (size_of_val(&t.gdt) - 1) as u16,
            base: (&raw const t.gdt) as u64,
        };
        let idtr = Pointer {
            limit: (size_of::<[Gate; 256]>() - 1) as u16,
            base: IDT.0.get() as u64,
        };
        asm!(
            "lgdt [{gdtr}]",
            // Reload CS with a far return, then the data segments. GS is
            // left alone: its base holds the per-CPU pointer.
            "push {cs}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ss, {ds:x}",
            "mov ds, {ds:x}",
            "mov es, {ds:x}",
            "mov fs, {zero:x}",
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
    // SYSRET's selectors come from STAR[63:48] with RPL 3 already in it:
    // Intel ORs 3 into SS, but AMD loads it as given, and a user SS of RPL
    // 0 passes until something returns to user mode by `iretq` and faults.
    let star = u64::from(KERNEL_DS | 3) << 48 | u64::from(KERNEL_CS) << 32;
    // SAFETY: `syscall` enters at the entry below, on the kernel stack
    // `set_kernel_stack` gives it; the selectors match the GDT.
    unsafe {
        super::cpu::wrmsr(IA32_STAR, star);
        super::cpu::wrmsr(IA32_LSTAR, (&raw const planr_syscall_entry) as u64);
        super::cpu::wrmsr(IA32_FMASK, SYSCALL_FMASK);
        super::cpu::wrmsr(IA32_EFER, super::cpu::rdmsr(IA32_EFER) | EFER_SCE);
    }
}

/// The calling CPU's state for the system call entry, at `gs:8` in its
/// [`Cpu`].
#[repr(C)]
#[derive(Debug)]
pub(crate) struct CpuLocal {
    /// The user stack pointer, while the entry saves it.
    scratch: AtomicU64,
    /// The running thread's kernel stack top.
    kstack: AtomicU64,
}

impl CpuLocal {
    pub(crate) const fn new() -> CpuLocal {
        CpuLocal {
            scratch: AtomicU64::new(0),
            kstack: AtomicU64::new(0),
        }
    }
}

/// Where the entry finds `CpuLocal`'s fields.
const SCRATCH: usize = core::mem::offset_of!(Cpu, arch) + core::mem::offset_of!(CpuLocal, scratch);
const KSTACK: usize = core::mem::offset_of!(Cpu, arch) + core::mem::offset_of!(CpuLocal, kstack);

/// Sets the stack the calling CPU enters the kernel on from user mode:
/// TSS `rsp0` for traps, and `CpuLocal` for `syscall`.
pub(super) fn set_kernel_stack(top: u64) {
    let c = cpu::current();
    c.arch.kstack.store(top, Ordering::Relaxed);
    // SAFETY: only this CPU uses its TSS, and the CPU reads `rsp0` only on
    // a trap from user mode, which cannot happen while the kernel runs.
    unsafe {
        let t = &raw mut (*TABLES.0.get())[c.index()].tss;
        (&raw mut (*t).rsp).cast::<u64>().write_unaligned(top);
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
    // From user mode (the saved CS is ring 3): take the kernel's GS.
    testb $3, 24(%rsp)
    jz 1f
    swapgs
1:  pushq %rax
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
    // Close any user-access window the trap interrupted; iretq reopens it.
    cmpb $0, {smap}(%rip)
    je 1f
    clac
1:  call {trap}
    // Also where a child of rfork first leaves for user mode, with its
    // frame at the top of its stack (`resume_user`).
    .global planr_trap_exit
planr_trap_exit:
.Ltrap_exit:
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
    testb $3, 8(%rsp)
    jz 1f
    swapgs
1:  iretq

    // syscall: rcx = the user's rip, r11 its rflags, interrupts off
    // (FMASK). Builds a trap frame on the thread's kernel stack.
    .global planr_syscall_entry
planr_syscall_entry:
    swapgs
    movq %rsp, %gs:{scratch}
    movq %gs:{kstack}, %rsp
    pushq ${user_ss}
    pushq %gs:{scratch}
    pushq %r11
    pushq ${user_cs}
    pushq %rcx
    pushq $0
    pushq ${sysvec}
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
    movq %rsp, %rdi
    cld
    call {syscall}
    // False: return through iretq.
    testb %al, %al
    jz .Ltrap_exit
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
    // Past the vector and error code: rip, cs, rflags, then rsp.
    movq 40(%rsp), %rsp
    swapgs
    sysretq
    "#,
    trap = sym trap,
    syscall = sym syscall,
    smap = sym super::protect::SMAP,
    scratch = const SCRATCH,
    kstack = const KSTACK,
    user_ss = const USER_SS,
    user_cs = const USER_CS,
    sysvec = const SYSCALL_VECTOR,
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

impl PfError {
    /// The page was not present (rather than present but protected).
    fn not_present(self) -> bool {
        self.0 & 1 == 0
    }

    fn access(self) -> Access {
        if self.0 & (1 << 4) != 0 {
            Access::Exec
        } else if self.0 & (1 << 1) != 0 {
            Access::Write
        } else {
            Access::Read
        }
    }
}

/// True if the fault at `addr` was on a page not yet mapped in a segment
/// of the running process, or a write to a copy-on-write page, and the
/// page is now mapped as the access needs: retry it.
fn demand_page(addr: u64, error: PfError) -> bool {
    (error.not_present() || error.access() == Access::Write) && segment::fault(addr, error.access())
}

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
    let user = f.cs & 3 == 3;
    match ex {
        Exception::Interrupt(v) => {
            if !super::interrupt(v) {
                trap::fatal(&ex, f, f.rip, f.rbp);
            }
            crate::port::proc::preempt();
        }
        // A fault the CPU could not push a frame for, from a stack pointer
        // in unmapped memory: the stack ran into its guard.
        Exception::DoubleFault if !user && probe_read(f.rsp.wrapping_sub(8)).is_none() => trap::fatal(
            &format_args!("kernel stack overflow: double fault, sp {:#x}", f.rsp),
            f,
            f.rip,
            f.rbp,
        ),
        // Not the user code's doing, wherever they arrive.
        Exception::Nmi | Exception::MachineCheck | Exception::DoubleFault => trap::fatal(&ex, f, f.rip, f.rbp),
        // A page of a segment, touched for the first time.
        Exception::PageFault { addr, error } if user && demand_page(addr, error) => {}
        // Anything else from user mode is the thread's own fault.
        _ if user => crate::port::syscall::user_trap(&ex, f.rip),
        // int3 leaves rip after itself, so returning resumes.
        Exception::Breakpoint => trap::count_breakpoint(),
        Exception::PageFault { .. } | Exception::GeneralProtection(_) => match trap::fixup(f.rip) {
            // A user copy on a page not yet touched: map it and retry
            // (`rep movsb` picks up where it stopped).
            Some(_) if matches!(ex, Exception::PageFault { addr, error } if demand_page(addr, error)) => {}
            Some(resume) => f.rip = resume,
            None => trap::fatal(&ex, f, f.rip, f.rbp),
        },
        _ => trap::fatal(&ex, f, f.rip, f.rbp),
    }
}

/// Called from the system call entry with the frame it built, interrupts
/// off. Returns true if the way back may be `sysret`.
extern "C" fn syscall(f: &mut TrapFrame) -> bool {
    super::spllo();
    let args = [f.rdi, f.rsi, f.rdx, f.r10, f.r8, f.r9];
    let [value, error] = crate::port::syscall::dispatch(f.rax, args);
    let _ = super::splhi();
    f.rax = value;
    f.rdx = error;
    f.rflags = (f.rflags & USER_FLAGS) | USER_RFLAGS;
    // What sysret loads rip and rflags from.
    f.rcx = f.rip;
    f.r11 = f.rflags;
    // sysret faults in ring 0 on a non-canonical rip, so keep it to the
    // user half proper.
    f.rip < uaccess::USER_TOP
}

/// The bytes of the frame a system call builds at the top of the kernel
/// stack.
const USER_FRAME_BYTES: u64 = size_of::<TrapFrame>() as u64;

/// Copies the user frame of the system call being made, at the top of the
/// kernel stack ending at `from`, to the top of the one ending at `to`,
/// made to return 0 with no error; returns the bytes it takes there.
///
/// # Safety
///
/// The calling thread is in a system call from user mode on the stack
/// ending at `from`; the stack ending at `to` is mapped and unused.
pub(crate) unsafe fn fork_frame(from: u64, to: u64) -> u64 {
    let dst = (to - USER_FRAME_BYTES) as *mut TrapFrame;
    // SAFETY: the caller's contract: the entry built the frame at the top
    // of `from`, and the other stack has room for it.
    let f = unsafe {
        core::ptr::copy_nonoverlapping((from - USER_FRAME_BYTES) as *const TrapFrame, dst, 1);
        &mut *dst
    };
    f.rax = 0;
    f.rdx = 0;
    f.rflags = (f.rflags & USER_FLAGS) | USER_RFLAGS;
    f.rcx = f.rip;
    f.r11 = f.rflags;
    USER_FRAME_BYTES
}

/// Leaves for user mode through the trap exit, with the frame
/// [`fork_frame`] wrote at the top of the stack ending at `top`.
///
/// # Safety
///
/// `top` is the top of the calling thread's kernel stack, nothing on it
/// below the frame is needed any more, and the frame's addresses are in
/// the active space.
pub(crate) unsafe fn resume_user(top: u64) -> ! {
    // SAFETY: the caller's contract; the exit swaps GS for ring 3 and
    // returns by iretq.
    unsafe {
        asm!(
            "cli",
            "mov rsp, {}",
            "jmp planr_trap_exit",
            in(reg) top - USER_FRAME_BYTES,
            options(noreturn),
        );
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

// The same for a word in a user page, inside a `stac`/`clac` window when
// SMAP is on. The `cmp` sets ZF for "no SMAP" and nothing after it changes
// the flags; a fault resumes at 4 with the flags (and AC) it saved.
global_asm!(
    ".section .text.probe, \"ax\"",
    ".global planr_probe_user_read",
    "planr_probe_user_read:",
    "    cmp byte ptr [rip + {smap}], 0",
    "    je 2f",
    "    stac",
    "2:  mov rdx, [rdi]",
    "    je 3f",
    "    clac",
    "3:  mov [rsi], rdx",
    "    mov eax, 1",
    "    ret",
    "4:  je 5f",
    "    clac",
    "5:  mov eax, 0",
    "    ret",
    ".pushsection .fixup, \"a\"",
    ".balign 8",
    ".quad 2b, 4b",
    ".popsection",
    smap = sym super::protect::SMAP,
);

// Copies `rdx` bytes from `rsi` to `rdi`, one side user memory, inside a
// `stac`/`clac` window as above, and returns how many were not copied: a
// fault in `rep movsb` resumes at 3 with rcx counting what is left.
global_asm!(
    ".section .text.probe, \"ax\"",
    ".global planr_copy_user",
    "planr_copy_user:",
    "    mov rcx, rdx",
    "    cmp byte ptr [rip + {smap}], 0",
    "    je 2f",
    "    stac",
    "2:  rep movsb",
    "3:  je 4f",
    "    clac",
    "4:  mov rax, rcx",
    "    ret",
    ".pushsection .fixup, \"a\"",
    ".balign 8",
    ".quad 2b, 3b",
    ".popsection",
    smap = sym super::protect::SMAP,
);

unsafe extern "C" {
    fn planr_probe_read(addr: u64, out: *mut u64) -> bool;
    fn planr_probe_user_read(addr: u64, out: *mut u64) -> bool;
    fn planr_copy_user(dst: *mut u8, src: *const u8, len: usize) -> usize;
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
    unsafe { planr_copy_user(dst, src as *const u8, len) }
}

/// Copies `len` bytes from `src` to the user address `dst`, stopping at a
/// fault; returns how many were not copied.
///
/// # Safety
///
/// `src` is valid for `len` bytes of reads.
pub(crate) unsafe fn copy_out(dst: u64, src: *const u8, len: usize) -> usize {
    // SAFETY: the caller's contract; a fault writing `dst` is fixed up.
    unsafe { planr_copy_user(dst as *mut u8, src, len) }
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
    // SAFETY: as for `probe_read`; the window is closed on both exits.
    unsafe { planr_probe_user_read(addr, &raw mut v) }.then_some(v)
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
