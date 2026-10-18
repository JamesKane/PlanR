//! Booting Plan R a second time. `test=reset` checks that the log ring
//! survives the kernel's warm reset, which needs 9boot to run again
//! afterwards. A board booted through `BootNext` would instead fall back
//! to its own OS, so on the first boot 9boot sets `BootNext` to the entry
//! it was started from. That is the only variable it writes.

use uefi::{Boot, Result, Status, SystemTable, guid, ucs2, var};

const BOOT_CURRENT: [u16; 12] = ucs2("BootCurrent");
const BOOT_NEXT: [u16; 9] = ucs2("BootNext");

/// Sets `BootNext` to `BootCurrent` and returns the entry number.
pub(crate) fn repeat(st: &SystemTable<Boot>) -> Result<u16> {
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
