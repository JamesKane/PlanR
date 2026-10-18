//! The kernel symbol table that backtraces are symbolized from.
//!
//! The linker script reserves a zero-filled block in the kernel's rodata,
//! between `__ksyms_start` and `__ksyms_end`. After the link, `xtask`
//! reads the ELF symbol table, demangles the names and [`write`]s the
//! table into that block in the image file, so the symbols' addresses
//! cannot shift. The kernel [`Table::parse`]s the block; a kernel built
//! without `xtask` finds zeros there and prints bare addresses.
//!
//! The layout, all little-endian:
//!
//! - a 24-byte header: [`MAGIC`], the lowest symbol address (`u64`), the
//!   symbol count (`u32`) and the length of the name area (`u32`);
//! - one 12-byte entry per symbol, sorted by address: its offset from the
//!   lowest address, its size, and where its name starts in the name area
//!   (each a `u32`);
//! - the name area: each name is a `u16` length and that many UTF-8 bytes.

#![cfg_attr(not(test), no_std)]

use core::fmt;

pub const MAGIC: [u8; 8] = *b"PRKSYMS1";
const HEADER: usize = 24;
const ENTRY: usize = 12;

/// A symbol to [`write`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry<'a> {
    pub addr: u64,
    /// 0 if unknown (assembly labels): the symbol then covers everything up
    /// to the next one.
    pub size: u32,
    pub name: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The table needs `need` bytes; the block has `have`.
    TooBig { need: usize, have: usize },
    /// Entries must be sorted by address.
    Unsorted,
    /// A symbol lies more than 4 GiB past the first.
    TooFar,
    /// A name longer than 64 KiB.
    NameTooLong,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooBig { need, have } => write!(f, "symbol table needs {need} bytes; the block has {have}"),
            Error::Unsorted => f.write_str("symbols not sorted by address"),
            Error::TooFar => f.write_str("symbols span more than 4 GiB"),
            Error::NameTooLong => f.write_str("symbol name longer than 64 KiB"),
        }
    }
}

/// The bytes [`write`] needs for `entries`.
#[must_use]
pub fn encoded_len(entries: &[Entry<'_>]) -> usize {
    HEADER + entries.iter().map(|e| ENTRY + 2 + e.name.len()).sum::<usize>()
}

/// Encodes `entries`, which must be sorted by address, at the start of
/// `out`, and returns the length used. The rest of `out` is left alone.
pub fn write(entries: &[Entry<'_>], out: &mut [u8]) -> Result<usize, Error> {
    let need = encoded_len(entries);
    if need > out.len() {
        return Err(Error::TooBig { need, have: out.len() });
    }
    if entries.windows(2).any(|w| w[0].addr > w[1].addr) {
        return Err(Error::Unsorted);
    }
    let base = entries.first().map_or(0, |e| e.addr);
    let count = u32::try_from(entries.len()).map_err(|_| Error::TooFar)?;
    let names_at = HEADER + entries.len() * ENTRY;
    let names_len = u32::try_from(need - names_at).map_err(|_| Error::TooFar)?;
    out[..8].copy_from_slice(&MAGIC);
    out[8..16].copy_from_slice(&base.to_le_bytes());
    out[16..20].copy_from_slice(&count.to_le_bytes());
    out[20..24].copy_from_slice(&names_len.to_le_bytes());
    let mut name = 0;
    for (i, e) in entries.iter().enumerate() {
        let offset = u32::try_from(e.addr - base).map_err(|_| Error::TooFar)?;
        let len = u16::try_from(e.name.len()).map_err(|_| Error::NameTooLong)?;
        let o = HEADER + i * ENTRY;
        out[o..o + 4].copy_from_slice(&offset.to_le_bytes());
        out[o + 4..o + 8].copy_from_slice(&e.size.to_le_bytes());
        // `name` is below `names_len`, which fits a u32.
        #[allow(clippy::cast_possible_truncation)]
        out[o + 8..o + 12].copy_from_slice(&(name as u32).to_le_bytes());
        let n = names_at + name;
        out[n..n + 2].copy_from_slice(&len.to_le_bytes());
        out[n + 2..n + 2 + e.name.len()].copy_from_slice(e.name.as_bytes());
        name += 2 + e.name.len();
    }
    Ok(need)
}

/// A symbol found by [`Table::lookup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Symbol<'a> {
    pub name: &'a str,
    pub addr: u64,
}

/// A parsed table. Parsing checks only the header; each lookup checks
/// the entries it reads, so a corrupt table yields `None`, never a panic.
#[derive(Debug, Clone, Copy)]
pub struct Table<'a> {
    base: u64,
    entries: &'a [u8],
    names: &'a [u8],
}

fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?))
}

impl<'a> Table<'a> {
    /// The table at the start of `data`, or `None` if there is none.
    #[must_use]
    pub fn parse(data: &'a [u8]) -> Option<Self> {
        if data.get(..8)? != MAGIC {
            return None;
        }
        let base = u64::from_le_bytes(data.get(8..16)?.try_into().ok()?);
        let count = usize::try_from(u32_at(data, 16)?).ok()?;
        let names_len = usize::try_from(u32_at(data, 20)?).ok()?;
        let names_at = count.checked_mul(ENTRY)?.checked_add(HEADER)?;
        Some(Table {
            base,
            entries: data.get(HEADER..names_at)?,
            names: data.get(names_at..names_at.checked_add(names_len)?)?,
        })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len() / ENTRY
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entry `i`'s address and size.
    fn span(&self, i: usize) -> Option<(u64, u32)> {
        let offset = u32_at(self.entries, i * ENTRY)?;
        Some((
            self.base.checked_add(u64::from(offset))?,
            u32_at(self.entries, i * ENTRY + 4)?,
        ))
    }

    fn name(&self, i: usize) -> Option<&'a str> {
        let at = usize::try_from(u32_at(self.entries, i * ENTRY + 8)?).ok()?;
        let len = usize::from(u16::from_le_bytes(self.names.get(at..at + 2)?.try_into().ok()?));
        core::str::from_utf8(self.names.get(at + 2..at + 2 + len)?).ok()
    }

    /// The symbol containing `addr`: the last one at or below it, if `addr`
    /// is within its size, or, for a symbol of unknown size, below the next
    /// symbol.
    #[must_use]
    pub fn lookup(&self, addr: u64) -> Option<Symbol<'a>> {
        // The number of symbols at or below `addr`.
        let (mut lo, mut hi) = (0, self.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.span(mid)?.0 <= addr {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let i = lo.checked_sub(1)?;
        let (start, size) = self.span(i)?;
        let within = if size == 0 {
            self.span(i + 1).is_none_or(|(next, _)| addr < next)
        } else {
            addr - start < u64::from(size)
        };
        within.then_some(Symbol {
            name: self.name(i)?,
            addr: start,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENTRIES: &[Entry<'_>] = &[
        Entry {
            addr: 0xffff_ffff_8000_0000,
            size: 0,
            name: "_start",
        },
        Entry {
            addr: 0xffff_ffff_8000_0100,
            size: 0x40,
            name: "kernel::kmain",
        },
        Entry {
            addr: 0xffff_ffff_8000_0200,
            size: 0x10,
            name: "<core::option::Option<u64>>::map::<u32, kernel::f::{closure#0}>",
        },
    ];

    fn encode(entries: &[Entry<'_>]) -> Vec<u8> {
        let mut out = vec![0; encoded_len(entries) + 7];
        assert_eq!(write(entries, &mut out), Ok(encoded_len(entries)));
        out
    }

    fn name_at(t: &Table<'_>, addr: u64) -> Option<(&'static str, u64)> {
        t.lookup(addr).map(|s| {
            let name = ENTRIES.iter().find(|e| e.name == s.name).unwrap().name;
            (name, addr - s.addr)
        })
    }

    #[test]
    fn looks_up_by_address() {
        let data = encode(ENTRIES);
        let t = Table::parse(&data).unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(name_at(&t, 0xffff_ffff_7fff_ffff), None);
        assert_eq!(name_at(&t, 0xffff_ffff_8000_0000), Some(("_start", 0)));
        // Unknown size: covers everything up to the next symbol.
        assert_eq!(name_at(&t, 0xffff_ffff_8000_00ff), Some(("_start", 0xff)));
        assert_eq!(name_at(&t, 0xffff_ffff_8000_0100), Some(("kernel::kmain", 0)));
        assert_eq!(name_at(&t, 0xffff_ffff_8000_013f), Some(("kernel::kmain", 0x3f)));
        // The gap after a sized symbol belongs to nothing.
        assert_eq!(name_at(&t, 0xffff_ffff_8000_0140), None);
        assert_eq!(name_at(&t, 0xffff_ffff_8000_020f), Some((ENTRIES[2].name, 0xf)));
        assert_eq!(name_at(&t, 0xffff_ffff_8000_0210), None);
    }

    #[test]
    fn last_symbol_of_unknown_size_is_open_ended() {
        let data = encode(&ENTRIES[..1]);
        let t = Table::parse(&data).unwrap();
        assert_eq!(t.lookup(u64::MAX).map(|s| s.name), Some("_start"));
    }

    #[test]
    fn empty_and_absent_tables() {
        let data = encode(&[]);
        let t = Table::parse(&data).unwrap();
        assert!(t.is_empty());
        assert_eq!(t.lookup(0x1234), None);
        // An unwritten block is all zeros.
        assert!(Table::parse(&[0; 64]).is_none());
        assert!(Table::parse(&[]).is_none());
    }

    #[test]
    fn write_checks_its_input() {
        let mut small = [0; 30];
        assert_eq!(
            write(ENTRIES, &mut small),
            Err(Error::TooBig {
                need: encoded_len(ENTRIES),
                have: 30
            })
        );
        let mut out = vec![0; 4096];
        let unsorted = [ENTRIES[1], ENTRIES[0]];
        assert_eq!(write(&unsorted, &mut out), Err(Error::Unsorted));
        let far = [
            Entry {
                addr: 0x1000,
                ..ENTRIES[0]
            },
            Entry {
                addr: 0x1000 + (1 << 32),
                ..ENTRIES[1]
            },
        ];
        assert_eq!(write(&far, &mut out), Err(Error::TooFar));
    }

    #[test]
    fn never_panics_on_corruption() {
        let data = encode(ENTRIES);
        for i in 8..data.len() {
            for b in [0x00, 0x7f, 0xff] {
                let mut d = data.clone();
                d[i] = b;
                if let Some(t) = Table::parse(&d) {
                    for e in ENTRIES {
                        let _ = t.lookup(e.addr + 1);
                    }
                }
            }
        }
        for n in 0..data.len() {
            if let Some(t) = Table::parse(&data[..n]) {
                let _ = t.lookup(ENTRIES[2].addr);
            }
        }
    }
}
