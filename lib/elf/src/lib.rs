//! ELF64 parsing: just what 9boot needs to load a static kernel, plus the
//! symbol table, which `xtask` reads to build the kernel's backtrace
//! symbols.
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

/// A section header. Loading ignores sections; they matter only for the
/// symbol table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section {
    pub kind: u32,
    pub flags: u64,
    pub addr: u64,
    pub offset: u64,
    pub size: u64,
    pub link: u32,
    pub entsize: u64,
}

/// `st_info`'s type field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    NoType,
    Object,
    Func,
    Section,
    File,
    Other(u8),
}

/// An entry in the `SHT_SYMTAB` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Symbol<'a> {
    pub name: &'a [u8],
    pub value: u64,
    pub size: u64,
    pub kind: SymbolKind,
    /// `st_shndx`: the section the symbol is defined in; 0 is undefined.
    pub section: u16,
}

const PT_LOAD: u32 = 1;
const ET_EXEC: u16 = 2;
const PHDR_SIZE: usize = 56;
pub const SHT_SYMTAB: u32 = 2;
const SHDR_SIZE: usize = 64;
const SYM_SIZE: usize = 24;

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

    /// The section headers, or none if the table is missing or malformed.
    pub fn sections(&self) -> impl Iterator<Item = Section> + '_ {
        let d = self.data;
        let table = (|| {
            let off = usize::try_from(u64_at(d, 40)?).ok()?;
            let entsize = usize::from(u16_at(d, 58)?);
            let num = usize::from(u16_at(d, 60)?);
            let end = num.checked_mul(entsize)?.checked_add(off)?;
            (entsize >= SHDR_SIZE && end <= d.len()).then_some((off, entsize, num))
        })();
        let (off, entsize, num) = table.unwrap_or((0, 0, 0));
        (0..num).filter_map(move |i| {
            let o = off + i * entsize;
            Some(Section {
                kind: u32_at(d, o + 4)?,
                flags: u64_at(d, o + 8)?,
                addr: u64_at(d, o + 16)?,
                offset: u64_at(d, o + 24)?,
                size: u64_at(d, o + 32)?,
                link: u32_at(d, o + 40)?,
                entsize: u64_at(d, o + 56)?,
            })
        })
    }

    /// The file bytes of a section, if they lie within the file.
    #[must_use]
    pub fn section_data(&self, s: &Section) -> Option<&'a [u8]> {
        let start = usize::try_from(s.offset).ok()?;
        let end = start.checked_add(usize::try_from(s.size).ok()?)?;
        self.data.get(start..end)
    }

    /// The entries of the first `SHT_SYMTAB` section, or none if there is
    /// no symbol table. Names are cut at their NUL; a name outside the
    /// string table reads as empty.
    pub fn symbols(&self) -> impl Iterator<Item = Symbol<'a>> + '_ {
        let tables = self.sections().find(|s| s.kind == SHT_SYMTAB).and_then(|symtab| {
            let strtab = self.sections().nth(usize::try_from(symtab.link).ok()?)?;
            let syms = self.section_data(&symtab)?;
            let entsize = usize::try_from(symtab.entsize).ok().filter(|&n| n >= SYM_SIZE)?;
            Some((syms, self.section_data(&strtab)?, entsize))
        });
        let (syms, strs, entsize) = tables.unwrap_or((&[], &[], SYM_SIZE));
        (0..syms.len() / entsize).filter_map(move |i| {
            let o = i * entsize;
            let name = usize::try_from(u32_at(syms, o)?)
                .ok()
                .and_then(|n| strs.get(n..))
                .unwrap_or(&[]);
            let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
            Some(Symbol {
                name,
                kind: match syms[o + 4] & 0xf {
                    0 => SymbolKind::NoType,
                    1 => SymbolKind::Object,
                    2 => SymbolKind::Func,
                    3 => SymbolKind::Section,
                    4 => SymbolKind::File,
                    t => SymbolKind::Other(t),
                },
                section: u16_at(syms, o + 6)?,
                value: u64_at(syms, o + 8)?,
                size: u64_at(syms, o + 16)?,
            })
        })
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

    /// Appends a symbol table of `(name, value, size, kind)` entries, its
    /// string table and a section header table to `f`.
    fn with_symbols(mut f: Vec<u8>, syms: &[(&str, u64, u64, u8)]) -> Vec<u8> {
        let mut strs = vec![0u8];
        let mut table = vec![0u8; SYM_SIZE]; // the null symbol
        for &(name, value, size, kind) in syms {
            let mut e = [0u8; SYM_SIZE];
            e[..4].copy_from_slice(&(strs.len() as u32).to_le_bytes());
            e[4] = 0x10 | kind; // global
            e[6..8].copy_from_slice(&1u16.to_le_bytes());
            e[8..16].copy_from_slice(&value.to_le_bytes());
            e[16..24].copy_from_slice(&size.to_le_bytes());
            table.extend_from_slice(&e);
            strs.extend_from_slice(name.as_bytes());
            strs.push(0);
        }
        let symoff = f.len() as u64;
        f.extend_from_slice(&table);
        let stroff = f.len() as u64;
        f.extend_from_slice(&strs);
        let shoff = f.len() as u64;
        // Null, .symtab (linked to section 2), .strtab.
        let headers = [
            (0u32, 0u64, 0u64, 0u32, 0u64),
            (SHT_SYMTAB, symoff, table.len() as u64, 2, SYM_SIZE as u64),
            (3, stroff, strs.len() as u64, 0, 0),
        ];
        for (kind, offset, size, link, entsize) in headers {
            let mut h = [0u8; SHDR_SIZE];
            h[4..8].copy_from_slice(&kind.to_le_bytes());
            h[24..32].copy_from_slice(&offset.to_le_bytes());
            h[32..40].copy_from_slice(&size.to_le_bytes());
            h[40..44].copy_from_slice(&link.to_le_bytes());
            h[56..64].copy_from_slice(&entsize.to_le_bytes());
            f.extend_from_slice(&h);
        }
        f[40..48].copy_from_slice(&shoff.to_le_bytes());
        f[58..60].copy_from_slice(&(SHDR_SIZE as u16).to_le_bytes());
        f[60..62].copy_from_slice(&3u16.to_le_bytes());
        f
    }

    #[test]
    fn reads_symbols() {
        let f = with_symbols(
            build(62, ET_EXEC, &[(0x1000, b"code", 4, 5)]),
            &[("kmain", 0x1000, 3, 2), ("__kernel_start", 0x1000, 0, 0)],
        );
        let e = Elf::parse(&f, Machine::X86_64).unwrap();
        assert_eq!(e.sections().count(), 3);
        let syms: Vec<_> = e.symbols().collect();
        assert_eq!(syms.len(), 3);
        assert_eq!(syms[0].name, b"");
        assert_eq!(
            syms[1],
            Symbol {
                name: b"kmain",
                value: 0x1000,
                size: 3,
                kind: SymbolKind::Func,
                section: 1,
            }
        );
        assert_eq!(syms[2].name, b"__kernel_start");
        assert_eq!(syms[2].kind, SymbolKind::NoType);
    }

    #[test]
    fn no_section_headers_means_no_symbols() {
        let f = build(62, ET_EXEC, &[(0x1000, b"code", 4, 5)]);
        let e = Elf::parse(&f, Machine::X86_64).unwrap();
        assert_eq!(e.sections().count(), 0);
        assert_eq!(e.symbols().count(), 0);
    }

    #[test]
    fn never_panics_on_truncation() {
        let f = build(62, ET_EXEC, &[(0x1000, b"abcdefgh", 8, 5), (0x3000, b"ij", 2, 6)]);
        for n in 0..f.len() {
            let _ = Elf::parse(&f[..n], Machine::X86_64);
        }
        // Section and symbol tables cut short read as fewer entries.
        let f = with_symbols(f, &[("a", 0x1000, 1, 2), ("b", 0x1001, 1, 2)]);
        for n in 0..f.len() {
            let mut g = f[..n].to_vec();
            g.resize(f.len(), 0xff);
            if let Ok(e) = Elf::parse(&g, Machine::X86_64) {
                let _ = e.symbols().count();
            }
        }
    }
}
