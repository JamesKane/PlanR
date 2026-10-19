//! Kernel context switch.
//!
//! A switched-out thread's callee-saved state (AAPCS64: x19-x29, the link
//! register, sp, and the low halves of v8-v15, since the kernel target
//! uses the FP/SIMD registers) is kept in its [`Context`].

use core::arch::global_asm;

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
