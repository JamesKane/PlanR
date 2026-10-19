//! arm64 support.

pub(crate) mod context;
mod gic;
pub(crate) mod mmu;
pub(crate) mod protect;
mod psci;
mod smp;
mod trap;

pub(crate) use smp::{hwid, send_ipi, start_cpus};
pub(crate) use trap::{CpuLocal, breakpoint, copy_in, copy_out, frame_pointer, probe_read, probe_user_read};

/// What another CPU was doing; arm64 has no NMI to stop one with, so
/// there is never one.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Snapshot {
    pub(crate) pc: u64,
    pub(crate) fp: u64,
}

impl core::fmt::Display for Snapshot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "pc {:#x}", self.pc)
    }
}

/// Would stop another CPU to see what it is doing: not on arm64.
pub(crate) fn snapshot(_: &crate::port::cpu::Cpu, _: u64) -> Option<Snapshot> {
    None
}

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use acpi::{SpcrIrq, UartKind};

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
/// The SGI that would ask a CPU to flush its TLB; unused, since arm64
/// broadcasts its invalidations ([`crate::port::mmu::shootdown`]).
pub(crate) const SHOOTDOWN_VECTOR: usize = 1;
/// The EL1 virtual timer's PPI when the GTDT does not say.
const DEFAULT_TIMER_PPI: u32 = 27;

/// Installs the trap vectors. First thing the kernel does, so even an
/// early fault powers off with a failure instead of resetting.
pub(crate) fn init_traps() {
    trap::init();
}

pub(crate) const NAME: &str = "arm64";

/// Abandons the current stack for the one ending at `top` and calls
/// `f(arg)` there, as the first frame of the backtrace.
pub(crate) fn run_on_stack(top: u64, f: extern "C" fn(usize) -> !, arg: usize) -> ! {
    // SAFETY: `top` is the top of a mapped stack nothing else uses, and
    // `f` never returns, so nothing on the old stack is needed again.
    unsafe {
        asm!(
            "mov sp, {top}",
            "mov x29, xzr",
            "blr {f}",
            "brk #1",
            top = in(reg) top,
            f = in(reg) f,
            in("x0") arg,
            options(noreturn),
        );
    }
}

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
pub(crate) const KTESTS: &[(&str, TestFn)] = &[("cpu: PAN on every CPU", protect::ktests::pan)];

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

/// True if IRQs are unmasked.
pub(crate) fn interrupts_enabled() -> bool {
    let daif: u64;
    // SAFETY: reads DAIF.
    unsafe { asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack)) };
    daif & (1 << 7) == 0
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

/// No default console, so no interrupt for it.
pub(crate) const DEFAULT_CONSOLE_IRQ: Option<SpcrIrq> = None;

/// Installs `handler` for a device interrupt, a level-triggered SPI routed
/// to the boot CPU, and enables it; returns its interrupt ID.
pub(crate) fn enable_irq(irq: SpcrIrq, name: &'static str, handler: intr::Handler) -> Result<usize, &'static str> {
    let SpcrIrq::Gsi(id) = irq else {
        return Err("no 8259 IRQs on arm64");
    };
    if !gic::is_spi(id) {
        return Err("not an SPI");
    }
    intr::register(id as usize, name, handler).map_err(|_| "interrupt ID already in use")?;
    gic::enable_spi(id);
    Ok(id as usize)
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
    if crate::port::config::halt_on_exit() {
        crate::println!("kernel: halted (exit=halt); reset or power-cycle to continue");
        halt();
    }
    // Does not return on success.
    psci::call(SYSTEM_OFF, 0, 0, 0);
    halt()
}

const PSCI_FEATURES: u64 = 0x8400_000a;
const SYSTEM_RESET: u64 = 0x8400_0009;
/// PSCI 1.1 `SYSTEM_RESET2` (SMC64), whose type 0 is an architectural warm
/// reset. Plain `SYSTEM_RESET` may be a cold one, which loses RAM.
const SYSTEM_RESET2: u64 = 0xc400_0012;
const SYSTEM_WARM_RESET: u64 = 0;

fn has_warm_reset() -> bool {
    psci::call(PSCI_FEATURES, SYSTEM_RESET2, 0, 0) >= 0
}

/// How [`reset`] will reset the machine.
pub(crate) fn reset_method(_acpi: Option<&System>) -> &'static str {
    if has_warm_reset() {
        "PSCI SYSTEM_RESET2, warm"
    } else {
        "PSCI SYSTEM_RESET (no SYSTEM_RESET2; warm or cold is the firmware's choice)"
    }
}

/// Resets the machine warm, keeping RAM and so the log ring: PSCI
/// `SYSTEM_RESET2` if the firmware has it, else `SYSTEM_RESET`.
pub(crate) fn reset(_acpi: Option<&System>) -> ! {
    // SAFETY: a barrier; completes the log ring's non-cacheable writes.
    unsafe { asm!("dsb sy", options(nostack)) };
    if has_warm_reset() {
        psci::call(SYSTEM_RESET2, SYSTEM_WARM_RESET, 0, 0);
    }
    psci::call(SYSTEM_RESET, 0, 0, 0);
    halt()
}

/// arm64 has no I/O ports. Reads float high, so a 16550 "at port 0" looks
/// absent and its output is dropped.
pub(crate) fn inb(_port: u16) -> u8 {
    0xff
}

pub(crate) fn outb(_port: u16, _v: u8) {}
