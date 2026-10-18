//! ELF64 parsing: just what 9boot needs to load a static kernel.
//!
//! Everything is read with explicit little-endian decoding from a byte
//! slice, so malformed input yields an [`Error`], never a misaligned read or
//! an out-of-bounds access.

#![cfg_attr(not(test), no_std)]

use core::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    TooShort,
    BadMagic,
    Not64Bit,
    NotLittleEndian,
    BadVersion,
    /// `e_type` other than `ET_EXEC` (static, non-PIE).
    NotExecutable(u16),
    WrongMachine {
        expected: Machine,
        found: u16,
    },
    BadProgramHeaders,
    /// A segment's file data lies outside the file, or `filesz > memsz`.
    BadSegment(usize),
    NoLoadableSegments,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooShort => f.write_str("file too short"),
            Error::BadMagic => f.write_str("not an ELF file"),
            Error::Not64Bit => f.write_str("not ELF64"),
            Error::NotLittleEndian => f.write_str("not little-endian"),
            Error::BadVersion => f.write_str("unknown ELF version"),
            Error::NotExecutable(t) => write!(f, "e_type {t}, want ET_EXEC"),
            Error::WrongMachine { expected, found } => write!(f, "machine {found}, want {expected:?}"),
            Error::BadProgramHeaders => f.write_str("bad program header table"),
            Error::BadSegment(i) => write!(f, "segment {i} is malformed"),
            Error::NoLoadableSegments => f.write_str("no PT_LOAD segments"),
        }
    }
}

/// Supported `e_machine` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Machine {
    X86_64,
    Aarch64,
}

impl Machine {
    #[must_use]
    pub fn code(self) -> u16 {
        match self {
            Machine::X86_64 => 62,
            Machine::Aarch64 => 183,
        }
    }
}

/// Segment permissions (`p_flags`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Perms {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
}

/// A `PT_LOAD` segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub vaddr: u64,
    pub offset: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub perms: Perms,
}

/// A validated ELF64 executable.
#[derive(Debug, Clone, Copy)]
pub struct Elf<'a> {
    data: &'a [u8],
    entry: u64,
    phoff: usize,
    phentsize: usize,
    phnum: usize,
}

const PT_LOAD: u32 = 1;
const ET_EXEC: u16 = 2;
const PHDR_SIZE: usize = 56;

fn u16_at(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(o..o + 2)?.try_into().ok()?))
}
fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?))
}
fn u64_at(d: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(d.get(o..o + 8)?.try_into().ok()?))
}

impl<'a> Elf<'a> {
    /// Parses and validates the header and every `PT_LOAD` segment.
    pub fn parse(data: &'a [u8], machine: Machine) -> Result<Self, Error> {
        if data.len() < 64 {
            return Err(Error::TooShort);
        }
        if data[..4] != *b"\x7fELF" {
            return Err(Error::BadMagic);
        }
        if data[4] != 2 {
            return Err(Error::Not64Bit);
        }
        if data[5] != 1 {
            return Err(Error::NotLittleEndian);
        }
        if data[6] != 1 {
            return Err(Error::BadVersion);
        }
        let short = Error::TooShort;
        let ty = u16_at(data, 16).ok_or(short)?;
        if ty != ET_EXEC {
            return Err(Error::NotExecutable(ty));
        }
        let found = u16_at(data, 18).ok_or(short)?;
        if found != machine.code() {
            return Err(Error::WrongMachine {
                expected: machine,
                found,
            });
        }
        let entry = u64_at(data, 24).ok_or(short)?;
        let phoff = usize::try_from(u64_at(data, 32).ok_or(short)?).map_err(|_| Error::BadProgramHeaders)?;
        let phentsize = usize::from(u16_at(data, 54).ok_or(short)?);
        let phnum = usize::from(u16_at(data, 56).ok_or(short)?);
        let table_end = phnum.checked_mul(phentsize).and_then(|n| n.checked_add(phoff));
        if phentsize < PHDR_SIZE || table_end.is_none_or(|e| e > data.len()) {
            return Err(Error::BadProgramHeaders);
        }
        let elf = Elf {
            data,
            entry,
            phoff,
            phentsize,
            phnum,
        };
        let mut any = false;
        for (i, s) in elf.segments().enumerate() {
            let end = s.offset.checked_add(s.filesz);
            if s.filesz > s.memsz || end.is_none_or(|e| e > data.len() as u64) || s.vaddr.checked_add(s.memsz).is_none()
            {
                return Err(Error::BadSegment(i));
            }
            any = true;
        }
        if any { Ok(elf) } else { Err(Error::NoLoadableSegments) }
    }

    #[must_use]
    pub fn entry(&self) -> u64 {
        self.entry
    }

    /// The `PT_LOAD` segments, in file order.
    pub fn segments(&self) -> impl Iterator<Item = Segment> + '_ {
        (0..self.phnum).filter_map(move |i| {
            let o = self.phoff + i * self.phentsize;
            let d = self.data;
            if u32_at(d, o)? != PT_LOAD {
                return None;
            }
            let flags = u32_at(d, o + 4)?;
            Some(Segment {
                offset: u64_at(d, o + 8)?,
                vaddr: u64_at(d, o + 16)?,
                filesz: u64_at(d, o + 32)?,
                memsz: u64_at(d, o + 40)?,
                perms: Perms {
                    exec: flags & 1 != 0,
                    write: flags & 2 != 0,
                    read: flags & 4 != 0,
                },
            })
        })
    }

    /// The file bytes of a segment (validated in `parse`).
    #[must_use]
    pub fn file_data(&self, s: &Segment) -> &'a [u8] {
        #[allow(clippy::cast_possible_truncation)] // bounded by data.len() in `parse`
        let (o, n) = (s.offset as usize, s.filesz as usize);
        &self.data[o..o + n]
    }

    /// The virtual address span `[lo, hi)` covered by all segments.
    #[must_use]
    pub fn span(&self) -> (u64, u64) {
        self.segments().fold((u64::MAX, 0), |(lo, hi), s| {
            (lo.min(s.vaddr), hi.max(s.vaddr + s.memsz))
        })
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)] // test fixtures are tiny
mod tests {
    use super::*;

    /// Builds a minimal ELF64 executable with the given segments
    /// `(vaddr, data, memsz, flags)`.
    fn build(machine: u16, ty: u16, segs: &[(u64, &[u8], u64, u32)]) -> Vec<u8> {
        let phoff = 64usize;
        let data_start = phoff + segs.len() * PHDR_SIZE;
        let mut f = vec![0u8; data_start];
        f[..4].copy_from_slice(b"\x7fELF");
        f[4] = 2;
        f[5] = 1;
        f[6] = 1;
        f[16..18].copy_from_slice(&ty.to_le_bytes());
        f[18..20].copy_from_slice(&machine.to_le_bytes());
        f[24..32].copy_from_slice(&0xffff_ffff_8000_0000u64.to_le_bytes());
        f[32..40].copy_from_slice(&(phoff as u64).to_le_bytes());
        f[54..56].copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
        f[56..58].copy_from_slice(&(segs.len() as u16).to_le_bytes());
        for (i, (vaddr, data, memsz, flags)) in segs.iter().enumerate() {
            let off = f.len() as u64;
            f.extend_from_slice(data);
            let p = phoff + i * PHDR_SIZE;
            f[p..p + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
            f[p + 4..p + 8].copy_from_slice(&flags.to_le_bytes());
            f[p + 8..p + 16].copy_from_slice(&off.to_le_bytes());
            f[p + 16..p + 24].copy_from_slice(&vaddr.to_le_bytes());
            f[p + 32..p + 40].copy_from_slice(&(data.len() as u64).to_le_bytes());
            f[p + 40..p + 48].copy_from_slice(&memsz.to_le_bytes());
        }
        f
    }

    #[test]
    fn parses_segments_and_span() {
        let f = build(
            62,
            ET_EXEC,
            &[
                (0xffff_ffff_8000_0000, b"code", 4, 5),
                (0xffff_ffff_8000_2000, b"dat", 0x100, 6),
            ],
        );
        let e = Elf::parse(&f, Machine::X86_64).unwrap();
        let segs: Vec<_> = e.segments().collect();
        assert_eq!(segs.len(), 2);
        assert!(segs[0].perms.exec && !segs[0].perms.write);
        assert!(segs[1].perms.write && !segs[1].perms.exec);
        assert_eq!(e.file_data(&segs[1]), b"dat");
        assert_eq!(e.span(), (0xffff_ffff_8000_0000, 0xffff_ffff_8000_2100));
        assert_eq!(e.entry(), 0xffff_ffff_8000_0000);
    }

    #[test]
    fn rejects_wrong_machine_and_type() {
        let f = build(62, ET_EXEC, &[(0x1000, b"x", 1, 5)]);
        assert!(matches!(
            Elf::parse(&f, Machine::Aarch64),
            Err(Error::WrongMachine { .. })
        ));
        let pie = build(183, 3, &[(0x1000, b"x", 1, 5)]);
        assert_eq!(Elf::parse(&pie, Machine::Aarch64).unwrap_err(), Error::NotExecutable(3));
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(Elf::parse(b"short", Machine::X86_64).unwrap_err(), Error::TooShort);
        let mut f = build(62, ET_EXEC, &[(0x1000, b"abcd", 4, 5)]);
        // filesz > memsz
        f[64 + 40..64 + 48].copy_from_slice(&1u64.to_le_bytes());
        assert_eq!(Elf::parse(&f, Machine::X86_64).unwrap_err(), Error::BadSegment(0));
        let none = build(62, ET_EXEC, &[]);
        assert_eq!(
            Elf::parse(&none, Machine::X86_64).unwrap_err(),
            Error::NoLoadableSegments
        );
        let mut huge = build(62, ET_EXEC, &[(0x1000, b"x", 1, 5)]);
        huge[56..58].copy_from_slice(&1000u16.to_le_bytes());
        assert_eq!(
            Elf::parse(&huge, Machine::X86_64).unwrap_err(),
            Error::BadProgramHeaders
        );
    }

    #[test]
    fn never_panics_on_truncation() {
        let f = build(62, ET_EXEC, &[(0x1000, b"abcdefgh", 8, 5), (0x3000, b"ij", 2, 6)]);
        for n in 0..f.len() {
            let _ = Elf::parse(&f[..n], Machine::X86_64);
        }
    }
}
