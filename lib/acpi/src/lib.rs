//! ACPI static table parsing (ACPI 6.5). AML comes in P4.
//!
//! Physical memory is reached through [`PhysMem`], so the same code runs in
//! the kernel (direct map) and in host tests (fake memory, captured dumps).
//! Every read is bounds-checked; malformed tables produce errors or are
//! skipped, never panics.

#![cfg_attr(not(test), no_std)]

mod iort;
mod pptt;
mod tables;

use core::fmt;

pub use iort::{IdMapping, Iort, IortKind, IortNode, ItsIds};
pub use pptt::{CacheKind, CacheNode, Pptt, PpttNode, ProcessorFlags, ProcessorNode};
pub use tables::{
    AddressSpace, Dbg2Device, Fadt, Gas, Gtdt, Hpet, Madt, MadtEntry, McfgEntry, MpsFlags, Polarity, PsciConduit, Spcr,
    SpcrIrq, TimerIrq, Trigger, UartKind, dbg2_devices, mcfg_entries,
};

/// Read access to physical memory.
pub trait PhysMem {
    /// Returns `len` bytes starting at physical address `phys`, or `None` if
    /// the range cannot be accessed.
    fn read(&self, phys: u64, len: usize) -> Option<&[u8]>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Unmapped(u64),
    BadRsdp,
    BadChecksum(Signature),
    BadLength(Signature),
    WrongSignature { expected: Signature, found: Signature },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unmapped(p) => write!(f, "cannot read physical {p:#x}"),
            Error::BadRsdp => f.write_str("invalid RSDP"),
            Error::BadChecksum(s) => write!(f, "{s}: bad checksum"),
            Error::BadLength(s) => write!(f, "{s}: bad length"),
            Error::WrongSignature { expected, found } => write!(f, "expected {expected}, found {found}"),
        }
    }
}

/// A four-byte table signature such as `APIC` or `FACP`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Signature(pub [u8; 4]);

impl Signature {
    pub const XSDT: Signature = Signature(*b"XSDT");
    pub const RSDT: Signature = Signature(*b"RSDT");
    pub const MADT: Signature = Signature(*b"APIC");
    pub const FADT: Signature = Signature(*b"FACP");
    pub const SPCR: Signature = Signature(*b"SPCR");
    pub const DBG2: Signature = Signature(*b"DBG2");
    pub const HPET: Signature = Signature(*b"HPET");
    pub const GTDT: Signature = Signature(*b"GTDT");
    pub const MCFG: Signature = Signature(*b"MCFG");
    pub const PPTT: Signature = Signature(*b"PPTT");
    pub const IORT: Signature = Signature(*b"IORT");
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &b in &self.0 {
            let c = if b.is_ascii_graphic() { b as char } else { '?' };
            fmt::Write::write_char(f, c)?;
        }
        Ok(())
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

fn checksum_ok(b: &[u8]) -> bool {
    b.iter().fold(0u8, |a, &x| a.wrapping_add(x)) == 0
}

pub(crate) fn u16_at(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(o..o + 2)?.try_into().ok()?))
}
pub(crate) fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?))
}
pub(crate) fn u64_at(d: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(d.get(o..o + 8)?.try_into().ok()?))
}

/// Trims trailing spaces and NULs from a fixed-width ASCII field.
fn ascii(b: &[u8]) -> &str {
    let s = core::str::from_utf8(b).unwrap_or("?");
    s.trim_end_matches([' ', '\0'])
}

/// The Root System Description Pointer.
#[derive(Debug, Clone, Copy)]
pub struct Rsdp {
    pub revision: u8,
    oem_id: [u8; 6],
    pub rsdt: u32,
    /// Zero for ACPI 1.0 (revision 0).
    pub xsdt: u64,
}

impl Rsdp {
    pub fn read(mem: &impl PhysMem, phys: u64) -> Result<Rsdp, Error> {
        let b = mem.read(phys, 20).ok_or(Error::Unmapped(phys))?;
        if b[..8] != *b"RSD PTR " || !checksum_ok(b) {
            return Err(Error::BadRsdp);
        }
        let revision = b[15];
        let rsdt = u32_at(b, 16).ok_or(Error::BadRsdp)?;
        let mut xsdt = 0;
        if revision >= 2 {
            let ext = mem.read(phys, 36).ok_or(Error::Unmapped(phys))?;
            let len = u32_at(ext, 20).ok_or(Error::BadRsdp)? as usize;
            let full = mem.read(phys, len.max(36)).ok_or(Error::Unmapped(phys))?;
            if !checksum_ok(full) {
                return Err(Error::BadRsdp);
            }
            xsdt = u64_at(ext, 24).ok_or(Error::BadRsdp)?;
        }
        Ok(Rsdp {
            revision,
            oem_id: b[9..15].try_into().map_err(|_| Error::BadRsdp)?,
            rsdt,
            xsdt,
        })
    }

    #[must_use]
    pub fn oem_id(&self) -> &str {
        ascii(&self.oem_id)
    }
}

/// A system description table: header plus body.
#[derive(Clone, Copy)]
pub struct Sdt<'a> {
    pub phys: u64,
    data: &'a [u8],
}

impl fmt::Debug for Sdt<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} @ {:#x} ({} bytes)", self.signature(), self.phys, self.data.len())
    }
}

impl<'a> Sdt<'a> {
    /// Reads the table at `phys`, checking its length and checksum.
    pub fn read(mem: &'a impl PhysMem, phys: u64) -> Result<Sdt<'a>, Error> {
        let hdr = mem.read(phys, 36).ok_or(Error::Unmapped(phys))?;
        let sig = Signature(hdr[..4].try_into().map_err(|_| Error::Unmapped(phys))?);
        let len = u32_at(hdr, 4).ok_or(Error::BadLength(sig))? as usize;
        if len < 36 {
            return Err(Error::BadLength(sig));
        }
        let data = mem.read(phys, len).ok_or(Error::Unmapped(phys))?;
        if !checksum_ok(data) {
            return Err(Error::BadChecksum(sig));
        }
        Ok(Sdt { phys, data })
    }

    #[must_use]
    pub fn signature(&self) -> Signature {
        Signature(self.data[..4].try_into().unwrap_or([0; 4]))
    }

    #[must_use]
    pub fn revision(&self) -> u8 {
        self.data[8]
    }

    #[must_use]
    pub fn oem_id(&self) -> &str {
        ascii(&self.data[10..16])
    }

    #[must_use]
    pub fn oem_table_id(&self) -> &str {
        ascii(&self.data[16..24])
    }

    /// The whole table, header included; offsets in the ACPI spec are
    /// relative to this.
    #[must_use]
    pub fn bytes(&self) -> &'a [u8] {
        self.data
    }

    fn expect(self, sig: Signature) -> Result<Self, Error> {
        if self.signature() == sig {
            Ok(self)
        } else {
            Err(Error::WrongSignature {
                expected: sig,
                found: self.signature(),
            })
        }
    }
}

/// The set of tables reachable from the RSDP.
#[derive(Debug)]
pub struct Acpi<'m, M: PhysMem> {
    mem: &'m M,
    pub rsdp: Rsdp,
    root: Sdt<'m>,
}

impl<'m, M: PhysMem> Acpi<'m, M> {
    /// Locates the XSDT (or the RSDT on ACPI 1.0) from the RSDP at `rsdp`.
    pub fn new(mem: &'m M, rsdp: u64) -> Result<Self, Error> {
        let r = Rsdp::read(mem, rsdp)?;
        let root = if r.xsdt != 0 {
            Sdt::read(mem, r.xsdt)?.expect(Signature::XSDT)?
        } else {
            Sdt::read(mem, u64::from(r.rsdt))?.expect(Signature::RSDT)?
        };
        Ok(Acpi { mem, rsdp: r, root })
    }

    /// Physical addresses of every table listed in the root table.
    pub fn table_addresses(&self) -> impl Iterator<Item = u64> + 'm {
        let body = &self.root.data[36..];
        let wide = self.root.signature() == Signature::XSDT;
        let step = if wide { 8 } else { 4 };
        body.chunks_exact(step).map(move |c| {
            if wide {
                u64_at(c, 0).unwrap_or(0)
            } else {
                u64::from(u32_at(c, 0).unwrap_or(0))
            }
        })
    }

    /// Every listed table, each read and checked; bad entries are yielded
    /// as errors so callers can log them and carry on.
    pub fn tables(&self) -> impl Iterator<Item = Result<Sdt<'m>, Error>> + 'm {
        let mem = self.mem;
        self.table_addresses().map(move |p| Sdt::read(mem, p))
    }

    /// The first valid table with signature `sig`.
    #[must_use]
    pub fn find(&self, sig: Signature) -> Option<Sdt<'m>> {
        self.tables().filter_map(Result::ok).find(|t| t.signature() == sig)
    }

    #[must_use]
    pub fn madt(&self) -> Option<Madt<'m>> {
        self.find(Signature::MADT).map(Madt::new)
    }

    #[must_use]
    pub fn fadt(&self) -> Option<Fadt<'m>> {
        self.find(Signature::FADT).map(Fadt::new)
    }

    #[must_use]
    pub fn spcr(&self) -> Option<Spcr> {
        self.find(Signature::SPCR).and_then(Spcr::parse)
    }

    #[must_use]
    pub fn hpet(&self) -> Option<Hpet> {
        self.find(Signature::HPET).and_then(Hpet::parse)
    }

    #[must_use]
    pub fn gtdt(&self) -> Option<Gtdt> {
        self.find(Signature::GTDT).and_then(Gtdt::parse)
    }

    pub fn dbg2(&self) -> impl Iterator<Item = Dbg2Device<'m>> + 'm {
        self.find(Signature::DBG2).into_iter().flat_map(dbg2_devices)
    }

    /// The PCI Express ECAM windows.
    pub fn mcfg(&self) -> impl Iterator<Item = McfgEntry> + 'm {
        self.find(Signature::MCFG).into_iter().flat_map(mcfg_entries)
    }

    #[must_use]
    pub fn pptt(&self) -> Option<Pptt<'m>> {
        self.find(Signature::PPTT).map(Pptt::new)
    }

    #[must_use]
    pub fn iort(&self) -> Option<Iort<'m>> {
        self.find(Signature::IORT).map(Iort::new)
    }
}

#[cfg(test)]
pub(crate) mod testmem {
    use super::*;
    use std::collections::BTreeMap;

    /// Sparse fake physical memory.
    #[derive(Debug, Default)]
    pub(crate) struct Mem(pub(crate) BTreeMap<u64, Vec<u8>>);

    impl PhysMem for Mem {
        fn read(&self, phys: u64, len: usize) -> Option<&[u8]> {
            let (base, buf) = self.0.range(..=phys).next_back()?;
            let off = usize::try_from(phys - base).ok()?;
            buf.get(off..off.checked_add(len)?)
        }
    }

    pub(crate) fn fix_checksum(t: &mut [u8], at: usize) {
        t[at] = 0;
        let s = t.iter().fold(0u8, |a, &x| a.wrapping_add(x));
        t[at] = 0u8.wrapping_sub(s);
    }

    /// A table with a standard header and the given body.
    pub(crate) fn sdt(sig: [u8; 4], rev: u8, body: &[u8]) -> Vec<u8> {
        let mut t = vec![0u8; 36];
        t[..4].copy_from_slice(&sig);
        t[8] = rev;
        t[10..16].copy_from_slice(b"PLANR ");
        t[16..24].copy_from_slice(b"TESTTBL ");
        t.extend_from_slice(body);
        let len = u32::try_from(t.len()).unwrap();
        t[4..8].copy_from_slice(&len.to_le_bytes());
        fix_checksum(&mut t, 9);
        t
    }

    pub(crate) fn rsdp(xsdt: u64) -> Vec<u8> {
        let mut r = vec![0u8; 36];
        r[..8].copy_from_slice(b"RSD PTR ");
        r[9..15].copy_from_slice(b"BOCHS ");
        r[15] = 2;
        r[20..24].copy_from_slice(&36u32.to_le_bytes());
        r[24..32].copy_from_slice(&xsdt.to_le_bytes());
        // The v1 checksum covers the first 20 bytes; the extended one all 36.
        fix_checksum(&mut r[..20], 8);
        fix_checksum(&mut r, 32);
        r
    }

    /// Memory holding an RSDP at 0x1000, an XSDT at 0x2000, and `tables`
    /// at 0x10000, 0x20000, ...
    pub(crate) fn system(tables: &[Vec<u8>]) -> Mem {
        let mut m = Mem::default();
        let addrs: Vec<u64> = (0..tables.len() as u64).map(|i| 0x10000 * (i + 1)).collect();
        let body: Vec<u8> = addrs.iter().flat_map(|a| a.to_le_bytes()).collect();
        m.0.insert(0x1000, rsdp(0x2000));
        m.0.insert(0x2000, sdt(*b"XSDT", 1, &body));
        for (a, t) in addrs.iter().zip(tables) {
            m.0.insert(*a, t.clone());
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::testmem::*;
    use super::*;

    #[test]
    fn walks_xsdt() {
        let m = system(&[sdt(*b"FACP", 6, &[0; 240]), sdt(*b"APIC", 5, &[0; 8])]);
        let acpi = Acpi::new(&m, 0x1000).unwrap();
        assert_eq!(acpi.rsdp.oem_id(), "BOCHS");
        let sigs: Vec<_> = acpi.tables().map(|t| t.unwrap().signature()).collect();
        assert_eq!(sigs, [Signature::FADT, Signature::MADT]);
        assert_eq!(acpi.find(Signature::MADT).unwrap().oem_table_id(), "TESTTBL");
        assert!(acpi.find(Signature::SPCR).is_none());
    }

    #[test]
    fn reports_bad_checksum_without_stopping() {
        let mut bad = sdt(*b"SSDT", 2, &[1, 2, 3]);
        bad[37] ^= 0xff;
        let m = system(&[bad, sdt(*b"APIC", 5, &[0; 8])]);
        let acpi = Acpi::new(&m, 0x1000).unwrap();
        let v: Vec<_> = acpi.tables().collect();
        assert_eq!(v[0].unwrap_err(), Error::BadChecksum(Signature(*b"SSDT")));
        assert!(v[1].is_ok());
    }

    #[test]
    fn rejects_bad_rsdp() {
        let mut m = system(&[]);
        m.0.get_mut(&0x1000).unwrap()[0] = b'X';
        assert_eq!(Acpi::new(&m, 0x1000).unwrap_err(), Error::BadRsdp);
        assert_eq!(Acpi::new(&m, 0x99_0000).unwrap_err(), Error::Unmapped(0x99_0000));
    }
}
