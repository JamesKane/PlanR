//! Notes: what a note handler is given, and how it ends.
//!
//! **Delivery.** A process that has set a handler with `notify` gets each
//! note on its way back to user mode, as Plan 9 delivers them. The kernel
//! saves the interrupted registers as a [`Ureg`] on the user stack, below
//! the stack pointer by at least 256 bytes (so the red zone and what lies
//! just under it survive), with the note's text, NUL-terminated, below
//! that. It then calls the handler as a C function:
//! `handler(ureg: *mut Ureg, note: *const u8)`, with the stack aligned as
//! a call leaves it and a return address of 0. The handler ends with
//! `noted` ([`NoteHow`]); returning faults.
//!
//! While a handler runs, other notes from processes wait; a note from the
//! kernel (`sys: …`) in a handler, or with no handler set, ends the
//! process with the note as its exit status.

/// What `noted` does once a handler is done.
#[repr(u64)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteHow {
    /// Resume with the registers in the `Ureg` (which the handler may have
    /// changed).
    Cont = 0,
    /// Do what would have happened with no handler: exit with the note.
    Dflt = 1,
    /// Resume as `Cont` does, but keep the `Ureg` on the stack, with its
    /// address in the first argument register.
    Save = 2,
    /// `Cont`, allowed outside a handler as well.
    Rstr = 3,
}

impl NoteHow {
    #[must_use]
    pub fn from_raw(raw: u64) -> Option<NoteHow> {
        match raw {
            0 => Some(NoteHow::Cont),
            1 => Some(NoteHow::Dflt),
            2 => Some(NoteHow::Save),
            3 => Some(NoteHow::Rstr),
            _ => None,
        }
    }
}

/// The bytes the kernel leaves untouched under the interrupted stack
/// pointer.
pub const NOTE_GAP: u64 = 256;

/// The registers of the interrupted code, as a note handler sees them:
/// the kernel's trap frame.
#[cfg(target_arch = "x86_64")]
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ureg {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    /// The trap's vector (256 for a system call), and its error code.
    pub trap: u64,
    pub error: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

#[cfg(target_arch = "x86_64")]
impl Ureg {
    #[must_use]
    pub fn pc(&self) -> u64 {
        self.rip
    }

    #[must_use]
    pub fn sp(&self) -> u64 {
        self.rsp
    }

    /// Makes the code resume at `pc`.
    pub fn set_pc(&mut self, pc: u64) {
        self.rip = pc;
    }

    /// Makes the registers call `pc(args[0], args[1])` with the stack at
    /// `sp`, 16-byte aligned; returns the stack pointer the call starts
    /// with, where its return address goes.
    pub fn call(&mut self, pc: u64, sp: u64, args: [u64; 2]) -> u64 {
        self.rip = pc;
        self.rsp = sp - 8;
        self.rdi = args[0];
        self.rsi = args[1];
        self.rbp = 0;
        self.rsp
    }
}

/// The registers of the interrupted code, as a note handler sees them:
/// the kernel's trap frame, with the user stack pointer in `sp`.
#[cfg(target_arch = "aarch64")]
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ureg {
    /// x0-x30.
    pub x: [u64; 31],
    /// Where it resumes (`ELR_EL1`).
    pub pc: u64,
    /// `SPSR_EL1`: only the condition flags may change.
    pub psr: u64,
    /// The syndrome and fault address of the trap, if it was one.
    pub esr: u64,
    pub far: u64,
    /// `SP_EL0`.
    pub sp: u64,
    /// The kernel's vector index.
    pub vector: u64,
    pub pad: u64,
    /// The FP/SIMD registers.
    pub v: [u128; 32],
    pub fpcr: u64,
    pub fpsr: u64,
}

#[cfg(target_arch = "aarch64")]
impl Ureg {
    #[must_use]
    pub fn pc(&self) -> u64 {
        self.pc
    }

    #[must_use]
    pub fn sp(&self) -> u64 {
        self.sp
    }

    /// Makes the code resume at `pc`.
    pub fn set_pc(&mut self, pc: u64) {
        self.pc = pc;
    }

    /// Makes the registers call `pc(args[0], args[1])` with the stack at
    /// `sp`, 16-byte aligned, and a return address of 0; returns the stack
    /// pointer the call starts with.
    pub fn call(&mut self, pc: u64, sp: u64, args: [u64; 2]) -> u64 {
        self.pc = pc;
        self.sp = sp;
        self.x[0] = args[0];
        self.x[1] = args[1];
        self.x[29] = 0;
        self.x[30] = 0;
        sp
    }
}
