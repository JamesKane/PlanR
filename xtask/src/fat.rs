//! A FAT32 file system writer for the ESP.
//!
//! It builds a whole volume in memory from a [`Dir`] tree. Every file and
//! directory gets a contiguous cluster run, and timestamps are fixed, so the
//! same input always gives the same bytes. Names must be 8.3 for now; long
//! names come when something needs them.

use std::collections::BTreeMap;

const SECTOR: usize = 512;
const SECTOR_U32: u32 = 512;
const RESERVED_SECTORS: u32 = 32;
const FATS: u32 = 2;
const EOC: u32 = 0x0fff_ffff;
const ATTR_DIR: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_VOLUME_ID: u8 = 0x08;
/// 1980-01-01, the FAT epoch; fixed for reproducible images.
const DATE: u16 = (1 << 5) | 1;
/// FAT32 needs at least this many clusters, or readers take it for FAT16.
const MIN_CLUSTERS: u32 = 65_525;

/// A directory: names map to files or subdirectories.
#[derive(Debug, Default)]
pub(crate) struct Dir(pub(crate) BTreeMap<String, Node>);

#[derive(Debug)]
pub(crate) enum Node {
    File(Vec<u8>),
    Dir(Dir),
}

impl Dir {
    /// Inserts `data` at `path` (e.g. `EFI/BOOT/BOOTX64.EFI`), creating
    /// directories on the way.
    pub(crate) fn insert(&mut self, path: &str, data: Vec<u8>) -> Result<(), String> {
        let (dirs, file) = path.rsplit_once('/').map_or(("", path), |(d, f)| (d, f));
        let mut cur = self;
        for part in dirs.split('/').filter(|p| !p.is_empty()) {
            let n = cur
                .0
                .entry(part.to_string())
                .or_insert_with(|| Node::Dir(Dir::default()));
            cur = match n {
                Node::Dir(d) => d,
                Node::File(_) => return Err(format!("{part:?} in {path:?} is a file")),
            };
        }
        cur.0.insert(file.to_string(), Node::File(data));
        Ok(())
    }
}

/// Volume parameters.
#[derive(Debug)]
pub(crate) struct Volume {
    /// Volume size in 512-byte sectors.
    pub(crate) sectors: u32,
    /// LBA of the volume on its disk (BPB "hidden sectors").
    pub(crate) hidden: u32,
    pub(crate) label: &'static str,
    pub(crate) serial: u32,
}

struct Layout {
    sectors_per_cluster: u32,
    fat_sectors: u32,
    clusters: u32,
}

impl Layout {
    fn new(sectors: u32) -> Result<Layout, String> {
        let sectors_per_cluster = 1;
        let mut fat_sectors = 1;
        loop {
            let clusters = (sectors - RESERVED_SECTORS - FATS * fat_sectors) / sectors_per_cluster;
            let need = ((clusters + 2) * 4).div_ceil(SECTOR_U32);
            if need <= fat_sectors {
                if clusters < MIN_CLUSTERS {
                    return Err(format!("volume of {sectors} sectors is too small for FAT32"));
                }
                return Ok(Layout {
                    sectors_per_cluster,
                    fat_sectors,
                    clusters,
                });
            }
            fat_sectors = need;
        }
    }

    fn cluster_bytes(&self) -> usize {
        self.sectors_per_cluster as usize * SECTOR
    }

    fn data_start(&self) -> usize {
        (RESERVED_SECTORS + FATS * self.fat_sectors) as usize * SECTOR
    }

    fn cluster_offset(&self, c: u32) -> usize {
        self.data_start() + (c as usize - 2) * self.cluster_bytes()
    }
}

/// Converts `name` to a space-padded 8.3 directory name.
fn short_name(name: &str) -> Result<[u8; 11], String> {
    let up = name.to_ascii_uppercase();
    let (base, ext) = up.rsplit_once('.').unwrap_or((&up, ""));
    let ok = |s: &str| {
        s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'()-@^_`{}~".contains(&b))
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 || !ok(base) || !ok(ext) {
        return Err(format!(
            "{name:?} is not a valid 8.3 name (long names are not supported yet)"
        ));
    }
    let mut out = [b' '; 11];
    out[..base.len()].copy_from_slice(base.as_bytes());
    out[8..8 + ext.len()].copy_from_slice(ext.as_bytes());
    Ok(out)
}

fn dir_entry(name: [u8; 11], attr: u8, cluster: u32, size: u32) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[..11].copy_from_slice(&name);
    e[11] = attr;
    for off in [16, 18, 24] {
        e[off..off + 2].copy_from_slice(&DATE.to_le_bytes()); // created, accessed, written
    }
    #[allow(clippy::cast_possible_truncation)]
    {
        e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
    }
    e[28..32].copy_from_slice(&size.to_le_bytes());
    e
}

struct Writer<'a> {
    img: &'a mut [u8],
    layout: Layout,
    fat: Vec<u32>,
    next: u32,
}

impl Writer<'_> {
    /// Allocates a contiguous chain of `n` clusters (`n` ≥ 1).
    fn alloc(&mut self, n: u32) -> Result<u32, String> {
        let start = self.next;
        if start + n > self.layout.clusters + 2 {
            return Err("ESP is full".into());
        }
        for c in start..start + n {
            self.fat[c as usize] = if c + 1 == start + n { EOC } else { c + 1 };
        }
        self.next += n;
        Ok(start)
    }

    fn clusters_for(&self, bytes: usize) -> u32 {
        u32::try_from(bytes.div_ceil(self.layout.cluster_bytes())).expect("fits")
    }

    fn put(&mut self, cluster: u32, data: &[u8]) {
        let off = self.layout.cluster_offset(cluster);
        self.img[off..off + data.len()].copy_from_slice(data);
    }

    /// Writes `dir` (already given `cluster`) and everything below it.
    /// `parent` is the parent's cluster, 0 for the root's children, and
    /// `None` for the root itself.
    fn write_dir(
        &mut self,
        dir: &Dir,
        cluster: u32,
        parent: Option<u32>,
        label: Option<[u8; 11]>,
    ) -> Result<(), String> {
        let mut entries: Vec<[u8; 32]> = Vec::new();
        match parent {
            Some(p) => {
                entries.push(dir_entry(*b".          ", ATTR_DIR, cluster, 0));
                entries.push(dir_entry(*b"..         ", ATTR_DIR, p, 0));
            }
            None => entries.extend(label.map(|l| dir_entry(l, ATTR_VOLUME_ID, 0, 0))),
        }
        let mut seen = std::collections::HashSet::new();
        let mut subdirs = Vec::new();
        for (name, node) in &dir.0 {
            let short = short_name(name)?;
            if !seen.insert(short) {
                return Err(format!("{name:?} collides with another name in its directory"));
            }
            match node {
                Node::File(data) => {
                    let size = u32::try_from(data.len()).map_err(|_| format!("{name:?} exceeds 4 GiB"))?;
                    let first = if data.is_empty() {
                        0
                    } else {
                        self.alloc(self.clusters_for(data.len()))?
                    };
                    if first != 0 {
                        self.put(first, data);
                    }
                    entries.push(dir_entry(short, ATTR_ARCHIVE, first, size));
                }
                Node::Dir(sub) => {
                    // Room for ".", "..", and the children.
                    let c = self.alloc(self.clusters_for((sub.0.len() + 2) * 32))?;
                    entries.push(dir_entry(short, ATTR_DIR, c, 0));
                    subdirs.push((sub, c));
                }
            }
        }
        let bytes: Vec<u8> = entries.concat();
        let have = self.chain_len(cluster) as usize * self.layout.cluster_bytes();
        assert!(bytes.len() <= have, "directory allocation too small");
        self.put(cluster, &bytes);
        let me = if parent.is_none() { 0 } else { cluster };
        for (sub, c) in subdirs {
            self.write_dir(sub, c, Some(me), None)?;
        }
        Ok(())
    }

    fn chain_len(&self, mut c: u32) -> u32 {
        let mut n = 1;
        while self.fat[c as usize] != EOC {
            c = self.fat[c as usize];
            n += 1;
        }
        n
    }
}

/// Builds the volume image.
pub(crate) fn build(vol: &Volume, root: &Dir) -> Result<Vec<u8>, String> {
    let layout = Layout::new(vol.sectors)?;
    let mut img = vec![0u8; vol.sectors as usize * SECTOR];
    let label = {
        let mut l = [b' '; 11];
        let up = vol.label.to_ascii_uppercase();
        if up.len() > 11 {
            return Err("volume label longer than 11 bytes".into());
        }
        l[..up.len()].copy_from_slice(up.as_bytes());
        l
    };

    let mut fat = vec![0u32; layout.clusters as usize + 2];
    fat[0] = 0x0fff_fff8;
    fat[1] = EOC;
    let mut w = Writer {
        img: &mut img,
        layout,
        fat,
        next: 2,
    };
    let root_entries = root.0.len() + 1;
    let root_cluster = w.alloc(w.clusters_for(root_entries * 32))?;
    w.write_dir(root, root_cluster, None, Some(label))?;
    let (fat, next, layout) = (w.fat, w.next, w.layout);

    // FATs.
    let fat_bytes: Vec<u8> = fat.iter().flat_map(|v| v.to_le_bytes()).collect();
    for i in 0..FATS {
        let off = (RESERVED_SECTORS + i * layout.fat_sectors) as usize * SECTOR;
        img[off..off + fat_bytes.len()].copy_from_slice(&fat_bytes);
    }

    // Boot sector (BPB) and FSInfo, with backups at sectors 6 and 7.
    let mut bs = [0u8; SECTOR];
    bs[..3].copy_from_slice(&[0xeb, 0x58, 0x90]);
    bs[3..11].copy_from_slice(b"PLANR   ");
    bs[11..13].copy_from_slice(&512u16.to_le_bytes());
    #[allow(clippy::cast_possible_truncation)]
    {
        bs[13] = layout.sectors_per_cluster as u8;
        bs[14..16].copy_from_slice(&(RESERVED_SECTORS as u16).to_le_bytes());
        bs[16] = FATS as u8;
    }
    bs[21] = 0xf8; // fixed media
    bs[24..26].copy_from_slice(&32u16.to_le_bytes()); // sectors per track
    bs[26..28].copy_from_slice(&64u16.to_le_bytes()); // heads
    bs[28..32].copy_from_slice(&vol.hidden.to_le_bytes());
    bs[32..36].copy_from_slice(&vol.sectors.to_le_bytes());
    bs[36..40].copy_from_slice(&layout.fat_sectors.to_le_bytes());
    bs[44..48].copy_from_slice(&root_cluster.to_le_bytes());
    bs[48..50].copy_from_slice(&1u16.to_le_bytes()); // FSInfo sector
    bs[50..52].copy_from_slice(&6u16.to_le_bytes()); // backup boot sector
    bs[64] = 0x80;
    bs[66] = 0x29;
    bs[67..71].copy_from_slice(&vol.serial.to_le_bytes());
    bs[71..82].copy_from_slice(&label);
    bs[82..90].copy_from_slice(b"FAT32   ");
    bs[510] = 0x55;
    bs[511] = 0xaa;

    let mut fsinfo = [0u8; SECTOR];
    fsinfo[..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
    fsinfo[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
    fsinfo[488..492].copy_from_slice(&(layout.clusters + 2 - next).to_le_bytes());
    fsinfo[492..496].copy_from_slice(&next.to_le_bytes());
    fsinfo[508..512].copy_from_slice(&0xaa55_0000u32.to_le_bytes());

    for base in [0, 6] {
        img[base * SECTOR..(base + 1) * SECTOR].copy_from_slice(&bs);
        img[(base + 1) * SECTOR..(base + 2) * SECTOR].copy_from_slice(&fsinfo);
    }
    Ok(img)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le16(b: &[u8], o: usize) -> u16 {
        u16::from_le_bytes([b[o], b[o + 1]])
    }
    fn le32(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
    }

    /// Reads `path` back out of an image, following the FAT: an independent
    /// check of the writer's layout.
    fn read(img: &[u8], path: &str) -> Option<Vec<u8>> {
        let spc = img[13] as usize;
        let reserved = le16(img, 14) as usize;
        let fat_sz = le32(img, 36) as usize;
        let data = (reserved + 2 * fat_sz) * SECTOR;
        let cb = spc * SECTOR;
        let fat = |c: u32| le32(img, reserved * SECTOR + c as usize * 4);
        let chain = |mut c: u32| {
            let mut out = Vec::new();
            while (2..0x0fff_fff8).contains(&c) {
                let off = data + (c as usize - 2) * cb;
                out.extend_from_slice(&img[off..off + cb]);
                c = fat(c);
            }
            out
        };
        let mut cur = chain(le32(img, 44));
        let parts: Vec<_> = path.split('/').collect();
        for (i, p) in parts.iter().enumerate() {
            let want = short_name(p).unwrap();
            let e = cur.chunks(32).take_while(|e| e[0] != 0).find(|e| e[..11] == want)?;
            let c = (u32::from(le16(e, 20)) << 16) | u32::from(le16(e, 26));
            let mut body = chain(c);
            if i + 1 == parts.len() {
                body.truncate(le32(e, 28) as usize);
                return Some(body);
            }
            cur = std::mem::take(&mut body);
        }
        None
    }

    #[test]
    fn short_names() {
        assert_eq!(&short_name("bootx64.efi").unwrap(), b"BOOTX64 EFI");
        assert_eq!(&short_name("EFI").unwrap(), b"EFI        ");
        assert!(short_name("toolongname.efi").is_err());
        assert!(short_name("a.b.c").is_err());
        assert!(short_name("sp ace").is_err());
    }

    #[test]
    fn round_trip() {
        let mut root = Dir::default();
        let big: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        root.insert("EFI/BOOT/BOOTX64.EFI", big.clone()).unwrap();
        root.insert("EFI/PLANR/PLAN9.INI", b"console=0\n".to_vec()).unwrap();
        root.insert("EMPTY", Vec::new()).unwrap();
        let img = build(
            &Volume {
                sectors: 70_000,
                hidden: 2048,
                label: "PLANR ESP",
                serial: 1,
            },
            &root,
        )
        .unwrap();
        assert_eq!(&img[82..90], b"FAT32   ");
        assert_eq!(img[..SECTOR], img[6 * SECTOR..7 * SECTOR]);
        assert_eq!(read(&img, "EFI/BOOT/BOOTX64.EFI").unwrap(), big);
        assert_eq!(read(&img, "EFI/PLANR/PLAN9.INI").unwrap(), b"console=0\n");
        assert_eq!(read(&img, "EMPTY").unwrap(), b"");
        assert!(read(&img, "EFI/NOPE").is_none());
    }

    #[test]
    fn deterministic() {
        let mk = || {
            let mut root = Dir::default();
            root.insert("A/B.TXT", vec![1, 2, 3]).unwrap();
            build(
                &Volume {
                    sectors: 70_000,
                    hidden: 0,
                    label: "X",
                    serial: 7,
                },
                &root,
            )
            .unwrap()
        };
        assert_eq!(mk(), mk());
    }

    #[test]
    fn rejects_tiny_volume() {
        assert!(
            build(
                &Volume {
                    sectors: 1000,
                    hidden: 0,
                    label: "X",
                    serial: 0
                },
                &Dir::default()
            )
            .is_err()
        );
    }
}
