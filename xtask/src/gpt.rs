//! GUID Partition Table writer (UEFI 2.10 §5.3), with protective MBR.

use crate::crc32::crc32;

pub(crate) const SECTOR: u64 = 512;
const ENTRIES: u64 = 128;
const ENTRY_SIZE: u64 = 128;
/// Sectors taken by the partition entry array: 128 × 128 / 512.
const ENTRY_SECTORS: u64 = ENTRIES * ENTRY_SIZE / SECTOR;

/// A GUID in canonical form; [`Guid::to_bytes`] gives the on-disk
/// mixed-endian layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Guid(pub(crate) u32, pub(crate) u16, pub(crate) u16, pub(crate) [u8; 8]);

impl Guid {
    /// EFI System Partition.
    pub(crate) const ESP: Guid = Guid(
        0xc12a_7328,
        0xf81f,
        0x11d2,
        [0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b],
    );

    pub(crate) fn to_bytes(self) -> [u8; 16] {
        let mut b = [0; 16];
        b[..4].copy_from_slice(&self.0.to_le_bytes());
        b[4..6].copy_from_slice(&self.1.to_le_bytes());
        b[6..8].copy_from_slice(&self.2.to_le_bytes());
        b[8..].copy_from_slice(&self.3);
        b
    }

    /// A deterministic version-4-shaped GUID derived from `name`, so images
    /// are reproducible byte for byte.
    pub(crate) fn derived(name: &str) -> Guid {
        let h = |seed: u64| {
            name.bytes()
                .fold(seed, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3))
        };
        let (a, b) = (h(0xcbf2_9ce4_8422_2325), h(0x6c62_272e_07bb_0142));
        let mut d4 = b.to_be_bytes();
        d4[0] = (d4[0] & 0x3f) | 0x80; // RFC 4122 variant
        #[allow(clippy::cast_possible_truncation)]
        Guid((a >> 32) as u32, (a >> 16) as u16, ((a as u16) & 0x0fff) | 0x4000, d4)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Partition {
    pub(crate) type_guid: Guid,
    pub(crate) guid: Guid,
    pub(crate) first_lba: u64,
    pub(crate) last_lba: u64,
    pub(crate) name: &'static str,
}

/// The usable LBA range for partitions on a disk of `sectors` sectors.
pub(crate) fn usable(sectors: u64) -> (u64, u64) {
    (2 + ENTRY_SECTORS, sectors - 2 - ENTRY_SECTORS)
}

/// Writes the protective MBR, primary and backup GPT into `disk`.
pub(crate) fn write(disk: &mut [u8], disk_guid: Guid, parts: &[Partition]) -> Result<(), String> {
    let sectors = disk.len() as u64 / SECTOR;
    let (first, last) = usable(sectors);
    if parts.len() as u64 > ENTRIES {
        return Err("too many partitions".into());
    }
    for p in parts {
        if p.first_lba < first || p.last_lba > last || p.first_lba > p.last_lba {
            return Err(format!("partition {:?} outside usable range {first}..={last}", p.name));
        }
    }

    // Protective MBR: one partition of type 0xEE covering the disk.
    let mbr = &mut disk[..512];
    mbr[446..462].copy_from_slice(&{
        let mut e = [0u8; 16];
        e[1..4].copy_from_slice(&[0x00, 0x02, 0x00]); // CHS of LBA 1
        e[4] = 0xee;
        e[5..8].copy_from_slice(&[0xff, 0xff, 0xff]);
        e[8..12].copy_from_slice(&1u32.to_le_bytes());
        e[12..16].copy_from_slice(&u32::try_from(sectors - 1).unwrap_or(u32::MAX).to_le_bytes());
        e
    });
    mbr[510] = 0x55;
    mbr[511] = 0xaa;

    // Partition entry array (identical copies before and after the data).
    let mut array = vec![0u8; usize::try_from(ENTRIES * ENTRY_SIZE).expect("small")];
    for (i, p) in parts.iter().enumerate() {
        let e = &mut array[i * 128..(i + 1) * 128];
        e[..16].copy_from_slice(&p.type_guid.to_bytes());
        e[16..32].copy_from_slice(&p.guid.to_bytes());
        e[32..40].copy_from_slice(&p.first_lba.to_le_bytes());
        e[40..48].copy_from_slice(&p.last_lba.to_le_bytes());
        for (j, u) in p.name.encode_utf16().take(36).enumerate() {
            e[56 + 2 * j..58 + 2 * j].copy_from_slice(&u.to_le_bytes());
        }
    }
    let array_crc = crc32(&array);

    let header = |my_lba: u64, alt_lba: u64, entries_lba: u64| {
        let mut h = [0u8; 92];
        h[..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&my_lba.to_le_bytes());
        h[32..40].copy_from_slice(&alt_lba.to_le_bytes());
        h[40..48].copy_from_slice(&first.to_le_bytes());
        h[48..56].copy_from_slice(&last.to_le_bytes());
        h[56..72].copy_from_slice(&disk_guid.to_bytes());
        h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
        #[allow(clippy::cast_possible_truncation)]
        h[80..84].copy_from_slice(&(ENTRIES as u32).to_le_bytes());
        #[allow(clippy::cast_possible_truncation)]
        h[84..88].copy_from_slice(&(ENTRY_SIZE as u32).to_le_bytes());
        h[88..92].copy_from_slice(&array_crc.to_le_bytes());
        let crc = crc32(&h);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    };

    let at = |lba: u64| usize::try_from(lba * SECTOR).expect("disk fits in memory");
    let backup_lba = sectors - 1;
    let backup_entries = backup_lba - ENTRY_SECTORS;
    disk[at(1)..at(1) + 92].copy_from_slice(&header(1, backup_lba, 2));
    disk[at(2)..at(2) + array.len()].copy_from_slice(&array);
    disk[at(backup_entries)..at(backup_entries) + array.len()].copy_from_slice(&array);
    disk[at(backup_lba)..at(backup_lba) + 92].copy_from_slice(&header(backup_lba, 1, backup_entries));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le32(b: &[u8]) -> u32 {
        u32::from_le_bytes(b[..4].try_into().unwrap())
    }

    #[test]
    fn esp_guid_on_disk_layout() {
        assert_eq!(
            Guid::ESP.to_bytes(),
            [
                0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b
            ]
        );
    }

    #[test]
    fn derived_guids_are_stable_and_distinct() {
        assert_eq!(Guid::derived("a"), Guid::derived("a"));
        assert_ne!(Guid::derived("a"), Guid::derived("b"));
        assert_eq!(Guid::derived("a").2 >> 12, 4);
    }

    #[test]
    fn headers_have_valid_crcs() {
        let sectors = 4096u64;
        let mut disk = vec![0u8; usize::try_from(sectors * SECTOR).unwrap()];
        let (first, last) = usable(sectors);
        let p = Partition {
            type_guid: Guid::ESP,
            guid: Guid::derived("p"),
            first_lba: first,
            last_lba: last,
            name: "ESP",
        };
        write(&mut disk, Guid::derived("d"), &[p]).unwrap();
        for lba in [1, sectors - 1] {
            let off = usize::try_from(lba * SECTOR).unwrap();
            let mut h = disk[off..off + 92].to_vec();
            assert_eq!(&h[..8], b"EFI PART");
            let stored = le32(&h[16..]);
            h[16..20].fill(0);
            assert_eq!(crc32(&h), stored, "header crc at lba {lba}");
            let entries_lba = u64::from_le_bytes(h[72..80].try_into().unwrap());
            let eoff = usize::try_from(entries_lba * SECTOR).unwrap();
            assert_eq!(
                crc32(&disk[eoff..eoff + 16384]),
                le32(&h[88..]),
                "array crc at lba {lba}"
            );
        }
        assert_eq!(&disk[510..512], &[0x55, 0xaa]);
        assert_eq!(disk[446 + 4], 0xee);
    }
}
