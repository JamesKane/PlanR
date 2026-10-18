//! UEFI memory map → `bootinfo::Region`s.

use bootinfo::{Region, RegionKind, uefi_type};
use uefi::{MemoryDescriptor, MemoryMap, MemoryType};

/// What the kernel should make of a UEFI memory type, once boot services
/// are gone. `None`: not RAM (MMIO), left out of the map.
fn kind(t: MemoryType) -> Option<RegionKind> {
    Some(match t {
        MemoryType::Conventional
        | MemoryType::LoaderCode
        | MemoryType::LoaderData
        | MemoryType::BootServicesCode
        | MemoryType::BootServicesData => RegionKind::Usable,
        MemoryType::Other(uefi_type::KERNEL) => RegionKind::Kernel,
        MemoryType::Other(uefi_type::BOOT) => RegionKind::Boot,
        MemoryType::Other(uefi_type::LOG) => RegionKind::LogRing,
        MemoryType::AcpiReclaim => RegionKind::AcpiReclaimable,
        MemoryType::AcpiNvs => RegionKind::AcpiNvs,
        MemoryType::RuntimeServicesCode | MemoryType::RuntimeServicesData => RegionKind::UefiRuntime,
        MemoryType::Persistent => RegionKind::Persistent,
        MemoryType::Unusable => RegionKind::Bad,
        MemoryType::Mmio | MemoryType::MmioPortSpace => return None,
        _ => RegionKind::Reserved,
    })
}

fn region(d: &MemoryDescriptor) -> Option<Region> {
    Some(Region {
        start: d.phys_start,
        len: d.bytes(),
        kind: kind(d.ty)?,
    })
}

/// Converts `map` into `out`, sorted and merged, without allocating (this
/// runs between the final `GetMemoryMap` and `ExitBootServices`). Returns
/// the number of regions, or `None` if `out` is too small.
pub(crate) fn convert(map: &MemoryMap<'_>, out: &mut [Region]) -> Option<usize> {
    let mut n = 0;
    for r in map.iter().filter_map(|d| region(&d)) {
        *out.get_mut(n)? = r;
        n += 1;
    }
    // UEFI does not promise address order. Insertion sort: small n, no
    // allocation, and the map is usually nearly sorted already.
    for i in 1..n {
        let mut j = i;
        while j > 0 && out[j - 1].start > out[j].start {
            out.swap(j - 1, j);
            j -= 1;
        }
    }
    let mut merged = 0;
    for i in 0..n {
        let r = out[i];
        bootinfo::push_region(&mut out[..=i], &mut merged, r);
    }
    Some(merged)
}

/// Physical ranges of RAM (any kind the kernel direct-maps), merged
/// regardless of kind, for building the direct map.
pub(crate) fn ram_ranges(map: &MemoryMap<'_>) -> alloc::vec::Vec<(u64, u64)> {
    let mut v: alloc::vec::Vec<(u64, u64)> = map
        .iter()
        .filter_map(|d| region(&d))
        .filter(|r| r.kind.is_ram())
        .map(|r| (r.start, r.end()))
        .collect();
    v.sort_unstable();
    let mut out: alloc::vec::Vec<(u64, u64)> = alloc::vec::Vec::with_capacity(v.len());
    for (s, e) in v {
        match out.last_mut() {
            Some(last) if last.1 >= s => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}
