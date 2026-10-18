//! I/O APICs: route global system interrupts (GSIs) to vectors on the
//! boot CPU. GSI `n` gets vector [`GSI_VECTOR_BASE`]` + n`.
//!
//! Interrupt remapping (needed for APIC IDs above 255) comes with the
//! IOMMU.

use acpi::{Madt, MadtEntry, MpsFlags, Polarity, Trigger};

use super::GSI_VECTOR_BASE;
use crate::port::lock::SpinLock;
use crate::port::mm::{self, MapKind};
use crate::println;

const IOREGSEL: u64 = 0x00;
const IOWIN: u64 = 0x10;
const REG_VERSION: u32 = 0x01;
const REG_REDIRECT: u32 = 0x10;

const RTE_MASKED: u64 = 1 << 16;
const RTE_LEVEL: u64 = 1 << 15;
const RTE_ACTIVE_LOW: u64 = 1 << 13;

/// Vectors available for GSIs: up to the local vectors at 0xf0.
const MAX_GSI: u32 = 0xf0 - GSI_VECTOR_BASE as u32;

#[derive(Debug, Clone, Copy)]
struct IoApic {
    va: u64,
    gsi_base: u32,
    inputs: u32,
}

impl IoApic {
    fn read(&self, reg: u32) -> u32 {
        // SAFETY: `va` maps this I/O APIC; the caller holds `IOAPICS`, which
        // keeps the select/window pair atomic.
        unsafe {
            ((self.va + IOREGSEL) as *mut u32).write_volatile(reg);
            ((self.va + IOWIN) as *const u32).read_volatile()
        }
    }

    fn write(&self, reg: u32, v: u32) {
        // SAFETY: as in `read`.
        unsafe {
            ((self.va + IOREGSEL) as *mut u32).write_volatile(reg);
            ((self.va + IOWIN) as *mut u32).write_volatile(v);
        }
    }

    #[allow(clippy::cast_possible_truncation)] // splitting the entry in two
    fn set(&self, pin: u32, rte: u64) {
        let reg = REG_REDIRECT + 2 * pin;
        // Mask first, so the half-written entry is never live.
        self.write(reg, RTE_MASKED as u32);
        self.write(reg + 1, (rte >> 32) as u32);
        self.write(reg, rte as u32);
    }

    fn covers(&self, gsi: u32) -> bool {
        (self.gsi_base..self.gsi_base + self.inputs).contains(&gsi)
    }
}

const MAX_IOAPICS: usize = 8;

#[derive(Debug)]
struct State {
    ioapics: [Option<IoApic>; MAX_IOAPICS],
    /// ISA IRQ to GSI, with polarity and trigger, from MADT overrides.
    isa: [(u32, MpsFlags); 16],
}

static STATE: SpinLock<State> = SpinLock::new(State {
    ioapics: [None; MAX_IOAPICS],
    isa: [(0, MpsFlags(0)); 16],
});

/// Finds, maps and masks every I/O APIC, and records the ISA overrides.
/// Returns how many there are.
pub(crate) fn init(madt: &Madt<'_>) -> Result<usize, &'static str> {
    let mut s = STATE.lock();
    for (irq, slot) in (0u32..).zip(s.isa.iter_mut()) {
        *slot = (irq, MpsFlags(0));
    }
    let mut n = 0;
    for e in madt.entries() {
        match e {
            MadtEntry::IoApic { id, address, gsi_base } => {
                let Some(slot) = s.ioapics.get_mut(n) else {
                    println!("ioapic: more than {MAX_IOAPICS}; ignoring id {id}");
                    continue;
                };
                let va = mm::map_phys(u64::from(address), mm::PAGE, MapKind::Device).ok_or("cannot map an I/O APIC")?;
                let mut io = IoApic {
                    va,
                    gsi_base,
                    inputs: 0,
                };
                io.inputs = ((io.read(REG_VERSION) >> 16) & 0xff) + 1;
                for pin in 0..io.inputs {
                    io.set(pin, RTE_MASKED);
                }
                println!(
                    "ioapic: id {id} at {address:#x}, GSIs {}-{}",
                    gsi_base,
                    gsi_base + io.inputs - 1
                );
                *slot = Some(io);
                n += 1;
            }
            MadtEntry::InterruptOverride {
                bus: 0,
                source,
                gsi,
                flags,
            } => {
                if let Some(slot) = s.isa.get_mut(usize::from(source)) {
                    *slot = (gsi, flags);
                }
            }
            _ => {}
        }
    }
    if n == 0 {
        return Err("no I/O APIC in MADT");
    }
    Ok(n)
}

/// The vector GSI `gsi` is delivered on.
pub(crate) fn vector(gsi: u32) -> Option<u8> {
    let gsi = u8::try_from(gsi).ok().filter(|&g| u32::from(g) < MAX_GSI)?;
    Some(GSI_VECTOR_BASE + gsi)
}

/// Whether some I/O APIC has input `gsi`.
pub(crate) fn exists(gsi: u32) -> bool {
    STATE.lock().ioapics.iter().flatten().any(|io| io.covers(gsi))
}

/// The GSI, polarity and trigger for ISA IRQ `irq`.
#[allow(dead_code)] // for the legacy devices that come with the drivers
pub(crate) fn isa(irq: u8) -> Option<(u32, Polarity, Trigger)> {
    let s = STATE.lock();
    let (gsi, flags) = *s.isa.get(usize::from(irq))?;
    let polarity = match flags.polarity() {
        Polarity::BusDefault => Polarity::High,
        p => p,
    };
    let trigger = match flags.trigger() {
        Trigger::BusDefault => Trigger::Edge,
        t => t,
    };
    Some((gsi, polarity, trigger))
}

/// Routes `gsi` to its vector on the CPU with APIC ID `dest`, unmasked.
pub(crate) fn route(gsi: u32, polarity: Polarity, trigger: Trigger, dest: u32) -> Result<u8, &'static str> {
    let vector = vector(gsi).ok_or("GSI beyond the vector range")?;
    let dest = u8::try_from(dest).map_err(|_| "APIC ID needs interrupt remapping")?;
    let s = STATE.lock();
    let io = s
        .ioapics
        .iter()
        .flatten()
        .find(|io| io.covers(gsi))
        .ok_or("no I/O APIC has that GSI")?;
    let mut rte = u64::from(vector) | u64::from(dest) << 56;
    if polarity == Polarity::Low {
        rte |= RTE_ACTIVE_LOW;
    }
    if trigger == Trigger::Level {
        rte |= RTE_LEVEL;
    }
    io.set(gsi - io.gsi_base, rte);
    Ok(vector)
}

pub(crate) fn mask(gsi: u32) {
    let s = STATE.lock();
    if let Some(io) = s.ioapics.iter().flatten().find(|io| io.covers(gsi)) {
        io.set(gsi - io.gsi_base, RTE_MASKED);
    }
}
