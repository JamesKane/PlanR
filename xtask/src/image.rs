//! The bootable disk image: GPT with a single FAT32 ESP.

use std::path::Path;

use crate::arch::Arch;
use crate::fat::{self, Dir};
use crate::gpt::{self, Guid, Partition, SECTOR};

/// Whole-disk size. Generous for an ESP; the file is mostly zeros.
const DISK_BYTES: u64 = 64 << 20;
/// First partition at 1 MiB, the usual alignment.
const ESP_FIRST_LBA: u64 = 2048;

/// What goes on the ESP.
#[derive(Debug)]
pub(crate) struct Contents<'a> {
    pub(crate) loader: &'a Path,
    pub(crate) kernel: &'a Path,
    /// `\planr\boot.arc`.
    pub(crate) archive: &'a Path,
    /// `\planr\plan9.ini`.
    pub(crate) config: &'a str,
}

pub(crate) fn write(arch: Arch, c: &Contents<'_>, out: &Path) -> Result<(), String> {
    let read = |p: &Path| std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()));
    let mut root = Dir::default();
    root.insert(&format!("EFI/BOOT/{}", arch.boot_file()), read(c.loader)?)?;
    root.insert("PLANR/KERNEL", read(c.kernel)?)?;
    root.insert("PLANR/BOOT.ARC", read(c.archive)?)?;
    root.insert("PLANR/PLAN9.INI", c.config.as_bytes().to_vec())?;
    // Marks the volume as one 9boot may write: it saves the previous boot's
    // log ring here, and never creates the file itself.
    root.insert("PLANR/LAST.LOG", Vec::new())?;

    let sectors = DISK_BYTES / SECTOR;
    let (_, last) = gpt::usable(sectors);
    let esp = Partition {
        type_guid: Guid::ESP,
        guid: Guid::derived(&format!("planr-{arch}-esp")),
        first_lba: ESP_FIRST_LBA,
        last_lba: last,
        name: "Plan R ESP",
    };
    let vol = fat::Volume {
        sectors: u32::try_from(last - ESP_FIRST_LBA + 1).map_err(|_| "ESP too large")?,
        hidden: u32::try_from(ESP_FIRST_LBA).expect("small"),
        label: "PLANR ESP",
        serial: crate::crc32::crc32(format!("planr-{arch}").as_bytes()),
    };
    let fs = fat::build(&vol, &root)?;

    let mut disk = vec![0u8; usize::try_from(DISK_BYTES).expect("fits")];
    let off = usize::try_from(ESP_FIRST_LBA * SECTOR).expect("fits");
    disk[off..off + fs.len()].copy_from_slice(&fs);
    gpt::write(&mut disk, Guid::derived(&format!("planr-{arch}-disk")), &[esp])?;

    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(out, &disk).map_err(|e| format!("{}: {e}", out.display()))
}
