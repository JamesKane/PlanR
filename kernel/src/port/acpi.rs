//! ACPI glue: physical memory access for `lib/acpi`, and the boot report.

use acpi::{Acpi, CacheKind, Error, Iort, IortKind, PhysMem, Pptt, PpttNode, Signature};
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
        for e in self.tables.mcfg() {
            let (base, len) = e.window();
            println!(
                "pci: segment {} buses {:#x}-{:#x}, ECAM at {base:#x} ({} MiB)",
                e.segment,
                e.bus_start,
                e.bus_end,
                len >> 20
            );
        }
        if let Some(p) = self.tables.pptt() {
            report_pptt(p);
        }
        if let Some(t) = self.tables.iort() {
            report_iort(t);
        }
    }
}

/// Counts the processors and packages, and lists the first processor's
/// caches.
fn report_pptt(p: Pptt<'_>) {
    let (mut cpus, mut packages) = (0, 0);
    let mut first = None;
    for n in p.nodes() {
        if let PpttNode::Processor(n) = n {
            if n.flags.leaf() {
                cpus += 1;
                first = first.or(Some(n));
            }
            if n.flags.physical_package() {
                packages += 1;
            }
        }
    }
    crate::print!("pptt: {cpus} processors, {packages} packages");
    if let Some(cpu) = first {
        crate::print!("; caches of the first:");
        if p.caches(cpu).next().is_none() {
            crate::print!(" none listed");
        }
        for (level, c) in p.caches(cpu) {
            let kind = match c.kind {
                Some(CacheKind::Data) => "d",
                Some(CacheKind::Instruction) => "i",
                _ => "",
            };
            crate::print!(" L{level}{kind}");
            if let Some(size) = c.size {
                crate::print!(" {}K", size >> 10);
            }
        }
    }
    println!();
}

/// Counts the nodes by kind, and says where each PCI segment's IDs go.
fn report_iort(t: Iort<'_>) {
    let (mut its, mut smmus, mut rcs, mut named) = (0, 0, 0, 0);
    for n in t.nodes() {
        match n.kind {
            IortKind::ItsGroup(_) => its += 1,
            IortKind::SmmuV1V2 { .. } | IortKind::SmmuV3 { .. } => smmus += 1,
            IortKind::RootComplex { .. } => rcs += 1,
            IortKind::NamedComponent { .. } => named += 1,
            _ => {}
        }
    }
    println!("iort: {its} ITS groups, {smmus} SMMUs, {rcs} root complexes, {named} named components");
    for rc in t.nodes() {
        let IortKind::RootComplex { segment, .. } = rc.kind else {
            continue;
        };
        crate::print!("iort: PCI segment {segment}:");
        if let Some((IortKind::SmmuV3 { base, .. } | IortKind::SmmuV1V2 { base, .. }, _)) =
            t.stream_id(&rc, 0).map(|(n, id)| (n.kind, id))
        {
            crate::print!(" behind the SMMU at {base:#x}");
        } else {
            crate::print!(" no SMMU");
        }
        let its = if t.its_device_id(&rc, 0).is_some() {
            "MSIs to an ITS"
        } else {
            "no ITS"
        };
        println!(", {its}");
    }
}
