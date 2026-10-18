//! amd64 support.

mod apic;
mod cpu;
mod ioapic;
pub(crate) mod mmu;
mod smp;
mod trap;

pub(crate) use smp::{hwid, send_ipi, start_cpus};
pub(crate) use trap::{breakpoint, frame_pointer, probe_read};

use core::arch::{asm, global_asm};

use acpi::UartKind;

use super::ExitCode;
use crate::drivers::hpet::Hpet;
use crate::drivers::uart16550::Uart16550;
use crate::port::acpi::System;
use crate::port::config::Config;
use crate::port::console::{ConsoleSource, Uart};
use crate::port::cpu::Cpu;
use crate::port::intr;
use crate::port::ktest::TestFn;
use crate::port::lock::SpinLock;
use crate::port::mm::{self, MapKind};
use crate::println;

/// IDT vectors: 0-31 exceptions, 32-47 the (masked) 8259s, then GSIs from
/// the I/O APICs, then the local APIC's own interrupts.
pub(crate) const NVEC: usize = 256;
const PIC_VECTOR_BASE: u8 = 0x20;
pub(crate) const GSI_VECTOR_BASE: u8 = 0x30;
pub(crate) const TIMER_VECTOR: u8 = 0xf0;
/// Used by the in-kernel tests' self-IPI.
pub(crate) const TEST_VECTOR: usize = 0xf1;
/// Wakes a CPU to look for work ([`crate::port::cpu::call`]).
pub(crate) const WAKE_VECTOR: usize = 0xf2;
pub(crate) const ERROR_VECTOR: u8 = 0xfe;
pub(crate) const SPURIOUS_VECTOR: u8 = 0xff;

static HPET: SpinLock<Option<Hpet>> = SpinLock::new(None);

/// Installs the trap vectors. First thing the kernel does, so even an
/// early fault powers off with a failure instead of resetting.
pub(crate) fn init_traps() {
    trap::init();
}

pub(crate) const NAME: &str = "amd64";

// Entry from 9boot: rdi = &BootInfo, rsp = top of the boot stack, boot page
// tables live, interrupts off (ADR 0010).
global_asm!(
    ".section .text.boot, \"ax\"",
    ".global _start",
    "_start:",
    "    and rsp, -16",
    "    xor ebp, ebp",
    "    call {kmain}",
    "2:  cli",
    "    hlt",
    "    jmp 2b",
    kmain = sym crate::kmain,
);

pub(crate) fn init(_acpi: Option<&System>) {}

/// Takes over interrupt delivery from the firmware: masks the 8259s,
/// enables the local APIC, masks every I/O APIC input, and starts the
/// timer. Returns the clock counter's (the TSC's) frequency.
pub(crate) fn init_interrupts(acpi: Option<&System>, config: &Config<'_>) -> Result<u64, &'static str> {
    let madt = acpi
        .and_then(|s| s.tables.madt())
        .ok_or("no MADT (ACPI is required, ADR 0003)")?;
    if madt.has_8259() {
        disable_8259();
    }
    apic::init(&madt, !config.is("apic", "xapic"), !config.is("apictimer", "oneshot"))?;
    ioapic::init(&madt)?;
    let hpet = acpi.and_then(|s| s.tables.hpet()).and_then(|t| {
        let va = mm::map_phys(t.base.address, mm::PAGE, MapKind::Device)?;
        let h = Hpet::new(va)?;
        println!(
            "hpet: at {:#x}, {} MHz, {} comparators",
            t.base.address,
            h.frequency() / 1_000_000,
            h.comparators()
        );
        Some(h)
    });
    *HPET.lock() = hpet;
    apic::init_timer(hpet)
}

/// Remaps the 8259s clear of the exception vectors and masks every input.
/// A spurious IRQ 7 or 15 can still arrive at `PIC_VECTOR_BASE + 7 or 15`.
fn disable_8259() {
    const ICW1_INIT: u8 = 0x11;
    for (cmd, data, base, wiring) in [(0x20, 0x21, PIC_VECTOR_BASE, 4), (0xa0, 0xa1, PIC_VECTOR_BASE + 8, 2)] {
        outb(cmd, ICW1_INIT);
        outb(data, base);
        outb(data, wiring); // master: slave on IRQ 2; slave: cascade identity 2
        outb(data, 0x01); // 8086 mode
        outb(data, 0xff); // mask everything
    }
}

/// Called by the trap handler for vectors 32-255.
pub(super) fn interrupt(vector: u8) -> bool {
    match vector {
        SPURIOUS_VECTOR => true, // no EOI for a spurious interrupt
        v if (PIC_VECTOR_BASE..GSI_VECTOR_BASE).contains(&v) => {
            // A spurious 8259 interrupt: the inputs are all masked.
            true
        }
        v => {
            let handled = intr::dispatch(usize::from(v));
            apic::eoi();
            handled
        }
    }
}

/// Installs the handlers for the kernel's own inter-processor interrupts.
pub(crate) fn register_ipis() -> Result<(), &'static str> {
    intr::register(WAKE_VECTOR, "wake", crate::port::cpu::wake).map_err(|_| "wake vector busy")
}

const IA32_GS_BASE: u32 = 0xc000_0101;

/// Points the calling CPU's `GS` base at its [`Cpu`].
pub(crate) fn set_cpu_local(cpu: &'static Cpu) {
    // SAFETY: the kernel does not otherwise use GS; `cpu_index` reads
    // through it.
    unsafe { cpu::wrmsr(IA32_GS_BASE, (&raw const *cpu) as u64) };
}

/// The calling CPU's index, the first field of its [`Cpu`].
pub(crate) fn cpu_index() -> usize {
    let i: usize;
    // SAFETY: `set_cpu_local` pointed GS at this CPU's `Cpu`.
    unsafe { asm!("mov {}, gs:[0]", out(reg) i, options(nostack, readonly, preserves_flags)) };
    i
}

pub(crate) fn self_ipi(vector: usize) {
    if let Ok(v) = u8::try_from(vector) {
        apic::self_ipi(v);
    }
}

/// The clock counter: the TSC.
pub(crate) fn counter() -> u64 {
    cpu::rdtsc()
}

/// Arms the timer for when [`counter`] reaches `deadline`.
pub(crate) fn timer_at(deadline: u64) {
    apic::timer_at(deadline);
}

/// A saved interrupt state (`RFLAGS`), from [`splhi`].
#[derive(Debug, Clone, Copy)]
#[must_use]
pub(crate) struct Pl(u64);

const RFLAGS_IF: u64 = 1 << 9;

/// Disables interrupts and returns the previous state for [`splx`].
pub(crate) fn splhi() -> Pl {
    let flags: u64;
    // SAFETY: saves RFLAGS and clears IF; the stack use is balanced.
    unsafe { asm!("pushfq", "pop {}", "cli", out(reg) flags, options(nomem, preserves_flags)) };
    Pl(flags)
}

/// Restores the interrupt state saved by [`splhi`].
pub(crate) fn splx(pl: Pl) {
    if pl.0 & RFLAGS_IF != 0 {
        // SAFETY: re-enables interrupts that were on before `splhi`.
        unsafe { asm!("sti", options(nomem, nostack)) };
    }
}

/// Enables interrupts.
pub(crate) fn spllo() {
    // SAFETY: the trap vectors and the local APIC are set up.
    unsafe { asm!("sti", options(nomem, nostack)) };
}

/// Waits for an interrupt with interrupts enabled, and returns with them
/// still enabled. `sti` holds off interrupts for one instruction, so one
/// arriving just before the `hlt` still wakes it. Not for use with a lock
/// held.
pub(crate) fn idle() {
    // SAFETY: as for `spllo`.
    unsafe { asm!("sti", "hlt", options(nomem, nostack)) };
}

/// amd64 in-kernel tests, run after the portable ones.
pub(crate) const KTESTS: &[(&str, TestFn)] = &[("ioapic: HPET interrupt delivered", hpet_through_ioapic)];

/// An HPET comparator raises an edge on an I/O APIC input, which must
/// arrive at the GSI's vector: the external interrupt path end to end.
fn hpet_through_ioapic() -> crate::port::ktest::TestResult {
    use crate::port::ktest::Fail;
    use core::sync::atomic::{AtomicU64, Ordering};

    static HITS: AtomicU64 = AtomicU64::new(0);
    fn hit() {
        HITS.fetch_add(1, Ordering::Relaxed);
    }

    let h = (*HPET.lock()).ok_or(Fail::Skipped("no HPET"))?;
    // Prefer inputs above the ISA range: nothing else is wired there.
    let (n, gsi) = (0..h.comparators())
        .flat_map(|n| {
            let cap = h.route_capability(n);
            (0..32).rev().filter(move |b| cap & (1 << b) != 0).map(move |b| (n, b))
        })
        .find(|&(_, gsi)| gsi >= 16 && ioapic::exists(gsi) && ioapic::vector(gsi).is_some())
        .ok_or(Fail::Skipped("no comparator routable above the ISA range"))?;
    let vector = usize::from(ioapic::vector(gsi).ok_or("no vector")?);
    intr::register(vector, "ktest hpet", hit).map_err(|_| "GSI vector busy")?;
    ioapic::route(gsi, acpi::Polarity::High, acpi::Trigger::Edge, apic::id())?;
    let before = HITS.load(Ordering::Relaxed);
    h.arm(n, gsi, h.counter().wrapping_add(h.ticks(1_000_000)));
    let ok = crate::port::clock::wait_until(|| HITS.load(Ordering::Relaxed) > before, 1_000_000_000);
    h.disarm(n);
    ioapic::mask(gsi);
    if ok {
        println!("ktest: HPET comparator {n} via GSI {gsi}, vector {vector:#x}");
        Ok(())
    } else {
        Err("no interrupt within 1 s".into())
    }
}

/// No exception levels on amd64.
pub(crate) fn current_el() -> u8 {
    0
}

/// COM1, the conventional PC serial port. Harmless if absent.
pub(crate) fn default_console() -> (Uart, ConsoleSource) {
    (
        Uart::Ns16550(Uart16550::io(0x3f8)),
        ConsoleSource::Default(UartKind::Ns16550),
    )
}

pub(crate) fn halt() -> ! {
    loop {
        // SAFETY: stops this CPU until an interrupt; interrupts are off.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

/// QEMU's `isa-debug-exit` device (port 0xf4) makes QEMU exit with status
/// `(value << 1) | 1`: 99 for success, 97 for failure. On real hardware the
/// write goes nowhere and we halt.
pub(crate) fn exit(code: ExitCode) -> ! {
    let v: u32 = match code {
        ExitCode::Success => 0x31,
        ExitCode::Failure => 0x30,
    };
    // SAFETY: a write to an unused I/O port has no effect.
    unsafe { asm!("out dx, eax", in("dx") 0xf4u16, in("eax") v, options(nomem, nostack)) };
    halt()
}

pub(crate) fn inb(port: u16) -> u8 {
    let v: u8;
    // SAFETY: port I/O to a device the caller owns.
    unsafe { asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack)) };
    v
}

pub(crate) fn outb(port: u16, v: u8) {
    // SAFETY: port I/O to a device the caller owns.
    unsafe { asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack)) };
}
