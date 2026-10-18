//! arm64 support.

mod gic;
pub(crate) mod mmu;
mod trap;

pub(crate) use trap::{breakpoint, frame_pointer, probe_read};

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU8, Ordering};

use acpi::{PsciConduit, UartKind};

use super::ExitCode;
use crate::drivers::uart16550::Uart16550;
use crate::port::acpi::System;
use crate::port::config::Config;
use crate::port::console::{ConsoleSource, Uart};
use crate::port::ktest::TestFn;
use crate::port::{clock, intr};
use crate::println;

/// GIC interrupt IDs: SGIs 0-15, PPIs 16-31, SPIs 32-1019.
pub(crate) const NVEC: usize = 1020;
/// The SGI used by the in-kernel tests' self-IPI.
pub(crate) const TEST_VECTOR: usize = 15;
/// The EL1 virtual timer's PPI when the GTDT does not say.
const DEFAULT_TIMER_PPI: u32 = 27;

/// Installs the trap vectors. First thing the kernel does, so even an
/// early fault powers off with a failure instead of resetting.
pub(crate) fn init_traps() {
    trap::init();
}

pub(crate) const NAME: &str = "arm64";

// Entry from 9boot: x0 = &BootInfo, sp = top of the boot stack, EL1 with the
// boot page tables live, DAIF masked (ADR 0010).
global_asm!(
    ".section .text.boot, \"ax\"",
    ".global _start",
    "_start:",
    "    mov x29, xzr",
    "    mov x30, xzr",
    "    bl {kmain}",
    "1:  wfe",
    "    b 1b",
    kmain = sym crate::kmain,
);

const PSCI_NONE: u8 = 0;
const PSCI_SMC: u8 = 1;
const PSCI_HVC: u8 = 2;
static PSCI: AtomicU8 = AtomicU8::new(PSCI_NONE);

/// Records how to reach PSCI firmware, from the FADT.
pub(crate) fn init(acpi: Option<&System>) {
    let c = match acpi.and_then(|s| s.tables.fadt()).and_then(|f| f.psci()) {
        Some(PsciConduit::Smc) => PSCI_SMC,
        Some(PsciConduit::Hvc) => PSCI_HVC,
        None => PSCI_NONE,
    };
    PSCI.store(c, Ordering::Relaxed);
}

/// Brings up the GIC and the EL1 virtual timer and routes the timer to the
/// clock. Returns the counter (`CNTVCT_EL0`) frequency.
pub(crate) fn init_interrupts(acpi: Option<&System>, _config: &Config<'_>) -> Result<u64, &'static str> {
    let madt = acpi
        .and_then(|s| s.tables.madt())
        .ok_or("no MADT (ACPI is required, ADR 0003)")?;
    gic::init(&madt)?;
    let timer = acpi.and_then(|s| s.tables.gtdt()).map(|g| g.virtual_el1);
    let (ppi, edge) = match timer {
        Some(t) if (16..32).contains(&t.gsiv) => (t.gsiv, t.edge),
        _ => (DEFAULT_TIMER_PPI, false),
    };
    let hz = counter_frequency();
    if hz == 0 {
        return Err("CNTFRQ_EL0 is zero (firmware did not set the counter frequency)");
    }
    // SAFETY: disables the timer until the clock arms it.
    unsafe { asm!("msr cntv_ctl_el0, xzr", "isb", options(nostack)) };
    intr::register(ppi as usize, "timer", clock::tick).map_err(|_| "timer PPI busy")?;
    gic::enable_local(ppi, edge);
    let trigger = if edge { "edge" } else { "level" };
    println!(
        "clock: generic timer {} MHz, virtual timer PPI {ppi} ({trigger})",
        hz / 1_000_000
    );
    Ok(hz)
}

fn counter_frequency() -> u64 {
    let v: u64;
    // SAFETY: reads a system register.
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) v, options(nomem, nostack)) };
    v & 0xffff_ffff
}

/// The clock counter: the EL1 virtual count. `isb` keeps it from being
/// read early.
pub(crate) fn counter() -> u64 {
    let v: u64;
    // SAFETY: reads a system register.
    unsafe { asm!("isb", "mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack)) };
    v
}

/// Arms the virtual timer for when [`counter`] reaches `deadline`.
pub(crate) fn timer_at(deadline: u64) {
    // SAFETY: sets the compare value, then enables the timer unmasked; a
    // past deadline fires at once.
    unsafe {
        asm!(
            "msr cntv_cval_el0, {d}",
            "msr cntv_ctl_el0, {on}",
            "isb",
            d = in(reg) deadline,
            on = in(reg) 1u64,
            options(nostack),
        );
    }
}

/// Called by the trap handler for an IRQ.
pub(super) fn interrupt() -> bool {
    gic::irq()
}

pub(crate) fn self_ipi(vector: usize) {
    if let Ok(id) = u32::try_from(vector) {
        gic::self_sgi(id);
    }
}

/// arm64 has no tests of its own yet.
pub(crate) const KTESTS: &[(&str, TestFn)] = &[];

/// A saved interrupt state (`DAIF`), from [`splhi`].
#[derive(Debug, Clone, Copy)]
#[must_use]
pub(crate) struct Pl(u64);

/// Masks IRQs and returns the previous state for [`splx`].
pub(crate) fn splhi() -> Pl {
    let daif: u64;
    // SAFETY: saves DAIF and masks IRQ.
    unsafe { asm!("mrs {}, daif", "msr daifset, #2", out(reg) daif, options(nomem, nostack)) };
    Pl(daif)
}

/// Restores the interrupt state saved by [`splhi`].
pub(crate) fn splx(pl: Pl) {
    // SAFETY: restores a DAIF value read by `splhi`.
    unsafe { asm!("msr daif, {}", in(reg) pl.0, options(nomem, nostack)) };
}

/// Unmasks IRQs.
pub(crate) fn spllo() {
    // SAFETY: the vectors and the GIC are set up.
    unsafe { asm!("msr daifclr, #2", options(nomem, nostack)) };
}

/// Waits for an interrupt and returns with IRQs unmasked. `wfi` wakes on
/// a pending interrupt even while masked, so masking first closes the
/// window where one could arrive just before the `wfi`. Not for use with a
/// lock held.
pub(crate) fn idle() {
    // SAFETY: as for `spllo`.
    unsafe {
        asm!(
            "msr daifset, #2",
            "wfi",
            "msr daifclr, #2",
            "isb",
            options(nomem, nostack)
        );
    }
}

pub(crate) fn current_el() -> u8 {
    let el: u64;
    // SAFETY: reading CurrentEL has no side effects.
    unsafe { asm!("mrs {}, CurrentEL", out(reg) el, options(nomem, nostack)) };
    ((el >> 2) & 3) as u8
}

/// No conventional default on arm64: without SPCR or DBG2 there is no
/// console. A dead UART keeps the rest of the code uniform.
pub(crate) fn default_console() -> (Uart, ConsoleSource) {
    (
        Uart::Ns16550(Uart16550::io(0)),
        ConsoleSource::Default(UartKind::Other(0xffff)),
    )
}

pub(crate) fn halt() -> ! {
    loop {
        // SAFETY: waits for an interrupt; DAIF is masked, so this idles.
        unsafe { asm!("wfi", options(nomem, nostack)) };
    }
}

/// PSCI `SYSTEM_OFF`. QEMU exits with status 0 either way; the test harness
/// tells success from failure by the console markers.
pub(crate) fn exit(_code: ExitCode) -> ! {
    const SYSTEM_OFF: u64 = 0x8400_0008;
    match PSCI.load(Ordering::Relaxed) {
        // SAFETY: SYSTEM_OFF does not return on success; on failure the
        // firmware returns an error code in x0, which we ignore.
        PSCI_SMC => unsafe { asm!("smc #0", inout("x0") SYSTEM_OFF => _, options(nomem, nostack)) },
        // SAFETY: as above.
        PSCI_HVC => unsafe { asm!("hvc #0", inout("x0") SYSTEM_OFF => _, options(nomem, nostack)) },
        _ => {}
    }
    halt()
}

/// arm64 has no I/O ports. Reads float high, so a 16550 "at port 0" looks
/// absent and its output is dropped.
pub(crate) fn inb(_port: u16) -> u8 {
    0xff
}

pub(crate) fn outb(_port: u16, _v: u8) {}
