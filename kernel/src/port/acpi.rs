//! ACPI glue: physical memory access for `lib/acpi`, and the boot report.

use acpi::{Acpi, Error, PhysMem, Signature};
use bootinfo::phys_to_virt;

use crate::port::mm::{self, MapKind};
use crate::println;

/// Physical memory as the kernel sees it: the direct map for RAM, and
/// on-demand read-only mappings for anything else (some firmware keeps
/// ACPI tables in reserved ranges).
#[derive(Debug)]
pub(crate) struct KernelMem;

impl PhysMem for KernelMem {
    fn read(&self, phys: u64, len: usize) -> Option<&[u8]> {
        let n = len as u64;
        let va = if mm::in_direct_map(phys, n) {
            phys_to_virt(phys)
        } else {
            // Leaks the mapping; table reads during bring-up are few.
            mm::map_phys(phys, n, MapKind::NormalReadOnly)?
        };
        // SAFETY: `va` maps `len` readable bytes for the kernel's lifetime.
        Some(unsafe { core::slice::from_raw_parts(va as *const u8, len) })
    }
}

static MEM: KernelMem = KernelMem;

/// Why ACPI is unavailable.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Unavailable {
    NoRsdp,
    Bad(Error),
}

impl core::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Unavailable::NoRsdp => f.write_str("no RSDP from the loader (ACPI is required, ADR 0003)"),
            Unavailable::Bad(e) => write!(f, "{e}"),
        }
    }
}

/// The system's ACPI tables.
#[derive(Debug)]
pub(crate) struct System {
    pub(crate) tables: Acpi<'static, KernelMem>,
}

impl System {
    pub(crate) fn new(rsdp: Option<u64>) -> Result<System, Unavailable> {
        let rsdp = rsdp.ok_or(Unavailable::NoRsdp)?;
        Acpi::new(&MEM, rsdp)
            .map(|tables| System { tables })
            .map_err(Unavailable::Bad)
    }

    pub(crate) fn report(&self) {
        let r = &self.tables.rsdp;
        crate::print!("acpi: RSDP rev {} OEM \"{}\"; tables:", r.revision, r.oem_id());
        for t in self.tables.tables() {
            match t {
                Ok(t) => crate::print!(" {}", t.signature()),
                Err(e) => crate::print!(" [{e}]"),
            }
        }
        println!();
        if let Some(madt) = self.tables.madt() {
            println!("cpus: {} usable in MADT", madt.cpu_count());
        }
        if let Some(fadt) = self.tables.fadt() {
            let s = fadt.sdt();
            println!("fadt: revision {}.{}", s.revision(), fadt.minor_version());
            if let Some(c) = fadt.psci() {
                println!("psci: {c:?} conduit");
            }
        }
        for sig in [Signature::SPCR, Signature::DBG2] {
            if self.tables.find(sig).is_none() {
                println!("acpi: no {sig}");
            }
        }
    }
}
