//! Kernel context switch, and the user state that goes with a thread.
//!
//! A switched-out thread's callee-saved registers are pushed on its own
//! stack, and its [`Context`] keeps only the stack pointer. The kernel is
//! built without SSE, so it never touches the x87/SSE registers: a user
//! thread's stay live in them while it runs in the kernel, and are saved
//! in its [`UserRegs`] only when it is switched out.

use core::arch::{asm, global_asm};

use super::cpu;

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

const IA32_FS_BASE: u32 = 0xc000_0100;

const CR0_MP: u64 = 1 << 1;
const CR0_EM: u64 = 1 << 2;
const CR0_TS: u64 = 1 << 3;
const CR0_NE: u64 = 1 << 5;
const CR4_OSFXSR: u64 = 1 << 9;
const CR4_OSXMMEXCPT: u64 = 1 << 10;

/// Lets user code use x87 and SSE on the calling CPU, with `fxsave` and
/// `fxrstor`. UEFI leaves them on already; this makes sure.
pub(crate) fn init_cpu() {
    let cr0: u64;
    // SAFETY: reads CR0.
    unsafe { asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack)) };
    let cr0 = (cr0 | CR0_MP | CR0_NE) & !(CR0_EM | CR0_TS);
    // SAFETY: only the FPU bits change; the kernel uses no FPU state.
    unsafe {
        asm!("mov cr0, {}", in(reg) cr0, options(nostack));
        cpu::write_cr4(cpu::read_cr4() | CR4_OSFXSR | CR4_OSXMMEXCPT);
    }
}

/// The `fxsave` area: x87, MMX and SSE state.
#[repr(C, align(16))]
#[derive(Debug, Clone)]
struct FxArea([u8; 512]);

impl FxArea {
    /// The state after `fninit`, with every exception masked
    /// (FCW 0x37f, MXCSR 0x1f80).
    const INITIAL: FxArea = {
        let mut a = [0; 512];
        a[0] = 0x7f;
        a[1] = 0x03;
        a[24] = 0x80;
        a[25] = 0x1f;
        FxArea(a)
    };
}

/// A user thread's registers that the kernel's context switch does not
/// otherwise keep.
#[derive(Debug, Clone)]
pub(crate) struct UserRegs {
    fx: FxArea,
    /// The `FS` base: the thread pointer (`settls`).
    tls: u64,
}

impl UserRegs {
    pub(crate) const INITIAL: UserRegs = UserRegs {
        fx: FxArea::INITIAL,
        tls: 0,
    };

    /// Saves the calling CPU's user state, which belongs to the thread
    /// being switched out.
    pub(crate) fn save(&mut self) {
        // SAFETY: writes the 512-byte, 16-aligned area.
        unsafe { asm!("fxsave64 [{}]", in(reg) &raw mut self.fx, options(nostack, preserves_flags)) };
    }

    /// Loads this thread's user state into the calling CPU.
    pub(crate) fn restore(&self) {
        // SAFETY: the area came from `fxsave` or is `INITIAL`, both valid;
        // `tls` is a canonical address (`set_tls`).
        unsafe {
            asm!("fxrstor64 [{}]", in(reg) &raw const self.fx, options(nostack, preserves_flags));
            cpu::wrmsr(IA32_FS_BASE, self.tls);
        }
    }

    /// Sets the thread pointer, here and in the calling CPU. `tls` must
    /// be a user address, so canonical.
    pub(crate) fn set_tls(&mut self, tls: u64) {
        self.tls = tls;
        // SAFETY: the FS base is the user's; the kernel does not use FS.
        unsafe { cpu::wrmsr(IA32_FS_BASE, tls) };
    }
}

/// Leaves the kernel for user mode at `pc`, with stack pointer `sp` and
/// `arg` in the first argument register, on a kernel stack emptied to
/// `top`. Every other register starts at zero; the x87/SSE state is the
/// thread's, already loaded by the switch to it.
///
/// # Safety
///
/// `top` is the top of the calling thread's kernel stack, which nothing
/// on it is needed any more; `pc` and `sp` are user addresses in the
/// active space.
pub(crate) unsafe fn enter_user(pc: u64, sp: u64, arg: u64, top: u64) -> ! {
    // SAFETY: the caller's contract. The frame is the one iretq pops; GS
    // is swapped to the user's last, with interrupts off.
    unsafe {
        asm!(
            "cli",
            "mov rsp, {top}",
            "push {ss}",
            "push {sp}",
            "push {flags}",
            "push {cs}",
            "push {pc}",
            "xor eax, eax",
            "xor ebx, ebx",
            "xor ecx, ecx",
            "xor edx, edx",
            "xor esi, esi",
            "xor ebp, ebp",
            "xor r8d, r8d",
            "xor r9d, r9d",
            "xor r10d, r10d",
            "xor r11d, r11d",
            "xor r12d, r12d",
            "xor r13d, r13d",
            "xor r14d, r14d",
            "xor r15d, r15d",
            "swapgs",
            "iretq",
            top = in(reg) top,
            ss = const super::trap::USER_SS,
            sp = in(reg) sp,
            flags = const super::trap::USER_RFLAGS,
            cs = const super::trap::USER_CS,
            pc = in(reg) pc,
            in("rdi") arg,
            options(noreturn),
        );
    }
}

/// Sets the stack the calling CPU enters the kernel on from user mode,
/// for the thread it is switching to.
pub(crate) fn set_kernel_stack(top: u64) {
    super::trap::set_kernel_stack(top);
}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use core::arch::global_asm;

    use abi::Sys;

    // The user test program (`port/syscall.rs`), position independent.
    // rdi = its data page: [0] a tag, [8] the mode (1: fault at once); it
    // writes [16] settls's error word, [24] its verdict (1 passed, 2 FP or
    // thread pointer lost, 3 a register lost), [32] and [40] errstr's
    // result pair, [48] an unknown call's error word, and swaps the
    // string at 0x100 with its error text. It sits in rodata: the kernel
    // copies it to a user page and never runs it itself.
    global_asm!(
        ".pushsection .rodata.usertest, \"a\"",
        ".balign 16",
        ".global planr_user_test, planr_user_test_end",
        "planr_user_test:",
        "    mov r12, rdi",
        "    cmp qword ptr [r12 + 8], 1",
        "    jne 1f",
        "    xor eax, eax",
        "    mov rax, qword ptr [rax]",
        "1:  mov eax, {settls}",
        "    mov rdi, r12",
        "    syscall",
        "    mov qword ptr [r12 + 16], rdx",
        "    movq xmm0, qword ptr [r12]",
        "    mov r14, qword ptr [r12]",
        "    mov r13d, 50",
        "2:  mov eax, {sleep}",
        "    xor edi, edi",
        "    syscall",
        "    movq rax, xmm0",
        "    cmp rax, qword ptr [r12]",
        "    jne 5f",
        "    mov rax, qword ptr fs:[0]",
        "    cmp rax, qword ptr [r12]",
        "    jne 5f",
        "    cmp r14, qword ptr [r12]",
        "    jne 6f",
        "    dec r13d",
        "    jnz 2b",
        "    mov eax, 999",
        "    syscall",
        "    mov qword ptr [r12 + 48], rdx",
        "    mov eax, {errstr}",
        "    lea rdi, [r12 + 0x100]",
        "    mov esi, 128",
        "    syscall",
        "    mov qword ptr [r12 + 32], rax",
        "    mov qword ptr [r12 + 40], rdx",
        "    mov qword ptr [r12 + 24], 1",
        "    jmp 7f",
        "5:  mov qword ptr [r12 + 24], 2",
        "    jmp 7f",
        "6:  mov qword ptr [r12 + 24], 3",
        "7:  mov eax, {exits}",
        "    xor edi, edi",
        "    syscall",
        "    ud2",
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
