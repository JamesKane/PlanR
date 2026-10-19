//! Booting Plan R a second time. `test=reset` checks that the log ring
//! survives the kernel's warm reset, which needs 9boot to run again
//! afterwards. A board booted through `BootNext` would instead fall back
//! to its own OS, so on the first boot 9boot sets `BootNext` to the entry
//! it was started from. That is the only variable it writes.
//!
//! The first boot also leaves a marker in `\planr\last.log` saying where
//! its ring was. The second boot overwrites it when it saves the surviving
//! ring. If a boot instead finds the marker and no ring, the ring was
//! lost: 9boot reports what that memory holds now, replaces the marker
//! with the report, and stops instead of starting the kernel, which would
//! reset again. Without the marker, a lost ring makes every boot look
//! like the first, and the board loops.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use logring::MAGIC;
use uefi::{Boot, File, MemoryType, Result, Status, SystemTable, guid, ucs2, var};

use crate::logring::SAVED_PATH;

const BOOT_CURRENT: [u16; 12] = ucs2("BootCurrent");
const BOOT_NEXT: [u16; 9] = ucs2("BootNext");
const MARKER: &str = "planr: reset test pending; ring at ";

/// True if `plan9.ini` sets `key` to `value`, the last setting winning, as
/// the kernel reads it (`kernel/src/port/config.rs`).
pub(crate) fn config_is(config: &[u8], key: &str, value: &str) -> bool {
    let text = core::str::from_utf8(config).unwrap_or("");
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim())
        .next_back()
        == Some(value)
}

/// True if `plan9.ini`'s last `key` setting lists `word` among its
/// comma-separated values (`bootdebug=exittest`).
pub(crate) fn config_lists(config: &[u8], key: &str, word: &str) -> bool {
    let text = core::str::from_utf8(config).unwrap_or("");
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim())
        .next_back()
        .is_some_and(|v| v.split(',').any(|w| w.trim() == word))
}

/// What the first boot of a reset test recorded in its marker.
pub(crate) struct Pending {
    pub(crate) ring: u64,
    /// Pages holding [`canary`] patterns, spread over RAM.
    pub(crate) canaries: Vec<u64>,
}

/// The first boot's marker, if `last.log` holds one.
pub(crate) fn pending(root: &File<'_>) -> Option<Pending> {
    let saved = crate::read_file(root, SAVED_PATH).ok()?;
    let rest = core::str::from_utf8(&saved).ok()?.strip_prefix(MARKER)?;
    let hex = |s: &str| u64::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
    let mut parts = rest.trim().split("; canaries ");
    let ring = hex(parts.next()?)?;
    let canaries = parts
        .next()
        .map_or(Vec::new(), |c| c.split(',').filter_map(hex).collect());
    Some(Pending { ring, canaries })
}

/// The 64 bytes a canary page starts with: a tag, then its own address.
fn canary(at: u64) -> [u8; 64] {
    let mut c = [0u8; 64];
    for (i, w) in c.as_chunks_mut::<8>().0.iter_mut().enumerate() {
        let v = if i % 2 == 0 {
            u64::from_le_bytes(*b"PLANRCAN")
        } else {
            at
        };
        w.copy_from_slice(&v.to_le_bytes());
    }
    c
}

/// Reserves a page near the bottom, middle and top of each large free
/// range and writes a [`canary`] in it, flushed to RAM. The second boot
/// checks which survived, mapping what a reset preserves.
fn plant_canaries(st: &SystemTable<Boot>) -> Vec<u64> {
    const STEP: u64 = 2 << 20;
    const MAX: usize = 12;
    let bs = st.boot_services();
    let mut buf = alloc::vec![0u8; bs.memory_map_size() * 2 + 4096];
    let Ok(map) = bs.memory_map(&mut buf) else {
        return Vec::new();
    };
    let free: Vec<(u64, u64)> = map
        .iter()
        .filter(|d| d.ty == MemoryType::Conventional && d.bytes() >= 4 * STEP)
        .map(|d| (d.phys_start, d.phys_start + d.bytes()))
        .collect();
    let mut planted = Vec::new();
    for (start, end) in free {
        let lo = start.next_multiple_of(STEP) + STEP;
        let hi = (end - STEP) / STEP * STEP;
        let mid = lo.midpoint(hi) / STEP * STEP;
        for at in [lo, mid, hi] {
            if planted.len() == MAX || planted.contains(&at) {
                continue;
            }
            if bs.allocate_pages_at(MemoryType::LoaderData, 1, at).is_ok() {
                // SAFETY: a page just allocated to us, identity-mapped.
                unsafe { core::ptr::copy_nonoverlapping(canary(at).as_ptr(), at as *mut u8, 64) };
                crate::handoff::flush_to_memory(at, 64);
                planted.push(at);
            }
        }
    }
    planted
}

/// Marks the test pending with the ring at `ring` and freshly planted
/// canaries, then sets `BootNext` to `BootCurrent`. Returns the entry. If
/// `BootNext` cannot be set, the marker is cleared again.
pub(crate) fn arrange(st: &SystemTable<Boot>, root: &File<'_>, ring: u64) -> Result<u16> {
    let canaries = plant_canaries(st);
    let list: Vec<String> = canaries.iter().map(|a| format!("{a:#x}")).collect();
    rewrite(root, &format!("{MARKER}{ring:#x}; canaries {}\n", list.join(",")))?;
    repeat(st).inspect_err(|_| {
        let _ = rewrite(root, "");
    })
}

/// What the old ring's memory and each canary hold now; read before
/// `logring::setup` can reuse any of it.
pub(crate) fn survey(st: &SystemTable<Boot>, p: &Pending) -> String {
    let mut s = format!("the ring's memory was {}\n", describe(st, p.ring));
    let (mut kept, mut total) = (0, 0);
    for &at in &p.canaries {
        total += 1;
        // SAFETY: as in `describe`, RAM by the first boot's memory map.
        let now = unsafe { core::slice::from_raw_parts(at as *const u8, 64) };
        let state = if now == canary(at) {
            kept += 1;
            "kept"
        } else if now.iter().all(|&b| b == 0) {
            "zeroed"
        } else {
            "changed"
        };
        let _ = writeln!(s, "  canary {at:#x}: {state}");
    }
    let _ = writeln!(s, "{kept} of {total} canaries survived the reset");
    s
}

fn repeat(st: &SystemTable<Boot>) -> Result<u16> {
    let rt = st.runtime_services();
    let mut entry = [0u8; 2];
    let (_, len) = rt.get_variable(&BOOT_CURRENT, &guid::GLOBAL_VARIABLE, &mut entry)?;
    if len != entry.len() {
        return Err(Status::BAD_BUFFER_SIZE);
    }
    let attrs = var::NON_VOLATILE | var::BOOTSERVICE_ACCESS | var::RUNTIME_ACCESS;
    rt.set_variable(&BOOT_NEXT, &guid::GLOBAL_VARIABLE, attrs, &entry)?;
    Ok(u16::from_le_bytes(entry))
}

fn rewrite(root: &File<'_>, text: &str) -> Result<()> {
    let mut f = root.rewrite(SAVED_PATH)?;
    f.write_all(text.as_bytes())?;
    f.flush()
}

/// Reports that the ring the first boot put at `at` did not survive, saves
/// the report over the marker, and stops.
pub(crate) fn lost(st: &SystemTable<Boot>, root: &File<'_>, at: u64, what: &str) -> ! {
    let mut report = format!("9boot: reset test FAILED: the log ring at {at:#x} did not survive the reset.\n");
    for line in what.lines() {
        let _ = writeln!(report, "9boot: {line}");
    }
    let _ = rewrite(root, &report);
    let mut out = st.stdout();
    let _ = write!(out, "{report}");
    let _ = writeln!(
        out,
        "9boot: report saved to \\{SAVED_PATH}; halting. Reset to boot the usual OS."
    );
    loop {
        core::hint::spin_loop();
    }
}

/// What the memory map says `at` is now, and what its first bytes hold.
fn describe(st: &SystemTable<Boot>, at: u64) -> String {
    let bs = st.boot_services();
    let mut buf = alloc::vec![0u8; bs.memory_map_size() * 2 + 4096];
    let ty = bs.memory_map(&mut buf).ok().and_then(|m| {
        m.iter()
            .find(|d| (d.phys_start..d.phys_start + d.bytes()).contains(&at))
            .map(|d| d.ty)
    });
    let ram = matches!(
        ty,
        Some(
            MemoryType::Conventional
                | MemoryType::LoaderCode
                | MemoryType::LoaderData
                | MemoryType::BootServicesCode
                | MemoryType::BootServicesData
                | MemoryType::Other(_)
        )
    );
    let kind = ty.map_or_else(
        || String::from("not in the memory map"),
        |t| format!("{t:?} in the memory map"),
    );
    if !ram {
        return format!("{kind}; not read");
    }
    // SAFETY: RAM by the memory map, identity-mapped by UEFI; only read.
    let head = unsafe { core::slice::from_raw_parts(at as *const u8, 64) };
    let contents = if head.iter().all(|&b| b == 0) {
        String::from("all zeros (cleared)")
    } else if head[..8] == MAGIC.to_le_bytes() {
        // Header fields: version (u32 at 8), capacity, boot, head, check.
        let word = |o: usize| u64::from_le_bytes(head[o..o + 8].try_into().unwrap_or([0; 8]));
        format!(
            "a ring header that fails its check: version {}, capacity {:#x}, boot {}, written {:#x}, check {:#x}",
            word(8) & 0xffff_ffff,
            word(16),
            word(24),
            word(32),
            word(40)
        )
    } else {
        let mut s = String::from("other data:");
        for b in &head[..16] {
            let _ = write!(s, " {b:02x}");
        }
        s
    };
    format!("{kind}, holding {contents}")
}
