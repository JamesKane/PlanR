//! arm64 support.

mod gic;
pub(crate) mod mmu;
mod psci;
mod smp;
mod trap;

pub(crate) use smp::{hwid, send_ipi, start_cpus};
pub(crate) use trap::{breakpoint, frame_pointer, probe_read};

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use acpi::UartKind;

use super::ExitCode;
use crate::drivers::uart16550::Uart16550;
use crate::port::acpi::System;
use crate::port::config::Config;
use crate::port::console::{ConsoleSource, Uart};
use crate::port::cpu::Cpu;
use crate::port::ktest::TestFn;
use crate::port::{clock, intr};
use crate::println;

/// GIC interrupt IDs: SGIs 0-15, PPIs 16-31, SPIs 32-1019.
pub(crate) const NVEC: usize = 1020;
/// The SGI used by the in-kernel tests' self-IPI.
pub(crate) const TEST_VECTOR: usize = 15;
/// The SGI that wakes a CPU to look for work ([`crate::port::cpu::call`]).
pub(crate) const WAKE_VECTOR: usize = 0;
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

/// Records how to reach PSCI firmware, from the FADT.
pub(crate) fn init(acpi: Option<&System>) {
    psci::init(acpi);
}

/// The timer's PPI and trigger, for every CPU.
static TIMER_PPI: AtomicU32 = AtomicU32::new(DEFAULT_TIMER_PPI);
static TIMER_EDGE: AtomicBool = AtomicBool::new(false);

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
    TIMER_PPI.store(ppi, Ordering::Relaxed);
    TIMER_EDGE.store(edge, Ordering::Relaxed);
    intr::register(ppi as usize, "timer", clock::tick).map_err(|_| "timer PPI busy")?;
    init_local_timer();
    let trigger = if edge { "edge" } else { "level" };
    println!(
        "clock: generic timer {} MHz, virtual timer PPI {ppi} ({trigger})",
        hz / 1_000_000
    );
    Ok(hz)
}

/// Stops the calling CPU's virtual timer until the clock arms it, and
/// enables its interrupt.
fn init_local_timer() {
    // SAFETY: disables the timer.
    unsafe { asm!("msr cntv_ctl_el0, xzr", "isb", options(nostack)) };
    gic::enable_local(TIMER_PPI.load(Ordering::Relaxed), TIMER_EDGE.load(Ordering::Relaxed));
}

/// Installs the handlers for the kernel's own inter-processor interrupts.
pub(crate) fn register_ipis() -> Result<(), &'static str> {
    intr::register(WAKE_VECTOR, "wake", crate::port::cpu::wake).map_err(|_| "wake SGI busy")
}

/// Records the calling CPU's index in `TPIDR_EL1`.
pub(crate) fn set_cpu_local(cpu: &'static Cpu) {
    // SAFETY: TPIDR_EL1 is the kernel's to use; `cpu_index` reads it.
    unsafe { asm!("msr tpidr_el1, {}", in(reg) cpu.index(), options(nomem, nostack)) };
}

/// The calling CPU's index.
pub(crate) fn cpu_index() -> usize {
    let i: usize;
    // SAFETY: reads a system register `set_cpu_local` wrote.
    unsafe { asm!("mrs {}, tpidr_el1", out(reg) i, options(nomem, nostack, preserves_flags)) };
    i
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
    // Does not return on success.
    psci::call(SYSTEM_OFF, 0, 0, 0);
    halt()
}

/// arm64 has no I/O ports. Reads float high, so a 16550 "at port 0" looks
/// absent and its output is dropped.
pub(crate) fn inb(_port: u16) -> u8 {
    0xff
}

pub(crate) fn outb(_port: u16, _v: u8) {}
