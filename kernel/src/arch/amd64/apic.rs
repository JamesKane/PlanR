//! The local APIC (in xAPIC or x2APIC mode) and its timer.
//!
//! x2APIC is used whenever the CPU has it: registers are MSRs, IDs are 32
//! bits, and an ICR write is a single instruction. The timer runs in
//! TSC-deadline mode when available, so a deadline is just a TSC value;
//! otherwise it is a one-shot count calibrated against the TSC.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use acpi::{Madt, MadtEntry};

use super::cpu::{self, rdtsc};
use super::{ERROR_VECTOR, SPURIOUS_VECTOR, TIMER_VECTOR};
use crate::port::mm::{self, MapKind};
use crate::port::{clock, intr};
use crate::println;

const IA32_APIC_BASE: u32 = 0x1b;
const APIC_BASE_X2APIC: u64 = 1 << 10;
const APIC_BASE_ENABLE: u64 = 1 << 11;
const IA32_TSC_DEADLINE: u32 = 0x6e0;

// Register offsets, xAPIC layout; x2APIC MSR = 0x800 + offset / 16.
const ID: u32 = 0x020;
const VERSION: u32 = 0x030;
const TPR: u32 = 0x080;
const EOI: u32 = 0x0b0;
const SVR: u32 = 0x0f0;
const ESR: u32 = 0x280;
const ICR_LOW: u32 = 0x300;
const ICR_HIGH: u32 = 0x310;
const LVT_TIMER: u32 = 0x320;
const LVT_LINT0: u32 = 0x350;
const LVT_LINT1: u32 = 0x360;
const LVT_ERROR: u32 = 0x370;
const TIMER_INITIAL: u32 = 0x380;
const TIMER_CURRENT: u32 = 0x390;
const TIMER_DIVIDE: u32 = 0x3e0;

const SVR_ENABLE: u32 = 1 << 8;
const LVT_MASKED: u32 = 1 << 16;
const LVT_NMI: u32 = 0b100 << 8;
const LVT_LEVEL: u32 = 1 << 15;
const LVT_ACTIVE_LOW: u32 = 1 << 13;
const LVT_TSC_DEADLINE: u32 = 0b10 << 17;
const ICR_PENDING: u32 = 1 << 12;
const ICR_ASSERT: u32 = 1 << 14;
const ICR_SELF: u32 = 0b01 << 18;
const DIVIDE_BY_1: u32 = 0b1011;

/// 0 in x2APIC mode, else the virtual address of the xAPIC registers.
static MMIO: AtomicU64 = AtomicU64::new(0);
static X2APIC: AtomicBool = AtomicBool::new(false);
static TSC_DEADLINE: AtomicBool = AtomicBool::new(false);
/// Local APIC timer ticks per second, for one-shot count mode.
static TIMER_HZ: AtomicU64 = AtomicU64::new(0);
/// TSC ticks per second.
static TSC_HZ: AtomicU64 = AtomicU64::new(0);

#[allow(clippy::cast_possible_truncation)] // x2APIC registers are 32 bits but ICR
fn read(reg: u32) -> u32 {
    if X2APIC.load(Ordering::Relaxed) {
        cpu::rdmsr(0x800 + reg / 16) as u32
    } else {
        let va = MMIO.load(Ordering::Relaxed) + u64::from(reg);
        // SAFETY: `MMIO` maps the local APIC registers.
        unsafe { (va as *const u32).read_volatile() }
    }
}

fn write(reg: u32, v: u32) {
    if X2APIC.load(Ordering::Relaxed) {
        // SAFETY: an x2APIC register write with an architectural layout.
        unsafe { cpu::wrmsr(0x800 + reg / 16, u64::from(v)) };
    } else {
        let va = MMIO.load(Ordering::Relaxed) + u64::from(reg);
        // SAFETY: as in `read`.
        unsafe { (va as *mut u32).write_volatile(v) };
    }
}

/// This CPU's APIC ID.
pub(crate) fn id() -> u32 {
    if X2APIC.load(Ordering::Relaxed) {
        read(ID)
    } else {
        read(ID) >> 24
    }
}

pub(crate) fn eoi() {
    write(EOI, 0);
}

/// Sends an interrupt command; `dest` is an APIC ID, ignored for the
/// self shorthand.
fn send(low: u32, dest: u32) {
    if X2APIC.load(Ordering::Relaxed) {
        // SAFETY: one ICR write sends the IPI.
        unsafe { cpu::wrmsr(0x830, u64::from(dest) << 32 | u64::from(low)) };
    } else {
        while read(ICR_LOW) & ICR_PENDING != 0 {
            core::hint::spin_loop();
        }
        write(ICR_HIGH, dest << 24);
        write(ICR_LOW, low);
    }
}

pub(crate) fn self_ipi(vector: u8) {
    send(u32::from(vector) | ICR_ASSERT | ICR_SELF, 0);
}

fn error() {
    // Writing ESR latches the errors since the last write.
    write(ESR, 0);
    println!("lapic: error, status {:#x}", read(ESR));
}

/// Enables this CPU's local APIC with every local interrupt masked except
/// NMI. `madt` supplies the xAPIC address and the NMI pin. `allow_x2apic`
/// and `allow_deadline` permit those modes where the CPU has them.
pub(crate) fn init(madt: &Madt<'_>, allow_x2apic: bool, allow_deadline: bool) -> Result<(), &'static str> {
    let features = cpu::cpuid(1, 0);
    if features.edx & (1 << 9) == 0 {
        return Err("no local APIC");
    }
    let base = cpu::rdmsr(IA32_APIC_BASE);
    // Firmware may already have switched to x2APIC; there is no way back.
    let x2 = base & APIC_BASE_X2APIC != 0 || (allow_x2apic && features.ecx & (1 << 21) != 0);
    if x2 {
        // xAPIC to x2APIC is a legal transition; the reverse is not.
        // SAFETY: enables the APIC in x2APIC mode; nothing uses it yet.
        unsafe { cpu::wrmsr(IA32_APIC_BASE, base | APIC_BASE_ENABLE | APIC_BASE_X2APIC) };
        X2APIC.store(true, Ordering::Relaxed);
    } else {
        let phys = match madt.local_apic_address() {
            0 => base & 0xf_ffff_f000,
            a => a,
        };
        let va = mm::map_phys(phys, mm::PAGE, MapKind::Device).ok_or("cannot map the local APIC")?;
        MMIO.store(va, Ordering::Relaxed);
        // SAFETY: enables the APIC at its current address.
        unsafe { cpu::wrmsr(IA32_APIC_BASE, base | APIC_BASE_ENABLE) };
    }
    TSC_DEADLINE.store(allow_deadline && features.ecx & (1 << 24) != 0, Ordering::Relaxed);

    write(TPR, 0);
    write(LVT_TIMER, LVT_MASKED);
    write(LVT_LINT0, LVT_MASKED);
    write(LVT_LINT1, nmi_lvt(madt).unwrap_or(LVT_MASKED));
    intr::register(usize::from(ERROR_VECTOR), "lapic error", error).map_err(|_| "error vector busy")?;
    write(LVT_ERROR, u32::from(ERROR_VECTOR));
    write(ESR, 0);
    write(ESR, 0);
    write(SVR, SVR_ENABLE | u32::from(SPURIOUS_VECTOR));
    eoi();

    let mode = if x2 { "x2APIC" } else { "xAPIC" };
    println!("lapic: {mode}, id {}, version {:#x}", id(), read(VERSION) & 0xff);
    Ok(())
}

/// The LINT1 setting from MADT's NMI entries, for any processor (0xff) or
/// this one. Plan R does not yet map ACPI UIDs to APIC IDs, so any entry
/// naming LINT1 counts.
fn nmi_lvt(madt: &Madt<'_>) -> Option<u32> {
    madt.entries().find_map(|e| match e {
        MadtEntry::LocalApicNmi { lint: 1, flags, .. } => {
            let mut v = LVT_NMI;
            if flags.polarity() == acpi::Polarity::Low {
                v |= LVT_ACTIVE_LOW;
            }
            if flags.trigger() == acpi::Trigger::Level {
                v |= LVT_LEVEL;
            }
            Some(v)
        }
        _ => None,
    })
}

/// Where the TSC frequency came from.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TscSource {
    Cpuid,
    Hpet,
}

/// Measures the TSC (and, without TSC-deadline mode, the APIC timer),
/// routes the timer interrupt to the clock, and returns the TSC frequency.
pub(crate) fn init_timer(hpet: Option<crate::drivers::hpet::Hpet>) -> Result<u64, &'static str> {
    let (tsc_hz, source) = match (cpu::tsc_hz_from_cpuid(), hpet) {
        (Some(hz), _) => (hz, TscSource::Cpuid),
        (None, Some(h)) => (calibrate_tsc(h), TscSource::Hpet),
        (None, None) => return Err("no HPET or CPUID frequency to calibrate the TSC against"),
    };
    if tsc_hz == 0 {
        return Err("TSC calibration failed");
    }
    TSC_HZ.store(tsc_hz, Ordering::Relaxed);
    let invariant = if cpu::invariant_tsc() {
        "invariant"
    } else {
        "not invariant"
    };
    println!("clock: TSC {} MHz ({source:?}), {invariant}", tsc_hz / 1_000_000);

    intr::register(usize::from(TIMER_VECTOR), "timer", clock::tick).map_err(|_| "timer vector busy")?;
    if TSC_DEADLINE.load(Ordering::Relaxed) {
        write(LVT_TIMER, u32::from(TIMER_VECTOR) | LVT_TSC_DEADLINE);
        // Intel SDM 10.5.4.1: order the LVT write before deadline writes.
        // SAFETY: fences only.
        unsafe { core::arch::asm!("mfence", "lfence", options(nostack)) };
        println!("lapic: timer in TSC-deadline mode");
    } else {
        write(TIMER_DIVIDE, DIVIDE_BY_1);
        let hz = calibrate_timer(tsc_hz);
        if hz == 0 {
            return Err("local APIC timer does not count");
        }
        TIMER_HZ.store(hz, Ordering::Relaxed);
        write(LVT_TIMER, u32::from(TIMER_VECTOR));
        println!("lapic: timer one-shot at {} MHz", hz / 1_000_000);
    }
    Ok(tsc_hz)
}

/// Counts TSC ticks over 20 ms of HPET time.
fn calibrate_tsc(h: crate::drivers::hpet::Hpet) -> u64 {
    h.enable();
    let want = h.ticks(20_000_000);
    let (c0, t0) = (h.counter(), rdtsc());
    let mut c1 = c0;
    while h.elapsed(c0, c1) < want {
        c1 = h.counter();
    }
    let t1 = rdtsc();
    let elapsed_fs = u128::from(h.elapsed(c0, c1)) * u128::from(h.period_fs());
    u64::try_from(u128::from(t1 - t0) * 1_000_000_000_000_000 / elapsed_fs.max(1)).unwrap_or(0)
}

/// Counts APIC timer ticks over 10 ms of TSC time.
fn calibrate_timer(tsc_hz: u64) -> u64 {
    write(LVT_TIMER, LVT_MASKED);
    write(TIMER_INITIAL, u32::MAX);
    let end = rdtsc() + tsc_hz / 100;
    while rdtsc() < end {
        core::hint::spin_loop();
    }
    let used = u32::MAX - read(TIMER_CURRENT);
    write(TIMER_INITIAL, 0);
    u64::from(used) * 100
}

pub(crate) fn tsc_hz() -> u64 {
    TSC_HZ.load(Ordering::Relaxed)
}

/// Arms the timer to interrupt when the TSC reaches `deadline`.
pub(crate) fn timer_at(deadline: u64) {
    if TSC_DEADLINE.load(Ordering::Relaxed) {
        // SAFETY: arms the deadline timer; a past deadline fires at once.
        unsafe { cpu::wrmsr(IA32_TSC_DEADLINE, deadline) };
        return;
    }
    let left = deadline.saturating_sub(rdtsc());
    let count = u128::from(left) * u128::from(TIMER_HZ.load(Ordering::Relaxed)) / u128::from(tsc_hz().max(1));
    write(TIMER_INITIAL, u32::try_from(count).unwrap_or(u32::MAX).max(1));
}
