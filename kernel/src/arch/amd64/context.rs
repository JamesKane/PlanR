//! Kernel context switch.
//!
//! A switched-out thread's callee-saved registers are pushed on its own
//! stack, and its [`Context`] keeps only the stack pointer. The kernel is
//! built without SSE, so there is no floating-point state to keep.

use core::arch::global_asm;

/// A thread's saved kernel context.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct Context {
    rsp: u64,
}

// planr_switch(from: *mut Context, to: *const Context): pushes the
// callee-saved registers, saves rsp in `from`, loads `to`'s, pops them and
// returns into whatever `to` was switched out from (or `planr_thread_start`).
global_asm!(
    ".text",
    ".global planr_switch",
    "planr_switch:",
    "    push rbp",
    "    push rbx",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    "    mov [rdi], rsp",
    "    mov rsp, [rsi]",
    "    pop r15",
    "    pop r14",
    "    pop r13",
    "    pop r12",
    "    pop rbx",
    "    pop rbp",
    "    ret",
    // A new thread: r12 = its argument, r13 = its entry. The pushed zero
    // is a return address that ends backtraces and leaves the stack as a
    // call would.
    ".global planr_thread_start",
    "planr_thread_start:",
    "    mov rdi, r12",
    "    push 0",
    "    jmp r13",
);

unsafe extern "C" {
    fn planr_switch(from: *mut Context, to: *const Context);
    static planr_thread_start: u8;
}

impl Context {
    /// A context to save into; switching to it is not allowed.
    pub(crate) const EMPTY: Context = Context { rsp: 0 };

    /// A context that, switched to, calls `entry(arg)` on the stack ending
    /// at `top` (16-byte aligned).
    ///
    /// # Safety
    ///
    /// The stack is mapped, writable and used by nothing else.
    pub(crate) unsafe fn new(top: u64, entry: extern "C" fn(usize) -> !, arg: usize) -> Context {
        // What planr_switch pops: r15, r14, r13, r12, rbx, rbp, then the
        // return address.
        let frame: [u64; 7] = [
            0,
            0,
            entry as usize as u64,
            arg as u64,
            0,
            0,
            (&raw const planr_thread_start) as u64,
        ];
        let rsp = top - 8 * frame.len() as u64;
        // SAFETY: the caller's contract; the frame fits below `top`.
        unsafe { core::ptr::copy_nonoverlapping(frame.as_ptr(), rsp as *mut u64, frame.len()) };
        Context { rsp }
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
