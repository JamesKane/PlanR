//! Starting the other CPUs with PSCI `CPU_ON`.
//!
//! Firmware starts each CPU at [`planr_ap_entry`]'s physical address with
//! the MMU off, at EL2 if the machine has it (as for the boot CPU, 9boot's
//! job is done here: EL2 is set up for an EL1 kernel), otherwise at EL1.
//! The entry code is position-independent and takes everything it needs
//! from a parameter block whose physical address arrives in `x0`: the boot
//! CPU's translation registers, the new CPU's stack and index, and the
//! address of [`ap_entry`].

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU64, Ordering};

use acpi::MadtEntry;
use bootinfo::phys_to_virt;

use super::{gic, psci, trap};
use crate::port::acpi::System;
use crate::port::cpu::{self, Census, Cpu};
use crate::port::mm;
use crate::println;

const PSCI_CPU_ON: u64 = 0xc400_0003;
const PSCI_ALREADY_ON: i64 = -4;

/// The parameter block, read with the MMU (and so the data cache) off.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Params {
    mair: u64,
    tcr: u64,
    ttbr0: u64,
    ttbr1: u64,
    sctlr: u64,
    stack: u64,
    entry: u64,
    index: u64,
    /// Written by the new CPU: `CurrentEL` as it started.
    started_el: u64,
    /// Written by the new CPU if it started at EL2: `HCR_EL2.E2H`.
    started_e2h: u64,
}

// x0 = &Params (physical). x19-x22 carry stack, entry and index across the
// EL change; x1-x9 hold the register values.
global_asm!(
    ".section .text.ap, \"ax\"",
    ".balign 4",
    ".global planr_ap_entry",
    "planr_ap_entry:",
    "    msr daifset, #0xf",
    "    ldp x1, x2, [x0]",        // mair, tcr
    "    ldp x3, x4, [x0, #16]",   // ttbr0, ttbr1
    "    ldp x6, x20, [x0, #32]",  // sctlr, stack
    "    ldp x21, x22, [x0, #48]", // entry, index
    "    mov x5, #(3 << 20)",      // CPACR_EL1.FPEN: FP/SIMD at EL1
    "    mrs x9, CurrentEL",
    "    str x9, [x0, #64]", // started_el
    "    cmp x9, #(2 << 2)",
    "    b.ne 3f",
    // EL2: keep the firmware's choice of E2H, as 9boot does; open the GIC
    // system registers and the counters to EL1; then the EL1 registers,
    // through their _EL12 names under VHE.
    "    mrs x9, id_aa64pfr0_el1",
    "    tst x9, #(0xf << 24)",
    "    b.eq 1f",
    "    mrs x9, S3_4_C12_C9_5", // ICC_SRE_EL2
    "    orr x9, x9, #1",        // SRE
    "    orr x9, x9, #8",        // Enable: EL1 may use ICC_SRE_EL1
    "    msr S3_4_C12_C9_5, x9",
    "    isb",
    "    msr S3_4_C12_C11_0, xzr", // ICH_HCR_EL2
    "1:  mrs x9, hcr_el2",
    "    and x9, x9, #(1 << 34)",   // E2H
    "    str x9, [x0, #72]",        // started_e2h
    "    mov x10, #(1 << 31)",      // RW: EL1 is AArch64
    "    orr x10, x10, #(1 << 40)", // APK
    "    orr x10, x10, #(1 << 41)", // API
    "    orr x10, x10, x9",
    "    msr hcr_el2, x10",
    "    isb",
    "    msr cntvoff_el2, xzr",
    "    msr vttbr_el2, xzr",
    // EL1 reads MIDR_EL1/MPIDR_EL1 from VPIDR_EL2/VMPIDR_EL2, which reset
    // to unknown values: the Orange Pi's A520s read garbage affinities.
    "    mrs x10, midr_el1",
    "    msr vpidr_el2, x10",
    "    mrs x10, mpidr_el1",
    "    msr vmpidr_el2, x10",
    "    cbz x9, 2f",
    // VHE: CNTHCTL EL1PCTEN|EL1PTEN; CPTR in CPACR layout, FPEN|ZEN.
    "    mov x10, #(3 << 10)",
    "    msr cnthctl_el2, x10",
    "    mov x10, #(3 << 20)",
    "    orr x10, x10, #(3 << 16)",
    "    msr cptr_el2, x10",
    "    msr S3_5_C10_C2_0, x1", // MAIR_EL12
    "    msr S3_5_C2_C0_2, x2",  // TCR_EL12
    "    msr S3_5_C2_C0_0, x3",  // TTBR0_EL12
    "    msr S3_5_C2_C0_1, x4",  // TTBR1_EL12
    "    msr S3_5_C1_C0_2, x5",  // CPACR_EL12
    "    isb",
    "    tlbi vmalle1",
    "    dsb ish",
    "    isb",
    "    msr S3_5_C1_C0_0, x6", // SCTLR_EL12
    "    b 4f",
    // nVHE: CNTHCTL EL1PCTEN|EL1PCEN; CPTR RES1 bits, no traps.
    "2:  mov x10, #3",
    "    msr cnthctl_el2, x10",
    "    mov x10, #0x33ff",
    "    msr cptr_el2, x10",
    "    msr mair_el1, x1",
    "    msr tcr_el1, x2",
    "    msr ttbr0_el1, x3",
    "    msr ttbr1_el1, x4",
    "    msr cpacr_el1, x5",
    "    isb",
    "    tlbi vmalle1",
    "    dsb ish",
    "    isb",
    "    msr sctlr_el1, x6",
    "4:  msr sp_el1, x20",
    "    msr elr_el2, x21",
    "    mov x10, #0x3c5", // EL1h, DAIF masked
    "    msr spsr_el2, x10",
    "    mov x0, x22",
    "    mov x29, xzr",
    "    mov x30, xzr",
    "    eret",
    // EL1: install the translation regime and turn the MMU on. This code
    // keeps running through the identity map until the branch.
    "3:  msr cpacr_el1, x5",
    "    msr mair_el1, x1",
    "    msr tcr_el1, x2",
    "    msr ttbr0_el1, x3",
    "    msr ttbr1_el1, x4",
    "    isb",
    "    tlbi vmalle1",
    "    dsb ish",
    "    isb",
    "    msr sctlr_el1, x6",
    "    isb",
    "    mov sp, x20",
    "    mov x0, x22",
    "    mov x29, xzr",
    "    mov x30, xzr",
    "    br x21",
    ".global planr_ap_entry_end",
    "planr_ap_entry_end:",
);

unsafe extern "C" {
    static planr_ap_entry: u8;
    static planr_ap_entry_end: u8;
}

fn sysreg_values() -> (u64, u64, u64, u64, u64) {
    let (mair, tcr, ttbr0, ttbr1, sctlr): (u64, u64, u64, u64, u64);
    // SAFETY: reads system registers.
    unsafe {
        asm!(
            "mrs {0}, mair_el1",
            "mrs {1}, tcr_el1",
            "mrs {2}, ttbr0_el1",
            "mrs {3}, ttbr1_el1",
            "mrs {4}, sctlr_el1",
            out(reg) mair,
            out(reg) tcr,
            out(reg) ttbr0,
            out(reg) ttbr1,
            out(reg) sctlr,
            options(nomem, nostack),
        );
    }
    (mair, tcr, ttbr0, ttbr1, sctlr)
}

/// Cleans `[va, va + len)` to the point of coherency, so a CPU running with
/// its MMU and caches off reads what this one wrote.
fn clean_to_poc(va: u64, len: u64) {
    let ctr: u64;
    // SAFETY: reading CTR_EL0 has no side effects.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
    let line = 4u64 << ((ctr >> 16) & 0xf);
    let mut a = va & !(line - 1);
    while a < va + len {
        // SAFETY: cache maintenance on mapped, readable memory.
        unsafe { asm!("dc cvac, {}", in(reg) a, options(nostack)) };
        a += line;
    }
    // SAFETY: a barrier only.
    unsafe { asm!("dsb sy", options(nostack)) };
}

/// Starts every enabled CPU in the MADT other than this one.
pub(crate) fn start_cpus(acpi: Option<&System>) -> Result<Census, &'static str> {
    let madt = acpi.and_then(|s| s.tables.madt()).ok_or("no MADT")?;
    if !psci::available() {
        return Err("no PSCI to start CPUs with");
    }
    let me = gic::mpidr() & AFFINITY;
    let entry_va = (&raw const planr_ap_entry) as u64;
    let entry_len = (&raw const planr_ap_entry_end) as u64 - entry_va;
    clean_to_poc(entry_va, entry_len);
    let entry = mm::kernel_phys(entry_va);

    let mut found = 1;
    let targets = madt.entries().filter_map(|e| match e {
        MadtEntry::Gicc {
            mpidr, enabled: true, ..
        } => Some(mpidr & AFFINITY),
        _ => None,
    });
    for target in targets.filter(|&m| m != me) {
        found += 1;
        if let Err(e) = start(target, entry) {
            println!("smp: CPU {target:#x}: {e}");
        }
    }
    match STARTED_EL.load(Ordering::Relaxed) {
        0 => {}
        2 => println!("smp: firmware starts CPUs at EL2 with E2H clear"),
        0x102 => println!("smp: firmware starts CPUs at EL2 with E2H set"),
        el => println!("smp: firmware starts CPUs at EL{}", el & 3),
    }
    Ok(Census { found })
}

/// The exception level the last CPU started at, and bit 8 for E2H, for
/// the report.
static STARTED_EL: AtomicU64 = AtomicU64::new(0);

/// MPIDR affinity fields: Aff3 and Aff2-Aff0.
const AFFINITY: u64 = 0xff_00ff_ffff;

/// Starts the CPU with affinity `target` at physical address `entry`.
/// Each CPU gets its own parameter page, so one that starts after its
/// timeout still finds its own stack and index.
fn start(target: u64, entry: u64) -> Result<(), &'static str> {
    let cpu = cpu::claim(target).ok_or("too many CPUs")?;
    let stack = mm::alloc_stack(mm::STACK_PAGES).ok_or("no memory for a stack")?;
    trap::prepare(cpu.index())?;
    let block = mm::alloc_frame().ok_or("no memory for the parameter block")?;
    let (mair, tcr, ttbr0, ttbr1, sctlr) = sysreg_values();
    let p = Params {
        mair,
        tcr,
        ttbr0,
        ttbr1,
        sctlr,
        stack,
        entry: ap_entry as *const () as u64,
        index: cpu.index() as u64,
        started_el: 0,
        started_e2h: 0,
    };
    let va = phys_to_virt(block);
    // SAFETY: `block` is a page of ours in the direct map.
    unsafe { (va as *mut Params).write_volatile(p) };
    clean_to_poc(va, size_of::<Params>() as u64);
    match psci::call(PSCI_CPU_ON, target, entry, block) {
        0 => {}
        PSCI_ALREADY_ON => return Err("already on"),
        _ => return Err("CPU_ON failed"),
    }
    if !cpu::wait_online(cpu) {
        return Err("did not come online");
    }
    // The CPU wrote with its caches off: drop any stale line before reading.
    // SAFETY: cache maintenance on our own page; the CPU is done with it.
    unsafe { asm!("dc civac, {}", "dsb sy", in(reg) va + 64, options(nostack)) };
    // SAFETY: as for the write above.
    let p = unsafe { (va as *const Params).read_volatile() };
    STARTED_EL.store(
        (p.started_el >> 2 & 3) | u64::from(p.started_e2h != 0) << 8,
        Ordering::Relaxed,
    );
    Ok(())
}

/// Where the entry code lands, at EL1 with the kernel's translation
/// regime, on the CPU's own stack.
extern "C" fn ap_entry(index: usize) -> ! {
    let Some(cpu) = cpu::get(index) else {
        super::halt();
    };
    // First: a stack overflow finds this CPU's overflow stack by its index.
    super::set_cpu_local(cpu);
    super::init_traps();
    super::protect::init_cpu();
    if let Err(e) = gic::init_cpu() {
        println!("smp: CPU {index}: {e}");
        super::halt();
    }
    super::init_local_timer();
    cpu::ap_main(cpu)
}

/// The calling CPU's hardware ID: its MPIDR affinity.
pub(crate) fn hwid() -> u64 {
    gic::mpidr() & AFFINITY
}

/// Sends `vector` (an SGI) to `cpu`.
pub(crate) fn send_ipi(cpu: &Cpu, vector: usize) {
    if let Ok(id) = u32::try_from(vector) {
        gic::send_sgi(id, cpu.hwid());
    }
}
