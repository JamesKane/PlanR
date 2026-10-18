//! GICv3/v4 interrupt controller: the distributor, each CPU's
//! redistributor, and the system-register CPU interface.
//!
//! Interrupts are Group 1 Non-secure, all at one priority, and SPIs are
//! routed to the boot CPU. SGIs are always enabled; PPIs and SPIs are
//! enabled by whoever handles them. Interrupt IDs are the vector numbers the
//! portable code sees: SGIs 0-15, PPIs 16-31, SPIs 32-1019. LPIs (and the
//! ITS) come with `PCIe` MSI.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use acpi::{Madt, MadtEntry};

use crate::port::cpu::{self, MAXCPU};
use crate::port::intr;
use crate::port::mm::{self, MapKind};
use crate::println;

// Distributor registers.
const GICD_CTLR: u64 = 0x0000;
const GICD_TYPER: u64 = 0x0004;
const GICD_IGROUPR: u64 = 0x0080;
const GICD_ICENABLER: u64 = 0x0180;
const GICD_ICPENDR: u64 = 0x0280;
const GICD_IPRIORITYR: u64 = 0x0400;
const GICD_ICFGR: u64 = 0x0c00;
const GICD_IROUTER: u64 = 0x6000;
const GICD_PIDR2: u64 = 0xffe8;
const GICD_CTLR_RWP: u32 = 1 << 31;
/// `EnableGrp1NS` (or `EnableGrp1` with one security state) and `ARE_NS`
/// (or `ARE`): the bit positions agree.
const GICD_CTLR_ENABLE: u32 = (1 << 1) | (1 << 4);
const GICD_BYTES: u64 = 0x1_0000;

// Redistributor: an RD frame, then an SGI frame 64 KiB on.
const GICR_TYPER: u64 = 0x0008;
const GICR_WAKER: u64 = 0x0014;
const GICR_TYPER_VLPIS: u64 = 1 << 1;
const GICR_TYPER_LAST: u64 = 1 << 4;
const GICR_WAKER_SLEEP: u32 = 1 << 1;
const GICR_WAKER_ASLEEP: u32 = 1 << 2;
const SGI_FRAME: u64 = 0x1_0000;
const GICR_IGROUPR0: u64 = SGI_FRAME + 0x0080;
const GICR_ISENABLER0: u64 = SGI_FRAME + 0x0100;
const GICR_ICENABLER0: u64 = SGI_FRAME + 0x0180;
const GICR_ICPENDR0: u64 = SGI_FRAME + 0x0280;
const GICR_IPRIORITYR: u64 = SGI_FRAME + 0x0400;
const GICR_ICFGR1: u64 = SGI_FRAME + 0x0c04;

/// The one priority every interrupt gets: mid-range, so `ICC_PMR_EL1` at
/// 0xff admits it and there is room above for an NMI-like class later.
const PRIORITY: u8 = 0xa0;
const SPURIOUS: u32 = 1020;

static GICD: AtomicU64 = AtomicU64::new(0);
/// Each CPU's redistributor (RD frame), by CPU index.
static GICR: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];

/// The mapped redistributor regions, as (virtual address, length).
const MAX_REGIONS: usize = 16;
static REGIONS: [(AtomicU64, AtomicU64); MAX_REGIONS] = [const { (AtomicU64::new(0), AtomicU64::new(0)) }; MAX_REGIONS];
static NREGIONS: AtomicUsize = AtomicUsize::new(0);

fn rd32(addr: u64) -> u32 {
    // SAFETY: callers pass addresses inside the mapped GIC frames.
    unsafe { (addr as *const u32).read_volatile() }
}

fn wr32(addr: u64, v: u32) {
    // SAFETY: as in `rd32`.
    unsafe { (addr as *mut u32).write_volatile(v) }
}

fn rd64(addr: u64) -> u64 {
    // SAFETY: as in `rd32`.
    unsafe { (addr as *const u64).read_volatile() }
}

fn wr64(addr: u64, v: u64) {
    // SAFETY: as in `rd32`.
    unsafe { (addr as *mut u64).write_volatile(v) }
}

fn gicd() -> u64 {
    GICD.load(Ordering::Relaxed)
}

fn gicr() -> u64 {
    GICR[cpu::current().index()].load(Ordering::Relaxed)
}

fn wait_rwp() {
    for _ in 0..1_000_000 {
        if rd32(gicd() + GICD_CTLR) & GICD_CTLR_RWP == 0 {
            return;
        }
        core::hint::spin_loop();
    }
}

pub(crate) fn mpidr() -> u64 {
    let v: u64;
    // SAFETY: reads an ID register.
    unsafe { asm!("mrs {}, mpidr_el1", out(reg) v, options(nomem, nostack)) };
    v
}

/// Affinity as `GICR_TYPER[63:32]` packs it: Aff3.Aff2.Aff1.Aff0.
fn packed_affinity(mpidr: u64) -> u32 {
    ((mpidr >> 8) & 0xff00_0000) as u32 | (mpidr & 0x00ff_ffff) as u32
}

/// Finds the redistributor for affinity `aff` in a mapped region.
fn find_redistributor(va: u64, len: u64, aff: u32) -> Option<u64> {
    let mut off = 0;
    while off + 2 * SGI_FRAME <= len {
        let typer = rd64(va + off + GICR_TYPER);
        if (typer >> 32) as u32 == aff {
            return Some(va + off);
        }
        if typer & GICR_TYPER_LAST != 0 {
            break;
        }
        // GICv4 adds two frames for virtual LPIs.
        off += if typer & GICR_TYPER_VLPIS != 0 { 4 } else { 2 } * SGI_FRAME;
    }
    None
}

/// GIC-600/700 `GICR_PWRR` (implementation defined): bit 0 asks for the
/// redistributor to be powered down, bit 1 reports that it is.
const GICR_PWRR: u64 = 0x0024;

/// Prints what each redistributor frame's `GICR_TYPER` and `GICR_PWRR` read
/// as, every 128 KiB (a v3 frame; v4 frames are twice that), for
/// diagnosing a CPU whose redistributor the walk did not find.
fn dump_redistributors(aff: u32) {
    crate::println!("gic: no redistributor with affinity {aff:#x}; TYPER/PWRR every 128 KiB:");
    for (va, len) in &REGIONS[..NREGIONS.load(Ordering::Acquire)] {
        let (va, len) = (va.load(Ordering::Relaxed), len.load(Ordering::Relaxed));
        let mut off = 0;
        while off + 2 * SGI_FRAME <= len {
            crate::print!(
                " {:#x}:{:x}/{:x}",
                off,
                rd64(va + off + GICR_TYPER),
                rd32(va + off + GICR_PWRR)
            );
            off += 2 * SGI_FRAME;
        }
        crate::println!();
    }
}

/// Maps the redistributor regions: the MADT's GICR entries, or failing
/// those, each GICC's own redistributor.
fn map_redistributors(madt: &Madt<'_>) -> Result<(), &'static str> {
    let regions = madt.entries().filter_map(|e| match e {
        MadtEntry::Gicr { base, length } => Some((base, u64::from(length))),
        _ => None,
    });
    let per_cpu = madt.entries().filter_map(|e| match e {
        MadtEntry::Gicc { gicr_base, enabled, .. } if gicr_base != 0 && enabled => Some((gicr_base, 2 * SGI_FRAME)),
        _ => None,
    });
    let mut n = 0;
    for (base, len) in regions {
        add_region(n, base, len)?;
        n += 1;
    }
    if n == 0 {
        for (base, len) in per_cpu {
            add_region(n, base, len)?;
            n += 1;
        }
    }
    NREGIONS.store(n, Ordering::Release);
    Ok(())
}

fn add_region(n: usize, base: u64, len: u64) -> Result<(), &'static str> {
    let slot = REGIONS.get(n).ok_or("too many redistributor regions")?;
    let va = mm::map_phys(base, len, MapKind::Device).ok_or("cannot map a redistributor")?;
    slot.0.store(va, Ordering::Relaxed);
    slot.1.store(len, Ordering::Relaxed);
    Ok(())
}

/// How long a CPU waits for its redistributor to appear.
const REDISTRIBUTOR_WAIT_MS: u64 = 100;

/// Finds and records the calling CPU's redistributor. The firmware may
/// still be powering it up when the CPU gets here (the Orange Pi's A520
/// cluster), so the walk is retried for a while before giving up.
fn attach_redistributor() -> Result<(), &'static str> {
    let aff = packed_affinity(mpidr());
    let find = || {
        REGIONS[..NREGIONS.load(Ordering::Acquire)]
            .iter()
            .find_map(|(va, len)| find_redistributor(va.load(Ordering::Relaxed), len.load(Ordering::Relaxed), aff))
    };
    let deadline = super::counter() + super::counter_frequency() * REDISTRIBUTOR_WAIT_MS / 1000;
    let rd = loop {
        if let Some(rd) = find() {
            break rd;
        }
        if super::counter() > deadline {
            dump_redistributors(aff);
            return Err("no redistributor for this CPU");
        }
        core::hint::spin_loop();
    };
    GICR[cpu::current().index()].store(rd, Ordering::Relaxed);
    Ok(())
}

/// Brings up the distributor, the boot CPU's redistributor and its CPU
/// interface, with every interrupt disabled.
pub(crate) fn init(madt: &Madt<'_>) -> Result<(), &'static str> {
    let (dist, version) = madt
        .entries()
        .find_map(|e| match e {
            MadtEntry::Gicd { base, version, .. } => Some((base, version)),
            _ => None,
        })
        .ok_or("no GIC distributor in MADT")?;
    let va = mm::map_phys(dist, GICD_BYTES, MapKind::Device).ok_or("cannot map the GIC distributor")?;
    GICD.store(va, Ordering::Relaxed);
    let arch_rev = (rd32(va + GICD_PIDR2) >> 4) & 0xf;
    let version = if version == 0 { arch_rev as u8 } else { version };
    if version < 3 {
        return Err("GICv2 is not supported (ADR 0003 requires GICv3)");
    }
    map_redistributors(madt)?;
    let me = mpidr();
    init_distributor(me);
    init_cpu()?;
    let lines = 32 * ((rd32(va + GICD_TYPER) & 0x1f) + 1);
    println!(
        "gic: v{version} distributor at {dist:#x}, {} SPIs; cpu affinity {:#x}",
        lines - 32,
        packed_affinity(me)
    );
    Ok(())
}

/// Brings up the calling CPU's redistributor and CPU interface.
pub(crate) fn init_cpu() -> Result<(), &'static str> {
    attach_redistributor()?;
    init_redistributor()?;
    init_cpu_interface()
}

fn init_distributor(me: u64) {
    let d = gicd();
    wr32(d + GICD_CTLR, 0);
    wait_rwp();
    let lines = 32 * ((rd32(d + GICD_TYPER) & 0x1f) + 1);
    let lines = lines.min(1020);
    // SPIs: Group 1, disabled, not pending, level, one priority, to us.
    for i in (32..lines).step_by(32) {
        let o = u64::from(i / 32) * 4;
        wr32(d + GICD_IGROUPR + o, u32::MAX);
        wr32(d + GICD_ICENABLER + o, u32::MAX);
        wr32(d + GICD_ICPENDR + o, u32::MAX);
    }
    for i in (32..lines).step_by(16) {
        wr32(d + GICD_ICFGR + u64::from(i / 16) * 4, 0);
    }
    for i in (32..lines).step_by(4) {
        wr32(d + GICD_IPRIORITYR + u64::from(i), u32::from_ne_bytes([PRIORITY; 4]));
    }
    for i in 32..lines {
        wr64(d + GICD_IROUTER + u64::from(i) * 8, me & 0xff_00ff_ffff);
    }
    wait_rwp();
    wr32(d + GICD_CTLR, GICD_CTLR_ENABLE);
    wait_rwp();
}

fn init_redistributor() -> Result<(), &'static str> {
    let r = gicr();
    wr32(r + GICR_WAKER, rd32(r + GICR_WAKER) & !GICR_WAKER_SLEEP);
    let mut awake = false;
    for _ in 0..1_000_000 {
        if rd32(r + GICR_WAKER) & GICR_WAKER_ASLEEP == 0 {
            awake = true;
            break;
        }
        core::hint::spin_loop();
    }
    if !awake {
        return Err("redistributor did not wake");
    }
    // SGIs and PPIs: Group 1, disabled, not pending, one priority.
    wr32(r + GICR_IGROUPR0, u32::MAX);
    wr32(r + GICR_ICENABLER0, u32::MAX);
    wr32(r + GICR_ICPENDR0, u32::MAX);
    for i in (0..32).step_by(4) {
        wr32(r + GICR_IPRIORITYR + i, u32::from_ne_bytes([PRIORITY; 4]));
    }
    // SGIs are the kernel's own inter-processor interrupts: always on. One
    // arriving without a handler is a kernel bug and is reported as such.
    wr32(r + GICR_ISENABLER0, 0xffff);
    Ok(())
}

fn init_cpu_interface() -> Result<(), &'static str> {
    let sre: u64;
    // SAFETY: enables the system-register interface (SRE); with firmware
    // at EL2 this needs ICC_SRE_EL2.Enable, which 9boot sets.
    unsafe {
        asm!(
            "mrs {t}, icc_sre_el1",
            "orr {t}, {t}, #1",
            "msr icc_sre_el1, {t}",
            "isb",
            "mrs {t}, icc_sre_el1",
            t = out(reg) sre,
            options(nostack),
        );
    }
    if sre & 1 == 0 {
        return Err("GIC system-register interface disabled by a higher EL");
    }
    // SAFETY: priority mask admits everything, no preemption grouping,
    // EOI both drops priority and deactivates, Group 1 enabled.
    unsafe {
        asm!(
            "msr icc_pmr_el1, {pmr}",
            "msr icc_bpr1_el1, xzr",
            "msr icc_ctlr_el1, xzr",
            "msr icc_igrpen1_el1, {one}",
            "isb",
            pmr = in(reg) 0xffu64,
            one = in(reg) 1u64,
            options(nostack),
        );
    }
    Ok(())
}

/// Enables SGI or PPI `id` on this CPU; `edge` selects edge triggering
/// (PPIs only; SGIs are always edge).
pub(crate) fn enable_local(id: u32, edge: bool) {
    let r = gicr();
    if (16..32).contains(&id) {
        let shift = 2 * (id - 16) + 1;
        let cfg = rd32(r + GICR_ICFGR1);
        wr32(r + GICR_ICFGR1, (cfg & !(1 << shift)) | (u32::from(edge) << shift));
    }
    wr32(r + GICR_ISENABLER0, 1 << id);
}

/// Sends SGI `id` to this CPU.
pub(crate) fn self_sgi(id: u32) {
    send_sgi(id, mpidr());
}

/// Sends SGI `id` to the CPU with affinity `m` (an MPIDR value).
pub(crate) fn send_sgi(id: u32, m: u64) {
    let aff0 = m & 0xff;
    let target = 1u64 << (aff0 % 16);
    let rs = aff0 / 16;
    let v = target
        | ((m >> 8) & 0xff) << 16
        | u64::from(id & 0xf) << 24
        | ((m >> 16) & 0xff) << 32
        | rs << 44
        | ((m >> 32) & 0xff) << 48;
    // SAFETY: generates an SGI; isb makes it visible before we continue.
    unsafe { asm!("msr icc_sgi1r_el1, {}", "isb", in(reg) v, options(nostack)) };
}

/// Handles one IRQ: acknowledge, dispatch, end. False if nothing was
/// registered for it.
pub(crate) fn irq() -> bool {
    let iar: u64;
    // SAFETY: acknowledging marks the interrupt active; we end it below.
    unsafe { asm!("mrs {}, icc_iar1_el1", out(reg) iar, options(nostack)) };
    let id = (iar & 0xff_ffff) as u32;
    if (SPURIOUS..1024).contains(&id) {
        return true;
    }
    let handled = intr::dispatch(id as usize);
    // SAFETY: ends the interrupt acknowledged above.
    unsafe { asm!("msr icc_eoir1_el1, {}", "isb", in(reg) iar, options(nostack)) };
    handled
}
