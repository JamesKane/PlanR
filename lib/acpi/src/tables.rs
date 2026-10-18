//! Typed views of individual tables.

use crate::{Sdt, u16_at, u32_at, u64_at};

/// `Generic Address Structure` address space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressSpace {
    SystemMemory,
    SystemIo,
    PciConfig,
    Other(u8),
}

/// Generic Address Structure (ACPI 6.5 §5.2.3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gas {
    pub space: AddressSpace,
    pub bit_width: u8,
    pub bit_offset: u8,
    /// 0 undefined, 1 byte, 2 word, 3 dword, 4 qword.
    pub access_size: u8,
    pub address: u64,
}

impl Gas {
    pub(crate) fn parse(b: &[u8], o: usize) -> Option<Gas> {
        let g = b.get(o..o + 12)?;
        Some(Gas {
            space: match g[0] {
                0 => AddressSpace::SystemMemory,
                1 => AddressSpace::SystemIo,
                2 => AddressSpace::PciConfig,
                n => AddressSpace::Other(n),
            },
            bit_width: g[1],
            bit_offset: g[2],
            access_size: g[3],
            address: u64_at(g, 4)?,
        })
    }
}

/// Serial interface types shared by SPCR and DBG2 (Microsoft "Serial Port
/// Console Redirection" and "Debug Port Table 2" specifications).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UartKind {
    /// Fully 16550-compatible.
    Ns16550,
    /// 16550 subset (Microsoft DBGP).
    Ns16550Subset,
    /// Arm PL011.
    Pl011,
    /// Arm SBSA generic UART, 32-bit access only.
    SbsaGeneric32,
    /// Arm SBSA generic UART.
    SbsaGeneric,
    /// 16550-compatible with register layout given by the GAS.
    Ns16550Gas,
    Other(u16),
}

impl UartKind {
    #[must_use]
    pub fn from_code(c: u16) -> UartKind {
        match c {
            0x00 => UartKind::Ns16550,
            0x01 => UartKind::Ns16550Subset,
            0x03 => UartKind::Pl011,
            0x0d => UartKind::SbsaGeneric32,
            0x0e => UartKind::SbsaGeneric,
            0x12 => UartKind::Ns16550Gas,
            n => UartKind::Other(n),
        }
    }

    /// PL011 and the SBSA generic UART share the data and flag registers.
    #[must_use]
    pub fn is_pl011_like(self) -> bool {
        matches!(self, UartKind::Pl011 | UartKind::SbsaGeneric | UartKind::SbsaGeneric32)
    }

    #[must_use]
    pub fn is_16550_like(self) -> bool {
        matches!(self, UartKind::Ns16550 | UartKind::Ns16550Subset | UartKind::Ns16550Gas)
    }
}

/// Serial Port Console Redirection table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spcr {
    pub kind: UartKind,
    pub base: Gas,
}

impl Spcr {
    #[must_use]
    pub fn parse(t: Sdt<'_>) -> Option<Spcr> {
        let b = t.bytes();
        Some(Spcr {
            kind: UartKind::from_code(u16::from(*b.get(36)?)),
            base: Gas::parse(b, 40)?,
        })
    }
}

/// One debug device from DBG2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dbg2Device<'a> {
    /// 0x8000 serial, 0x8001 1394, 0x8002 USB, 0x8003 network.
    pub port_type: u16,
    pub port_subtype: u16,
    /// The first base address register, if any.
    pub base: Option<Gas>,
    /// ACPI namespace path, e.g. `\_SB.COM2`.
    pub namespace: &'a str,
}

impl Dbg2Device<'_> {
    pub const SERIAL: u16 = 0x8000;

    /// The UART kind, for serial devices.
    #[must_use]
    pub fn uart(&self) -> Option<UartKind> {
        (self.port_type == Self::SERIAL).then(|| UartKind::from_code(self.port_subtype))
    }
}

/// The devices listed in a DBG2 table.
pub fn dbg2_devices(t: Sdt<'_>) -> impl Iterator<Item = Dbg2Device<'_>> {
    let b = t.bytes();
    let first = u32_at(b, 36).unwrap_or(0) as usize;
    let count = u32_at(b, 40).unwrap_or(0);
    let mut off = first;
    (0..count).map_while(move |_| {
        let d = b.get(off..)?;
        let len = usize::from(u16_at(d, 1)?);
        if len < 22 || len > d.len() {
            return None;
        }
        let d = &d[..len];
        off += len;
        let nregs = d[3];
        let ns_len = usize::from(u16_at(d, 4)?);
        let ns_off = usize::from(u16_at(d, 6)?);
        let bar_off = usize::from(u16_at(d, 16)?);
        let namespace = d
            .get(ns_off..ns_off + ns_len)
            .and_then(|s| core::str::from_utf8(s).ok())
            .map_or("", |s| s.trim_end_matches('\0'));
        Some(Dbg2Device {
            port_type: u16_at(d, 10)?,
            port_subtype: u16_at(d, 12)?,
            base: if nregs > 0 { Gas::parse(d, bar_off) } else { None },
            namespace,
        })
    })
}

/// Fixed ACPI Description Table: only the fields Plan R needs so far.
#[derive(Debug, Clone, Copy)]
pub struct Fadt<'a>(Sdt<'a>);

impl<'a> Fadt<'a> {
    pub(crate) fn new(t: Sdt<'a>) -> Self {
        Fadt(t)
    }

    /// `IAPC_BOOT_ARCH` (x86).
    #[must_use]
    pub fn iapc_boot_flags(&self) -> u16 {
        u16_at(self.0.bytes(), 109).unwrap_or(0)
    }

    /// `ARM_BOOT_ARCH`: bit 0 PSCI compliant, bit 1 use HVC (else SMC).
    #[must_use]
    pub fn arm_boot_flags(&self) -> u16 {
        u16_at(self.0.bytes(), 129).unwrap_or(0)
    }

    #[must_use]
    pub fn psci(&self) -> Option<PsciConduit> {
        let f = self.arm_boot_flags();
        (f & 1 != 0).then_some(if f & 2 != 0 { PsciConduit::Hvc } else { PsciConduit::Smc })
    }

    /// `FADT Minor Version` (the major version is the header revision).
    #[must_use]
    pub fn minor_version(&self) -> u8 {
        self.0.bytes().get(131).copied().unwrap_or(0) & 0xf
    }

    #[must_use]
    pub fn sdt(&self) -> Sdt<'a> {
        self.0
    }
}

/// How to call PSCI firmware on arm64.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PsciConduit {
    Smc,
    Hvc,
}

/// One MADT interrupt controller structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MadtEntry {
    LocalApic {
        uid: u8,
        apic_id: u8,
        enabled: bool,
        online_capable: bool,
    },
    IoApic {
        id: u8,
        address: u32,
        gsi_base: u32,
    },
    InterruptOverride {
        bus: u8,
        source: u8,
        gsi: u32,
        flags: u16,
    },
    X2Apic {
        uid: u32,
        x2apic_id: u32,
        enabled: bool,
        online_capable: bool,
    },
    Gicc {
        uid: u32,
        mpidr: u64,
        enabled: bool,
        online_capable: bool,
        gicr_base: u64,
    },
    Gicd {
        id: u32,
        base: u64,
        version: u8,
    },
    Gicr {
        base: u64,
        length: u32,
    },
    GicIts {
        id: u32,
        base: u64,
    },
    Other {
        ty: u8,
        len: u8,
    },
}

impl MadtEntry {
    /// True for an enabled (or online-capable) processor entry.
    #[must_use]
    pub fn is_usable_cpu(&self) -> bool {
        match *self {
            MadtEntry::LocalApic {
                enabled,
                online_capable,
                ..
            }
            | MadtEntry::X2Apic {
                enabled,
                online_capable,
                ..
            }
            | MadtEntry::Gicc {
                enabled,
                online_capable,
                ..
            } => enabled || online_capable,
            _ => false,
        }
    }
}

/// Multiple APIC Description Table.
#[derive(Debug, Clone, Copy)]
pub struct Madt<'a>(Sdt<'a>);

impl<'a> Madt<'a> {
    pub(crate) fn new(t: Sdt<'a>) -> Self {
        Madt(t)
    }

    /// The legacy local APIC physical address.
    #[must_use]
    pub fn local_apic_address(&self) -> u32 {
        u32_at(self.0.bytes(), 36).unwrap_or(0)
    }

    /// True if the system also has dual 8259 PICs (`PCAT_COMPAT`).
    #[must_use]
    pub fn has_8259(&self) -> bool {
        u32_at(self.0.bytes(), 40).unwrap_or(0) & 1 != 0
    }

    pub fn entries(&self) -> impl Iterator<Item = MadtEntry> + 'a {
        let b = self.0.bytes();
        let mut off = 44;
        core::iter::from_fn(move || {
            let ty = *b.get(off)?;
            let len = *b.get(off + 1)?;
            let e = b.get(off..off + usize::from(len)).filter(|_| len >= 2)?;
            off += usize::from(len);
            Some(parse_madt_entry(ty, len, e).unwrap_or(MadtEntry::Other { ty, len }))
        })
    }

    #[must_use]
    pub fn cpu_count(&self) -> usize {
        self.entries().filter(MadtEntry::is_usable_cpu).count()
    }
}

fn parse_madt_entry(ty: u8, len: u8, e: &[u8]) -> Option<MadtEntry> {
    let flag = |f: u32| (f & 1 != 0, f & 2 != 0);
    Some(match ty {
        0 => {
            let (enabled, online_capable) = flag(u32_at(e, 4)?);
            MadtEntry::LocalApic {
                uid: e[2],
                apic_id: e[3],
                enabled,
                online_capable,
            }
        }
        1 => MadtEntry::IoApic {
            id: e[2],
            address: u32_at(e, 4)?,
            gsi_base: u32_at(e, 8)?,
        },
        2 => MadtEntry::InterruptOverride {
            bus: e[2],
            source: e[3],
            gsi: u32_at(e, 4)?,
            flags: u16_at(e, 8)?,
        },
        9 => {
            let (enabled, online_capable) = flag(u32_at(e, 8)?);
            MadtEntry::X2Apic {
                uid: u32_at(e, 12)?,
                x2apic_id: u32_at(e, 4)?,
                enabled,
                online_capable,
            }
        }
        0x0b if len >= 76 => {
            let (enabled, online_capable) = flag(u32_at(e, 12)?);
            MadtEntry::Gicc {
                uid: u32_at(e, 8)?,
                mpidr: u64_at(e, 68)?,
                enabled,
                online_capable,
                gicr_base: u64_at(e, 60)?,
            }
        }
        0x0c => MadtEntry::Gicd {
            id: u32_at(e, 4)?,
            base: u64_at(e, 8)?,
            version: *e.get(20)?,
        },
        0x0e => MadtEntry::Gicr {
            base: u64_at(e, 4)?,
            length: u32_at(e, 12)?,
        },
        0x0f => MadtEntry::GicIts {
            id: u32_at(e, 4)?,
            base: u64_at(e, 8)?,
        },
        _ => return None,
    })
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)] // test fixtures are tiny
mod tests {
    use super::*;
    use crate::testmem::*;
    use crate::{Acpi, Signature};

    fn gas(space: u8, access: u8, addr: u64) -> Vec<u8> {
        let mut g = vec![space, 8, 0, access];
        g.extend_from_slice(&addr.to_le_bytes());
        g
    }

    #[test]
    fn spcr_pl011() {
        let mut body = vec![0x03, 0, 0, 0];
        body.extend(gas(0, 3, 0x0900_0000));
        body.extend([0; 40]);
        let m = system(&[sdt(*b"SPCR", 2, &body)]);
        let s = Acpi::new(&m, 0x1000).unwrap().spcr().unwrap();
        assert_eq!(s.kind, UartKind::Pl011);
        assert_eq!(s.base.space, AddressSpace::SystemMemory);
        assert_eq!(s.base.address, 0x0900_0000);
    }

    /// Mirrors the Orange Pi 6 Plus DBG2: one SBSA UART, `\_SB.COM2`.
    #[test]
    fn dbg2_device() {
        let ns = b"\\_SB.COM2\0";
        let mut dev = vec![0u8; 22];
        dev[0] = 0; // revision
        dev[3] = 1; // one GAS
        let gas_off = 22u16;
        let size_off = gas_off + 12;
        let ns_off = size_off + 4;
        let len = ns_off + ns.len() as u16;
        dev[1..3].copy_from_slice(&len.to_le_bytes());
        dev[4..6].copy_from_slice(&(ns.len() as u16).to_le_bytes());
        dev[6..8].copy_from_slice(&ns_off.to_le_bytes());
        dev[10..12].copy_from_slice(&0x8000u16.to_le_bytes());
        dev[12..14].copy_from_slice(&0x000eu16.to_le_bytes());
        dev[16..18].copy_from_slice(&gas_off.to_le_bytes());
        dev[18..20].copy_from_slice(&size_off.to_le_bytes());
        dev.extend(gas(0, 3, 0x040d_0000));
        dev.extend(0x1000u32.to_le_bytes());
        dev.extend_from_slice(ns);
        let mut body = 44u32.to_le_bytes().to_vec();
        body.extend(1u32.to_le_bytes());
        body.extend(dev);
        let m = system(&[sdt(*b"DBG2", 0, &body)]);
        let acpi = Acpi::new(&m, 0x1000).unwrap();
        let d: Vec<_> = acpi.dbg2().collect();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].uart(), Some(UartKind::SbsaGeneric));
        assert_eq!(d[0].base.unwrap().address, 0x040d_0000);
        assert_eq!(d[0].namespace, "\\_SB.COM2");
    }

    #[test]
    fn madt_cpus_and_gic() {
        let mut body = vec![0u8; 8];
        // Two enabled LAPICs, one disabled, one IOAPIC.
        for (uid, flags) in [(0u8, 1u32), (1, 1), (2, 0)] {
            body.extend([0, 8, uid, uid]);
            body.extend(flags.to_le_bytes());
        }
        body.extend([1, 12, 0, 0]);
        body.extend(0xfec0_0000u32.to_le_bytes());
        body.extend(0u32.to_le_bytes());
        // A GICC (76 bytes) with MPIDR 0x100.
        let mut gicc = vec![0u8; 76];
        gicc[0] = 0x0b;
        gicc[1] = 76;
        gicc[12..16].copy_from_slice(&1u32.to_le_bytes());
        gicc[68..76].copy_from_slice(&0x100u64.to_le_bytes());
        body.extend(gicc);
        // Unknown type is preserved, not fatal.
        body.extend([0x7f, 4, 0, 0]);
        let m = system(&[sdt(*b"APIC", 5, &body)]);
        let madt = Acpi::new(&m, 0x1000).unwrap().madt().unwrap();
        assert_eq!(madt.cpu_count(), 3);
        let e: Vec<_> = madt.entries().collect();
        assert!(matches!(
            e[3],
            MadtEntry::IoApic {
                address: 0xfec0_0000,
                ..
            }
        ));
        assert!(matches!(
            e[4],
            MadtEntry::Gicc {
                mpidr: 0x100,
                enabled: true,
                ..
            }
        ));
        assert_eq!(e[5], MadtEntry::Other { ty: 0x7f, len: 4 });
    }

    #[test]
    fn madt_stops_on_zero_length_entry() {
        let mut body = vec![0u8; 8];
        body.extend([0, 0, 0, 0]);
        let m = system(&[sdt(*b"APIC", 5, &body)]);
        assert_eq!(Acpi::new(&m, 0x1000).unwrap().madt().unwrap().entries().count(), 0);
    }

    #[test]
    fn fadt_psci_conduit() {
        let mut body = vec![0u8; 240];
        body[129 - 36] = 0b01; // PSCI, SMC
        let m = system(&[sdt(*b"FACP", 6, &body)]);
        let acpi = Acpi::new(&m, 0x1000).unwrap();
        assert_eq!(acpi.fadt().unwrap().psci(), Some(PsciConduit::Smc));
        assert!(acpi.find(Signature::FADT).is_some());
    }
}
