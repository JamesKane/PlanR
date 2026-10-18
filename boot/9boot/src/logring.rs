//! The persistent log ring (`lib/logring`): finds what the previous boot
//! left in RAM, saves it to the boot volume, and reserves a fresh ring for
//! the kernel.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;

use bootinfo::{LogRing, uefi_type};
use logring::{Found, RING_BYTES, Ring};
use uefi::{Boot, File, MemoryType, Status, SystemTable};

/// Where the previous boot's log is saved on the boot volume. 9boot only
/// overwrites this file, never creates it: the Plan R media `cargo xtask`
/// builds carry an empty one, so 9boot started from any other volume (a
/// shared ESP, say) writes nothing there (HARDWARE.md, storage safety).
pub(crate) const SAVED_PATH: &str = "planr\\last.log";
const PAGE: u64 = 4096;
/// The longest line of the saved log echoed on the console.
const ECHO_MAX: usize = 120;

/// Saves the previous boot's ring, if one survived, and reserves a new one.
/// Returns the ring and this boot's number in it (1 for a fresh ring).
/// Problems are reported and leave the kernel without a ring; none stops
/// the boot.
pub(crate) fn setup(st: &SystemTable<Boot>, root: &File<'_>) -> Option<(LogRing, u64)> {
    let bs = st.boot_services();
    let mut buf = vec![0u8; bs.memory_map_size() * 2 + 4096];
    let free: Vec<(u64, u64)> = bs
        .memory_map(&mut buf)
        .ok()?
        .iter()
        .filter(|d| d.ty == MemoryType::Conventional)
        .map(|d| (d.phys_start, d.phys_start + d.bytes()))
        .collect();
    let previous = free
        .iter()
        .flat_map(|&(start, end)| logring::sites(start, end))
        .filter_map(|a| Some((a, open(a)?.found()?)))
        .max_by_key(|(_, f)| f.boot);

    // Keep the previous ring's place if it is still free, so its contents
    // stay put while they are saved.
    let pages = usize::try_from(RING_BYTES / PAGE).ok()?;
    let reserve = |a| bs.allocate_pages_at(MemoryType::Other(uefi_type::LOG), pages, a).ok();
    let phys = match previous.and_then(|(a, _)| reserve(a)) {
        Some(a) => a,
        None => logring::place(&free).and_then(reserve).or_else(|| {
            let _ = writeln!(st.stdout(), "9boot: no room for the log ring");
            None
        })?,
    };
    if let Some((at, found)) = previous {
        save(st, root, &open(at)?, found);
    }
    let mut ring = open(phys)?;
    let boot = previous.map_or(1, |(_, f)| f.boot + 1);
    ring.reset(boot);
    Some((LogRing { phys, size: RING_BYTES }, boot))
}

/// A ring view of the block at `phys`.
fn open(phys: u64) -> Option<Ring> {
    // SAFETY: UEFI identity-maps RAM, and `phys` is a site in free RAM or
    // our own allocation; nothing else uses it while boot services run us.
    unsafe { Ring::new(phys as *mut u8, usize::try_from(RING_BYTES).ok()?) }
}

/// Writes what `ring` holds to [`SAVED_PATH`] and reports how it ends.
fn save(st: &SystemTable<Boot>, root: &File<'_>, ring: &Ring, found: Found) {
    let (a, b) = ring.contents();
    let saved = root.rewrite(SAVED_PATH).and_then(|mut f| {
        f.write_all(a)?;
        f.write_all(b)?;
        f.flush()
    });
    let mut out = st.stdout();
    let n = a.len() + b.len();
    let _ = match saved {
        Ok(()) => writeln!(
            out,
            "9boot: saved boot {}'s log ({n} of {} bytes) to \\{SAVED_PATH}",
            found.boot, found.written
        ),
        Err(Status::NOT_FOUND) => writeln!(
            out,
            "9boot: boot {}'s log ({n} bytes) not saved: this volume has no \\{SAVED_PATH}",
            found.boot
        ),
        Err(e) => writeln!(out, "9boot: boot {}'s log ({n} bytes) not saved: {e}", found.boot),
    };
    if let Some(line) = last_line(a, b) {
        let _ = writeln!(out, "9boot: it ends: {line}");
    }
}

/// The last non-blank line of `a` then `b`, as printable ASCII.
fn last_line(a: &[u8], b: &[u8]) -> Option<alloc::string::String> {
    let all: Vec<u8> = [a, b].concat();
    let line = all
        .split(|&c| c == b'\n')
        .rfind(|l| l.iter().any(|c| !c.is_ascii_whitespace()))?;
    Some(
        line.iter()
            .take(ECHO_MAX)
            .map(|&c| {
                if c.is_ascii_graphic() || c == b' ' {
                    char::from(c)
                } else {
                    '?'
                }
            })
            .collect(),
    )
}
