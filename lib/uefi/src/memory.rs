//! The UEFI memory map, decoded into typed descriptors.

use core::fmt;

use crate::{Result, Status, raw};

/// `EFI_MEMORY_TYPE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryType {
    Reserved,
    LoaderCode,
    LoaderData,
    BootServicesCode,
    BootServicesData,
    RuntimeServicesCode,
    RuntimeServicesData,
    Conventional,
    Unusable,
    AcpiReclaim,
    AcpiNvs,
    Mmio,
    MmioPortSpace,
    PalCode,
    Persistent,
    Unaccepted,
    /// OEM (`0x7000_0000..`) and OS-loader (`0x8000_0000..`) ranges, or values
    /// newer than this code.
    Other(u32),
}

const TYPES: [MemoryType; 16] = [
    MemoryType::Reserved,
    MemoryType::LoaderCode,
    MemoryType::LoaderData,
    MemoryType::BootServicesCode,
    MemoryType::BootServicesData,
    MemoryType::RuntimeServicesCode,
    MemoryType::RuntimeServicesData,
    MemoryType::Conventional,
    MemoryType::Unusable,
    MemoryType::AcpiReclaim,
    MemoryType::AcpiNvs,
    MemoryType::Mmio,
    MemoryType::MmioPortSpace,
    MemoryType::PalCode,
    MemoryType::Persistent,
    MemoryType::Unaccepted,
];

impl MemoryType {
    #[must_use]
    pub fn from_raw(v: u32) -> Self {
        TYPES.get(v as usize).copied().unwrap_or(MemoryType::Other(v))
    }

    #[must_use]
    pub fn to_raw(self) -> u32 {
        match self {
            MemoryType::Other(v) => v,
            #[allow(clippy::cast_possible_truncation)]
            t => TYPES.iter().position(|&x| x == t).unwrap_or(0) as u32,
        }
    }

    /// Memory the OS may use once boot services have been exited.
    #[must_use]
    pub fn is_usable_after_exit(self) -> bool {
        matches!(
            self,
            MemoryType::Conventional
                | MemoryType::LoaderCode
                | MemoryType::LoaderData
                | MemoryType::BootServicesCode
                | MemoryType::BootServicesData
        )
    }
}

/// `EFI_MEMORY_*` attribute bits.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MemoryAttribute(pub u64);

impl MemoryAttribute {
    pub const UC: u64 = 1 << 0;
    pub const WC: u64 = 1 << 1;
    pub const WT: u64 = 1 << 2;
    pub const WB: u64 = 1 << 3;
    pub const WP: u64 = 1 << 12;
    pub const RP: u64 = 1 << 13;
    pub const XP: u64 = 1 << 14;
    pub const NV: u64 = 1 << 15;
    pub const RO: u64 = 1 << 17;
    pub const RUNTIME: u64 = 1 << 63;

    #[must_use]
    pub fn contains(self, bits: u64) -> bool {
        self.0 & bits == bits
    }
}

impl fmt::Debug for MemoryAttribute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

/// Identifies one version of the memory map; required by `ExitBootServices`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapKey(pub usize);

/// One decoded memory-map entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryDescriptor {
    pub ty: MemoryType,
    pub phys_start: u64,
    pub pages: u64,
    pub attr: MemoryAttribute,
}

impl MemoryDescriptor {
    pub const PAGE_SIZE: u64 = 4096;

    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.pages * Self::PAGE_SIZE
    }
}

/// A memory map snapshot borrowed from a caller-provided buffer.
#[derive(Debug, Clone, Copy)]
pub struct MemoryMap<'a> {
    buf: &'a [u8],
    desc_size: usize,
    key: MapKey,
    version: u32,
}

impl<'a> MemoryMap<'a> {
    pub(crate) fn new(buf: &'a [u8], desc_size: usize, key: MapKey, version: u32) -> Result<Self> {
        if desc_size < size_of::<raw::MemoryDescriptor>() || !buf.len().is_multiple_of(desc_size) {
            return Err(Status::INVALID_PARAMETER);
        }
        Ok(MemoryMap {
            buf,
            desc_size,
            key,
            version,
        })
    }

    #[must_use]
    pub fn key(&self) -> MapKey {
        self.key
    }

    #[must_use]
    pub fn version(&self) -> u32 {
        self.version
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len() / self.desc_size
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = MemoryDescriptor> + 'a {
        self.buf.chunks_exact(self.desc_size).map(|c| {
            // SAFETY: each chunk holds at least one raw descriptor (checked in
            // `new`); the buffer may be unaligned, hence read_unaligned.
            let d = unsafe { c.as_ptr().cast::<raw::MemoryDescriptor>().read_unaligned() };
            MemoryDescriptor {
                ty: MemoryType::from_raw(d.ty),
                phys_start: d.physical_start,
                pages: d.number_of_pages,
                attr: MemoryAttribute(d.attribute),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_type_round_trip() {
        for v in 0..20 {
            assert_eq!(MemoryType::from_raw(v).to_raw(), v);
        }
        assert_eq!(MemoryType::from_raw(7), MemoryType::Conventional);
        assert_eq!(MemoryType::from_raw(0x8000_0001), MemoryType::Other(0x8000_0001));
    }

    #[test]
    fn iterates_with_firmware_stride() {
        // Firmware descriptors are commonly 48 bytes: 40 defined + 8 padding.
        let stride = 48;
        let mut buf = vec![0u8; stride * 2];
        for (i, (ty, start, pages)) in [(7u32, 0x10_0000u64, 16u64), (11, 0xfee0_0000, 1)]
            .into_iter()
            .enumerate()
        {
            let d = raw::MemoryDescriptor {
                ty,
                physical_start: start,
                virtual_start: 0,
                number_of_pages: pages,
                attribute: 0,
            };
            // SAFETY: test buffer is large enough; unaligned write is allowed.
            unsafe {
                buf.as_mut_ptr()
                    .add(i * stride)
                    .cast::<raw::MemoryDescriptor>()
                    .write_unaligned(d);
            };
        }
        let map = MemoryMap::new(&buf, stride, MapKey(1), 1).unwrap();
        let v: Vec<_> = map.iter().collect();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].ty, MemoryType::Conventional);
        assert_eq!(v[0].bytes(), 16 * 4096);
        assert_eq!(v[1].ty, MemoryType::Mmio);
    }

    #[test]
    fn rejects_bad_stride() {
        assert!(MemoryMap::new(&[0; 48], 8, MapKey(0), 1).is_err());
        assert!(MemoryMap::new(&[0; 50], 48, MapKey(0), 1).is_err());
    }
}
