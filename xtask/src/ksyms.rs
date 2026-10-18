//! Writes the kernel's backtrace symbols into the block its linker script
//! reserves (see `lib/ksyms`). The table goes into a copy of the linked
//! image, so cargo's own output stays as it built it.

use std::cmp::Reverse;
use std::fs;
use std::path::Path;

use elf::{Elf, SymbolKind};

use crate::arch::Arch;
use crate::demangle::demangle;

pub(crate) fn embed(arch: Arch, kernel: &Path, out: &Path) -> Result<(), String> {
    let what = kernel.display();
    let mut image = fs::read(kernel).map_err(|e| format!("{what}: {e}"))?;
    let elf = Elf::parse(&image, arch.machine()).map_err(|e| format!("{what}: {e}"))?;
    let text: Vec<_> = elf.segments().filter(|s| s.perms.exec).collect();
    let in_text = |a: u64| text.iter().any(|s| (s.vaddr..s.vaddr + s.memsz).contains(&a));
    let (mut start, mut end) = (None, None);
    let mut syms = Vec::new();
    for s in elf.symbols() {
        let Ok(name) = std::str::from_utf8(s.name) else {
            continue;
        };
        match name {
            "__ksyms_start" => start = Some(s.value),
            "__ksyms_end" => end = Some(s.value),
            _ => {}
        }
        // Skip arm64's `$x`/`$d` mapping symbols and assembler-local labels.
        let label = name.is_empty() || name.starts_with('$') || name.starts_with(".L");
        if matches!(s.kind, SymbolKind::Func | SymbolKind::NoType) && s.section != 0 && !label && in_text(s.value) {
            let size = u32::try_from(s.size).map_err(|_| format!("{what}: {name} is over 4 GiB"))?;
            syms.push((s.value, size, s.kind == SymbolKind::Func, name));
        }
    }
    // One name per address: a function over a label, then the larger, then
    // a name not starting `__` (linker-script markers).
    syms.sort_by_key(|&(addr, size, func, name)| (addr, Reverse(func), Reverse(size), name.starts_with("__")));
    syms.dedup_by_key(|s| s.0);
    let names: Vec<String> = syms.iter().map(|s| demangle(s.3)).collect();
    let entries: Vec<_> = syms
        .iter()
        .zip(&names)
        .map(|(&(addr, size, ..), name)| ksyms::Entry { addr, size, name })
        .collect();
    let (Some(start), Some(end)) = (start, end) else {
        return Err(format!("{what}: no __ksyms_start/__ksyms_end; is link.ld current?"));
    };
    let block = elf
        .segments()
        .find(|s| s.vaddr <= start && end <= s.vaddr + s.filesz)
        .and_then(|s| usize::try_from(s.offset + (start - s.vaddr)).ok())
        .ok_or_else(|| format!("{what}: the symbol block is not in the file"))?;
    let len = usize::try_from(end - start).map_err(|e| e.to_string())?;
    let used = ksyms::write(&entries, &mut image[block..block + len])
        .map_err(|e| format!("{what}: {e}; raise the block size in link.ld"))?;
    fs::write(out, &image).map_err(|e| format!("{}: {e}", out.display()))?;
    println!(
        "xtask: {arch} kernel symbols: {} in {} of {} KiB",
        entries.len(),
        used.div_ceil(1024),
        len / 1024
    );
    Ok(())
}
