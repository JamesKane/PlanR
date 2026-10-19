//! Kernel context switch, and the user state that goes with a thread.
//!
//! A switched-out thread's callee-saved state (AAPCS64: x19-x29, the link
//! register, sp, and the low halves of v8-v15, since the kernel target
//! uses the FP/SIMD registers) is kept in its [`Context`]. A user thread's
//! registers, FP/SIMD included, are in the trap frame at the top of its
//! kernel stack whenever it is in the kernel; only the thread pointer,
//! `TPIDR_EL0`, which user code may write, is left for [`UserRegs`].

use core::arch::{asm, global_asm};

/// A thread's saved kernel context.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct Context {
    /// x19-x28, x29 (fp), x30 (lr).
    x: [u64; 12],
    sp: u64,
    d: [u64; 8],
}

// planr_switch(from: *mut Context, to: *const Context).
global_asm!(
    ".text",
    ".global planr_switch",
    "planr_switch:",
    "    stp x19, x20, [x0, #0]",
    "    stp x21, x22, [x0, #16]",
    "    stp x23, x24, [x0, #32]",
    "    stp x25, x26, [x0, #48]",
    "    stp x27, x28, [x0, #64]",
    "    stp x29, x30, [x0, #80]",
    "    mov x9, sp",
    "    str x9, [x0, #96]",
    "    stp d8, d9, [x0, #104]",
    "    stp d10, d11, [x0, #120]",
    "    stp d12, d13, [x0, #136]",
    "    stp d14, d15, [x0, #152]",
    "    ldp x19, x20, [x1, #0]",
    "    ldp x21, x22, [x1, #16]",
    "    ldp x23, x24, [x1, #32]",
    "    ldp x25, x26, [x1, #48]",
    "    ldp x27, x28, [x1, #64]",
    "    ldp x29, x30, [x1, #80]",
    "    ldr x9, [x1, #96]",
    "    mov sp, x9",
    "    ldp d8, d9, [x1, #104]",
    "    ldp d10, d11, [x1, #120]",
    "    ldp d12, d13, [x1, #136]",
    "    ldp d14, d15, [x1, #152]",
    "    ret",
    // A new thread: x19 = its argument, x20 = its entry; a zero link
    // register ends backtraces.
    ".global planr_thread_start",
    "planr_thread_start:",
    "    mov x0, x19",
    "    mov x30, xzr",
    "    br x20",
);

const _: () = assert!(size_of::<Context>() == 168);

unsafe extern "C" {
    fn planr_switch(from: *mut Context, to: *const Context);
    static planr_thread_start: u8;
}

impl Context {
    /// A context to save into; switching to it is not allowed.
    pub(crate) const EMPTY: Context = Context {
        x: [0; 12],
        sp: 0,
        d: [0; 8],
    };

    /// A context that, switched to, calls `entry(arg)` on the stack ending
    /// at `top` (16-byte aligned).
    ///
    /// # Safety
    ///
    /// The stack is mapped, writable and used by nothing else.
    pub(crate) unsafe fn new(top: u64, entry: extern "C" fn(usize) -> !, arg: usize) -> Context {
        let mut x = [0; 12];
        x[0] = arg as u64;
        x[1] = entry as usize as u64;
        x[11] = (&raw const planr_thread_start) as u64;
        Context { x, sp: top, d: [0; 8] }
    }
}

/// Saves the current context in `from` and resumes `to`. Returns when
/// something switches back to `from`.
///
/// # Safety
///
/// Interrupts are off. `to` holds a context saved by `switch` or made by
/// [`Context::new`], which nothing else will resume, and `from` stays
/// valid until it is resumed.
pub(crate) unsafe fn switch(from: *mut Context, to: *const Context) {
    // SAFETY: the caller's contract.
    unsafe { planr_switch(from, to) };
}

/// Nothing to do: 9boot sets `CPACR_EL1.FPEN` to 0b11, so FP/SIMD works
/// at EL0 as at EL1.
pub(crate) fn init_cpu() {}

/// A user thread's registers that its trap frame does not keep.
#[derive(Debug, Clone)]
pub(crate) struct UserRegs {
    /// `TPIDR_EL0`: the thread pointer.
    tls: u64,
}

impl UserRegs {
    pub(crate) const INITIAL: UserRegs = UserRegs { tls: 0 };

    /// Saves the calling CPU's user state, which belongs to the thread
    /// being switched out.
    pub(crate) fn save(&mut self) {
        // SAFETY: reads a register.
        unsafe { asm!("mrs {}, tpidr_el0", out(reg) self.tls, options(nomem, nostack, preserves_flags)) };
    }

    /// Loads this thread's user state into the calling CPU. Also clears
    /// `TPIDRRO_EL0`, which user code can read and the overflow path
    /// writes.
    pub(crate) fn restore(&self) {
        // SAFETY: EL0's registers; nothing at EL1 reads them.
        unsafe {
            asm!(
                "msr tpidr_el0, {}",
                "msr tpidrro_el0, xzr",
                in(reg) self.tls,
                options(nomem, nostack, preserves_flags),
            );
        }
    }

    /// Sets the thread pointer, here and in the calling CPU.
    pub(crate) fn set_tls(&mut self, tls: u64) {
        self.tls = tls;
        // SAFETY: EL0's register.
        unsafe { asm!("msr tpidr_el0, {}", in(reg) tls, options(nomem, nostack, preserves_flags)) };
    }
}

/// Leaves the kernel for EL0 at `pc`, with stack pointer `sp` and `arg`
/// in x0, on a kernel stack emptied to `top`. Every other register,
/// FP/SIMD included, starts at zero.
///
/// # Safety
///
/// `top` is the top of the calling thread's kernel stack, which nothing
/// on it is needed any more; `pc` and `sp` are user addresses in the
/// active space.
pub(crate) unsafe fn enter_user(pc: u64, sp: u64, arg: u64, top: u64) -> ! {
    // SAFETY: the caller's contract. With every exception masked, sp
    // becomes the stack the next exception from EL0 is taken on; SPSR = 0
    // is EL0t with nothing masked.
    unsafe {
        asm!(
            "msr daifset, #0xf",
            "mov sp, {top}",
            "msr sp_el0, {sp}",
            "msr elr_el1, {pc}",
            "msr spsr_el1, xzr",
            "msr fpcr, xzr",
            "msr fpsr, xzr",
            "movi v0.2d, #0",
            "movi v1.2d, #0",
            "movi v2.2d, #0",
            "movi v3.2d, #0",
            "movi v4.2d, #0",
            "movi v5.2d, #0",
            "movi v6.2d, #0",
            "movi v7.2d, #0",
            "movi v8.2d, #0",
            "movi v9.2d, #0",
            "movi v10.2d, #0",
            "movi v11.2d, #0",
            "movi v12.2d, #0",
            "movi v13.2d, #0",
            "movi v14.2d, #0",
            "movi v15.2d, #0",
            "movi v16.2d, #0",
            "movi v17.2d, #0",
            "movi v18.2d, #0",
            "movi v19.2d, #0",
            "movi v20.2d, #0",
            "movi v21.2d, #0",
            "movi v22.2d, #0",
            "movi v23.2d, #0",
            "movi v24.2d, #0",
            "movi v25.2d, #0",
            "movi v26.2d, #0",
            "movi v27.2d, #0",
            "movi v28.2d, #0",
            "movi v29.2d, #0",
            "movi v30.2d, #0",
            "movi v31.2d, #0",
            "mov x1, xzr",
            "mov x2, xzr",
            "mov x3, xzr",
            "mov x4, xzr",
            "mov x5, xzr",
            "mov x6, xzr",
            "mov x7, xzr",
            "mov x8, xzr",
            "mov x9, xzr",
            "mov x10, xzr",
            "mov x11, xzr",
            "mov x12, xzr",
            "mov x13, xzr",
            "mov x14, xzr",
            "mov x15, xzr",
            "mov x16, xzr",
            "mov x17, xzr",
            "mov x18, xzr",
            "mov x19, xzr",
            "mov x20, xzr",
            "mov x21, xzr",
            "mov x22, xzr",
            "mov x23, xzr",
            "mov x24, xzr",
            "mov x25, xzr",
            "mov x26, xzr",
            "mov x27, xzr",
            "mov x28, xzr",
            "mov x29, xzr",
            "mov x30, xzr",
            "eret",
            top = in(reg) top,
            sp = in(reg) sp,
            pc = in(reg) pc,
            in("x0") arg,
            options(noreturn),
        );
    }
}

/// Nothing to do: an exception from EL0 is taken on `SP_EL1`, which is
/// the top of the thread's kernel stack whenever it is in user mode
/// ([`enter_user`], and the trap exit's frame).
pub(crate) fn set_kernel_stack(_top: u64) {}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use core::arch::global_asm;

    use abi::Sys;

    // The user test program (`port/syscall.rs`), position independent.
    // x0 = its data page, laid out as the amd64 version describes.
    global_asm!(
        ".pushsection .rodata.usertest, \"a\"",
        ".balign 16",
        ".global planr_user_test, planr_user_test_end",
        "planr_user_test:",
        "    mov x19, x0",
        "    ldr x9, [x19, #8]",
        "    cmp x9, #1",
        "    b.ne 1f",
        "    mov x9, xzr",
        "    ldr x9, [x9]",
        "1:  mov x8, #{settls}",
        "    mov x0, x19",
        "    svc #0",
        "    str x1, [x19, #16]",
        "    ldr d0, [x19]",
        "    ldr x21, [x19]",
        "    mov x20, #50",
        "2:  mov x8, #{sleep}",
        "    mov x0, xzr",
        "    svc #0",
        "    ldr x10, [x19]",
        "    fmov x9, d0",
        "    cmp x9, x10",
        "    b.ne 5f",
        "    mrs x9, tpidr_el0",
        "    ldr x9, [x9]",
        "    cmp x9, x10",
        "    b.ne 5f",
        "    cmp x21, x10",
        "    b.ne 6f",
        "    subs x20, x20, #1",
        "    b.ne 2b",
        "    mov x8, #999",
        "    svc #0",
        "    str x1, [x19, #48]",
        "    mov x8, #{errstr}",
        "    add x0, x19, #0x100",
        "    mov x1, #128",
        "    svc #0",
        "    stp x0, x1, [x19, #32]",
        "    mov x9, #1",
        "    str x9, [x19, #24]",
        "    b 7f",
        "5:  mov x9, #2",
        "    str x9, [x19, #24]",
        "    b 7f",
        "6:  mov x9, #3",
        "    str x9, [x19, #24]",
        "7:  mov x8, #{exits}",
        "    mov x0, xzr",
        "    svc #0",
        "    brk #0",
        "planr_user_test_end:",
        ".popsection",
        settls = const Sys::Settls as u64,
        sleep = const Sys::Sleep as u64,
        errstr = const Sys::Errstr as u64,
        exits = const Sys::Exits as u64,
    );

    unsafe extern "C" {
        static planr_user_test: u8;
        static planr_user_test_end: u8;
    }

    /// The user test program's bytes.
    pub(crate) fn user_test() -> &'static [u8] {
        let start = &raw const planr_user_test;
        let end = &raw const planr_user_test_end;
        // SAFETY: the assembler placed the program between the two
        // symbols, in rodata.
        unsafe { core::slice::from_raw_parts(start, end as usize - start as usize) }
    }
}
