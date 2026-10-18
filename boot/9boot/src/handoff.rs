//! The jump into the kernel, after `ExitBootServices` (ADR 0010).

/// Everything the final jump needs, computed while boot services were up.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Handoff {
    pub(crate) roots: [u64; 2],
    pub(crate) entry: u64,
    pub(crate) stack_top: u64,
    pub(crate) info: u64,
}

#[cfg(target_arch = "x86_64")]
pub(crate) use amd64::{entry_level, jump};
#[cfg(target_arch = "aarch64")]
pub(crate) use arm64::{entry_level, jump, prepare_exec};

#[cfg(target_arch = "x86_64")]
mod amd64 {
    use super::Handoff;
    use bootinfo::EntryLevel;
    use core::arch::asm;

    pub(crate) fn entry_level() -> EntryLevel {
        EntryLevel::NotApplicable
    }

    /// Enables NX and write protection, switches to the boot page tables
    /// and jumps to the kernel with `rdi = &BootInfo`.
    ///
    /// # Safety
    ///
    /// Boot services must have been exited, and `h` must describe valid
    /// page tables that identity-map this code and map the kernel and stack.
    pub(crate) unsafe fn jump(h: &Handoff) -> ! {
        // SAFETY: caller contract. rdmsr/wrmsr use eax, ecx, edx, so the
        // inputs are pinned to other registers.
        unsafe {
            asm!(
                "cli",
                "mov ecx, 0xc0000080", // IA32_EFER
                "rdmsr",
                "or eax, 0x800",       // NXE
                "wrmsr",
                "mov rax, cr0",
                "or rax, 0x10000",     // WP: honour read-only pages in ring 0
                "mov cr0, rax",
                "mov cr3, r8",
                "mov rsp, r9",
                "xor ebp, ebp",
                "jmp r10",
                in("r8") h.roots[0],
                in("r9") h.stack_top,
                in("r10") h.entry,
                in("rdi") h.info,
                options(noreturn),
            )
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod arm64 {
    use super::Handoff;
    use bootinfo::EntryLevel;
    use core::arch::asm;

    /// `SCTLR_EL1`: RES1 bits plus M (MMU), C (data cache), SA (stack
    /// alignment check), I (instruction cache).
    const SCTLR: u64 = 0x30d0_0800 | (1 << 0) | (1 << 2) | (1 << 3) | (1 << 12);
    /// `CPACR_EL1.FPEN` = 0b11: FP/SIMD usable at EL1 (rustc emits NEON).
    const CPACR_FPEN: u64 = 3 << 20;

    const HCR_E2H: u64 = 1 << 34;

    /// EL2 control values for running a kernel at EL1. Several registers
    /// change layout with `HCR_EL2.E2H`.
    #[derive(Debug, Clone, Copy)]
    struct El2Regs {
        e2h: bool,
        hcr: u64,
        cnthctl: u64,
        cptr: u64,
    }

    impl El2Regs {
        fn new(e2h: bool) -> Self {
            // RW: EL1 is AArch64. API/APK: don't trap pointer authentication.
            // E2H is kept as the firmware set it.
            let hcr = (1 << 31) | (1 << 40) | (1 << 41) | if e2h { HCR_E2H } else { 0 };
            if e2h {
                // CNTHCTL: EL1PCTEN (bit 10) and EL1PTEN (bit 11).
                // CPTR (CPACR layout): FPEN and ZEN = 0b11, no traps.
                El2Regs {
                    e2h,
                    hcr,
                    cnthctl: (1 << 10) | (1 << 11),
                    cptr: (3 << 20) | (3 << 16),
                }
            } else {
                // CNTHCTL: EL1PCTEN, EL1PCEN. CPTR: RES1 bits only, TFP clear.
                El2Regs {
                    e2h,
                    hcr,
                    cnthctl: 3,
                    cptr: 0x33ff,
                }
            }
        }
    }

    /// Configures EL1 from EL2 with its MMU enabled in `SCTLR_EL1`, then erets
    /// to the kernel entry with translation already on. The EL1 register
    /// names are parameters (see the E2H note in `jump`). Inputs are pinned
    /// to x0-x12 so none collides with another.
    macro_rules! enter_el1_from_el2 {
        ($h:expr, $regs:expr, $mair:expr, $tcr:expr,
         $r_mair:literal, $r_tcr:literal, $r_ttbr0:literal, $r_ttbr1:literal, $r_cpacr:literal, $r_sctlr:literal) => {
            asm!(
                "msr daifset, #0xf",
                "msr hcr_el2, x9",
                "isb",
                concat!("msr ", $r_mair, ", x1"),
                concat!("msr ", $r_tcr, ", x2"),
                concat!("msr ", $r_ttbr0, ", x3"),
                concat!("msr ", $r_ttbr1, ", x4"),
                concat!("msr ", $r_cpacr, ", x5"),
                "msr cnthctl_el2, x10",
                "msr cntvoff_el2, xzr",
                "msr cptr_el2, x11",
                "msr vttbr_el2, xzr",
                "isb",
                "tlbi vmalle1",
                "dsb ish",
                "isb",
                concat!("msr ", $r_sctlr, ", x6"),
                "msr sp_el1, x7",
                "msr elr_el2, x8",
                "msr spsr_el2, x12",
                "eret",
                in("x0") $h.info,
                in("x1") $mair,
                in("x2") $tcr,
                in("x3") $h.roots[0],
                in("x4") $h.roots[1],
                in("x5") CPACR_FPEN,
                in("x6") SCTLR,
                in("x7") $h.stack_top,
                in("x8") $h.entry,
                in("x9") $regs.hcr,
                in("x10") $regs.cnthctl,
                in("x11") $regs.cptr,
                in("x12") SPSR_EL1H_MASKED,
                options(noreturn),
            )
        };
    }

    /// `SPSR_EL2` for the eret: `EL1h` (EL1 using `SP_EL1`), DAIF masked.
    const SPSR_EL1H_MASKED: u64 = 0x3c5;

    fn current_el() -> u64 {
        let el: u64;
        // SAFETY: reading CurrentEL has no side effects.
        unsafe { asm!("mrs {}, CurrentEL", out(reg) el, options(nomem, nostack)) };
        (el >> 2) & 3
    }

    pub(crate) fn entry_level() -> EntryLevel {
        if current_el() == 2 {
            EntryLevel::El2
        } else {
            EntryLevel::El1
        }
    }

    /// `TCR_EL1` for 48-bit VAs in both halves, 4 KiB granules, write-back
    /// inner-shareable walks, and the CPU's physical address size.
    fn tcr() -> u64 {
        let mmfr0: u64;
        // SAFETY: reading an ID register has no side effects.
        unsafe { asm!("mrs {}, id_aa64mmfr0_el1", out(reg) mmfr0, options(nomem, nostack)) };
        let ips = (mmfr0 & 0xf).min(5); // up to 48-bit PA; 52-bit needs LPA
        let half = 16 | (1 << 8) | (1 << 10) | (3 << 12); // TxSZ=16, IRGN/ORGN WB, SH inner
        half | (half << 16) | (2 << 30) | (ips << 32) // TG1 = 4 KiB
    }

    /// Makes freshly copied code visible to instruction fetch: clean the
    /// data cache to the point of unification, then invalidate the
    /// instruction cache.
    pub(crate) fn prepare_exec(start: u64, len: u64) {
        let ctr: u64;
        // SAFETY: reading CTR_EL0 has no side effects.
        unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
        let line = 4u64 << ((ctr >> 16) & 0xf);
        let mut a = start & !(line - 1);
        while a < start + len {
            // SAFETY: cache maintenance by address on memory we own.
            unsafe { asm!("dc cvau, {}", in(reg) a, options(nostack)) };
            a += line;
        }
        // SAFETY: global I-cache invalidation is always permitted at EL1/EL2.
        unsafe { asm!("dsb ish", "ic iallu", "dsb ish", "isb", options(nostack)) };
    }

    /// Programs the EL1 translation regime and enters the kernel at EL1,
    /// dropping from EL2 first if the firmware ran us there.
    ///
    /// # Safety
    ///
    /// As for amd64: boot services exited, `h` valid, this code
    /// identity-mapped by `h.roots[0]`.
    pub(crate) unsafe fn jump(h: &Handoff) -> ! {
        let (mair, tcr) = (bootinfo::arm64::MAIR, tcr());
        if current_el() == 2 {
            let hcr: u64;
            // SAFETY: reading HCR_EL2 at EL2 has no side effects.
            unsafe { asm!("mrs {}, hcr_el2", out(reg) hcr, options(nomem, nostack)) };
            let regs = El2Regs::new(hcr & HCR_E2H != 0);
            // With E2H=1 (VHE, as AAVMF runs), `*_EL1` names accessed from
            // EL2 are redirected to the EL2 registers, so the EL1 registers
            // must be written through their `*_EL12` aliases. The generic
            // `S3_<op1>_...` encodings avoid needing the assembler's VHE
            // extension.
            if regs.e2h {
                // SAFETY: caller contract.
                unsafe {
                    enter_el1_from_el2!(
                        h,
                        regs,
                        mair,
                        tcr,
                        "S3_5_C10_C2_0",
                        "S3_5_C2_C0_2",
                        "S3_5_C2_C0_0",
                        "S3_5_C2_C0_1",
                        "S3_5_C1_C0_2",
                        "S3_5_C1_C0_0"
                    )
                }
            } else {
                // SAFETY: caller contract.
                unsafe {
                    enter_el1_from_el2!(
                        h,
                        regs,
                        mair,
                        tcr,
                        "mair_el1",
                        "tcr_el1",
                        "ttbr0_el1",
                        "ttbr1_el1",
                        "cpacr_el1",
                        "sctlr_el1"
                    )
                }
            }
        } else {
            // SAFETY: caller contract. The MMU is turned off while running
            // from the identity map (no data accesses in that window), the
            // new regime installed, and the MMU turned back on.
            unsafe {
                asm!(
                    "msr daifset, #0xf",
                    "msr cpacr_el1, x5",
                    "isb",
                    "mrs x9, sctlr_el1",
                    "bic x9, x9, #1",
                    "msr sctlr_el1, x9",
                    "isb",
                    "msr mair_el1, x1",
                    "msr tcr_el1, x2",
                    "msr ttbr0_el1, x3",
                    "msr ttbr1_el1, x4",
                    "isb",
                    "tlbi vmalle1",
                    "dsb ish",
                    "isb",
                    "msr sctlr_el1, x6",
                    "isb",
                    "mov sp, x7",
                    "br x8",
                    in("x1") mair,
                    in("x2") tcr,
                    in("x3") h.roots[0],
                    in("x4") h.roots[1],
                    in("x5") CPACR_FPEN,
                    in("x6") SCTLR,
                    in("x7") h.stack_top,
                    in("x8") h.entry,
                    in("x0") h.info,
                    options(noreturn),
                )
            }
        }
    }
}
