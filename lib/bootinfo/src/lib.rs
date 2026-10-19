//! The 9boot → kernel handoff (ADR 0010).
//!
//! 9boot fills in a [`BootInfo`] in memory the kernel can reach through the
//! direct map, then jumps to the kernel entry point with a pointer to it.
//! Both sides are built from this crate in the same tree, but the structure
//! still carries a magic number and version so that a mismatched loader and
//! kernel fail loudly instead of misreading each other.
//!
//! Every pointer in here is a kernel virtual address in the direct map
//! (`hhdm_base + phys`), valid as soon as the kernel starts.

#![cfg_attr(not(test), no_std)]

use core::fmt;

/// `BootInfo::magic`: "PLANR9BI".
pub const MAGIC: u64 = u64::from_le_bytes(*b"PLANR9BI");
/// Bumped on any layout change.
pub const VERSION: u32 = 2;

/// Base of the direct map of physical memory: `phys` is at `HHDM_BASE + phys`.
pub const HHDM_BASE: u64 = 0xffff_8000_0000_0000;
/// The kernel image is linked to run here (top 2 GiB; the amd64 kernel code
/// model requires it).
pub const KERNEL_BASE: u64 = 0xffff_ffff_8000_0000;

/// arm64 memory attributes: 9boot programs `MAIR_EL1` with these indices and
/// the kernel keeps them.
pub mod arm64 {
    /// Normal memory, write-back read/write-allocate.
    pub const ATTR_NORMAL: u64 = 0;
    /// Device-nGnRnE.
    pub const ATTR_DEVICE: u64 = 1;
    /// Normal memory, non-cacheable (framebuffers).
    pub const ATTR_NORMAL_NC: u64 = 2;
    /// `MAIR_EL1`: attr0 = 0xff, attr1 = 0x00, attr2 = 0x44.
    pub const MAIR: u64 = 0x00_44_00_ff;
}

/// A `(pointer, length)` pair pointing into the direct map.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Slice<T> {
    ptr: *const T,
    len: usize,
}

impl<T> Slice<T> {
    #[must_use]
    pub const fn empty() -> Self {
        Slice {
            ptr: core::ptr::null(),
            len: 0,
        }
    }

    #[must_use]
    pub const fn new(ptr: *const T, len: usize) -> Self {
        Slice { ptr, len }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// # Safety
    ///
    /// The pointer must be valid for `len` elements for the returned
    /// lifetime; [`BootInfo::validate`] establishes that for a handoff
    /// produced by 9boot.
    #[must_use]
    pub unsafe fn as_slice<'a>(&self) -> &'a [T] {
        if self.len == 0 {
            &[]
        } else {
            // SAFETY: caller contract.
            unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
        }
    }
}

/// What a range of physical memory is.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionKind {
    /// Free RAM the kernel may use.
    Usable = 1,
    /// The kernel image.
    Kernel = 2,
    /// 9boot's handoff data: page tables, boot stack, this structure.
    /// Reusable once the kernel has its own page tables and has copied what
    /// it needs.
    Boot = 3,
    /// ACPI tables; reusable after they are parsed.
    AcpiReclaimable = 4,
    /// ACPI non-volatile storage; never reusable.
    AcpiNvs = 5,
    /// UEFI runtime services code and data; keep mapped if runtime services
    /// are to be used.
    UefiRuntime = 6,
    /// Persistent memory.
    Persistent = 7,
    /// RAM with errors.
    Bad = 8,
    /// Anything else the firmware reserved.
    Reserved = 9,
    /// The persistent log ring ([`BootInfo::log_ring`]). Left out of the
    /// direct map, so the kernel can map it write-combining without a
    /// cacheable alias.
    LogRing = 10,
}

impl RegionKind {
    /// RAM that 9boot includes in the direct map.
    #[must_use]
    pub fn is_ram(self) -> bool {
        !matches!(self, RegionKind::Bad | RegionKind::Reserved | RegionKind::LogRing)
    }
}

/// A physical memory range, page aligned.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub start: u64,
    pub len: u64,
    pub kind: RegionKind,
}

impl Region {
    #[must_use]
    pub fn end(&self) -> u64 {
        self.start + self.len
    }
}

/// Pixel layout of the boot framebuffer.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Bytes R, G, B, x.
    Rgbx = 1,
    /// Bytes B, G, R, x.
    Bgrx = 2,
}

/// The firmware's linear framebuffer.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Framebuffer {
    /// Physical address; 0 means there is none (see [`BootInfo::framebuffer`]).
    pub phys: u64,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    /// Pixels per scan line.
    pub stride: u32,
    pub format: PixelFormat,
}

/// Where the persistent log ring is (`lib/logring`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogRing {
    /// Physical address; 0 means there is none.
    pub phys: u64,
    pub size: u64,
}

/// The exception level 9boot was entered at, on arm64.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryLevel {
    /// amd64, or anything without exception levels.
    NotApplicable = 0,
    El1 = 1,
    /// 9boot dropped to EL1 before entering the kernel.
    El2 = 2,
}

/// Everything 9boot hands the kernel.
#[repr(C)]
#[derive(Debug)]
pub struct BootInfo {
    pub magic: u64,
    pub version: u32,
    pub entry_level: EntryLevel,
    /// Physical memory, sorted by address, adjacent same-kind ranges merged.
    pub memory: Slice<Region>,
    /// Physical address of the ACPI 2.0+ RSDP; 0 if none.
    pub rsdp: u64,
    /// Physical address of the UEFI system table (for runtime services).
    pub uefi_system_table: u64,
    framebuffer: Framebuffer,
    log_ring: LogRing,
    /// Physical load address and size of the kernel image.
    pub kernel_phys: u64,
    pub kernel_size: u64,
    /// Contents of `\planr\plan9.ini` (UTF-8), possibly empty.
    pub config: Slice<u8>,
}

/// Why a handoff was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalid {
    Magic(u64),
    Version(u32),
    UnsortedMemory,
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Invalid::Magic(m) => write!(f, "bad magic {m:#x}"),
            Invalid::Version(v) => write!(f, "version {v}, kernel expects {VERSION}"),
            Invalid::UnsortedMemory => f.write_str("memory map not sorted or overlapping"),
        }
    }
}

impl BootInfo {
    #[must_use]
    pub fn new(entry_level: EntryLevel) -> Self {
        BootInfo {
            magic: MAGIC,
            version: VERSION,
            entry_level,
            memory: Slice::empty(),
            rsdp: 0,
            uefi_system_table: 0,
            framebuffer: Framebuffer {
                phys: 0,
                size: 0,
                width: 0,
                height: 0,
                stride: 0,
                format: PixelFormat::Bgrx,
            },
            log_ring: LogRing { phys: 0, size: 0 },
            kernel_phys: 0,
            kernel_size: 0,
            config: Slice::empty(),
        }
    }

    /// Checks that this looks like a handoff from a matching 9boot.
    pub fn validate(&self) -> Result<(), Invalid> {
        if self.magic != MAGIC {
            return Err(Invalid::Magic(self.magic));
        }
        if self.version != VERSION {
            return Err(Invalid::Version(self.version));
        }
        // SAFETY: magic and version match, so 9boot built `memory`.
        let mem = unsafe { self.memory.as_slice() };
        if mem.windows(2).any(|w| w[0].end() > w[1].start) {
            return Err(Invalid::UnsortedMemory);
        }
        Ok(())
    }

    #[must_use]
    pub fn memory(&self) -> &[Region] {
        // SAFETY: valid for a handoff that passed `validate`; the kernel
        // validates before anything else.
        unsafe { self.memory.as_slice() }
    }

    #[must_use]
    pub fn config(&self) -> &str {
        // SAFETY: as for `memory`.
        core::str::from_utf8(unsafe { self.config.as_slice() }).unwrap_or("")
    }

    #[must_use]
    pub fn framebuffer(&self) -> Option<Framebuffer> {
        (self.framebuffer.phys != 0).then_some(self.framebuffer)
    }

    pub fn set_framebuffer(&mut self, fb: Option<Framebuffer>) {
        if let Some(fb) = fb {
            self.framebuffer = fb;
        }
    }

    #[must_use]
    pub fn log_ring(&self) -> Option<LogRing> {
        (self.log_ring.phys != 0).then_some(self.log_ring)
    }

    pub fn set_log_ring(&mut self, ring: Option<LogRing>) {
        if let Some(r) = ring {
            self.log_ring = r;
        }
    }

    #[must_use]
    pub fn rsdp(&self) -> Option<u64> {
        (self.rsdp != 0).then_some(self.rsdp)
    }
}

/// Converts a physical address to its direct-map virtual address.
#[must_use]
pub const fn phys_to_virt(phys: u64) -> u64 {
    HHDM_BASE + phys
}

/// Merges `region` into a sorted list being built in address order,
/// coalescing with the previous entry when contiguous and of the same kind.
/// Returns false if `out` is full.
pub fn push_region(out: &mut [Region], len: &mut usize, region: Region) -> bool {
    if region.len == 0 {
        return true;
    }
    if let Some(last) = len.checked_sub(1).map(|i| &mut out[i])
        && last.kind == region.kind
        && last.end() == region.start
    {
        last.len += region.len;
        return true;
    }
    if *len == out.len() {
        return false;
    }
    out[*len] = region;
    *len += 1;
    true
}

/// Relabels `[start, start + size)` as `kind` in a sorted region list,
/// splitting the regions it partly covers. Gaps stay gaps. Returns false,
/// changing nothing, if `out` has no room for the extra regions. Does not
/// allocate: 9boot runs it between the final memory map and
/// `ExitBootServices`.
pub fn carve(out: &mut [Region], len: &mut usize, start: u64, size: u64, kind: RegionKind) -> bool {
    let end = start.saturating_add(size);
    // Count the extra regions first, so a full list is left untouched.
    let extra: usize = out[..*len]
        .iter()
        .filter(|r| r.start < end && start < r.end() && r.kind != kind)
        .map(|r| usize::from(r.start < start) + usize::from(end < r.end()))
        .sum();
    if *len + extra > out.len() {
        return false;
    }
    let mut at = 0;
    while at < *len {
        let old = out[at];
        if old.end() <= start || end <= old.start || old.kind == kind {
            at += 1;
            continue;
        }
        let (lo, hi) = (old.start.max(start), old.end().min(end));
        let pieces = [(old.start, lo, old.kind), (lo, hi, kind), (hi, old.end(), old.kind)];
        let mut put = [old; 3];
        let mut count = 0;
        for (from, to, piece_kind) in pieces {
            if from < to {
                put[count] = Region {
                    start: from,
                    len: to - from,
                    kind: piece_kind,
                };
                count += 1;
            }
        }
        out.copy_within(at + 1..*len, at + count);
        out[at..at + count].copy_from_slice(&put[..count]);
        *len += count - 1;
        at += count;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(start: u64, len: u64, kind: RegionKind) -> Region {
        Region { start, len, kind }
    }

    #[test]
    fn carve_splits_and_relabels() {
        let mut out = [r(0, 0, RegionKind::Reserved); 8];
        out[0] = r(0x1000, 0x4000, RegionKind::Boot);
        out[1] = r(0x5000, 0x1000, RegionKind::Usable);
        out[2] = r(0x8000, 0x2000, RegionKind::Boot);
        let mut n = 3;
        // Inside the first region: it splits in three.
        assert!(carve(&mut out, &mut n, 0x2000, 0x1000, RegionKind::Kernel));
        assert_eq!(
            out[..n],
            [
                r(0x1000, 0x1000, RegionKind::Boot),
                r(0x2000, 0x1000, RegionKind::Kernel),
                r(0x3000, 0x2000, RegionKind::Boot),
                r(0x5000, 0x1000, RegionKind::Usable),
                r(0x8000, 0x2000, RegionKind::Boot),
            ]
        );
        // Across a gap and the start of the last region.
        assert!(carve(&mut out, &mut n, 0x6000, 0x3000, RegionKind::LogRing));
        assert_eq!(
            out[n - 2..n],
            [
                r(0x8000, 0x1000, RegionKind::LogRing),
                r(0x9000, 0x1000, RegionKind::Boot)
            ]
        );
        assert_eq!(n, 6);
    }

    #[test]
    fn carve_without_room_changes_nothing() {
        let mut out = [r(0x1000, 0x4000, RegionKind::Boot), r(0, 0, RegionKind::Reserved)];
        let mut n = 1;
        assert!(!carve(&mut out, &mut n, 0x2000, 0x1000, RegionKind::Kernel));
        assert_eq!((n, out[0]), (1, r(0x1000, 0x4000, RegionKind::Boot)));
        // An exact fit needs no room.
        assert!(carve(&mut out, &mut n, 0x1000, 0x4000, RegionKind::Kernel));
        assert_eq!((n, out[0]), (1, r(0x1000, 0x4000, RegionKind::Kernel)));
    }

    #[test]
    fn push_merges_contiguous_same_kind() {
        let mut out = [r(0, 0, RegionKind::Reserved); 4];
        let mut n = 0;
        assert!(push_region(&mut out, &mut n, r(0, 0x1000, RegionKind::Usable)));
        assert!(push_region(&mut out, &mut n, r(0x1000, 0x1000, RegionKind::Usable)));
        assert!(push_region(&mut out, &mut n, r(0x2000, 0x1000, RegionKind::Kernel)));
        assert!(push_region(&mut out, &mut n, r(0x4000, 0x1000, RegionKind::Kernel)));
        assert!(push_region(&mut out, &mut n, r(0x9000, 0, RegionKind::Bad)));
        assert_eq!(n, 3);
        assert_eq!(out[0], r(0, 0x2000, RegionKind::Usable));
    }

    #[test]
    fn push_reports_full() {
        let mut out = [r(0, 0, RegionKind::Reserved); 1];
        let mut n = 0;
        assert!(push_region(&mut out, &mut n, r(0, 0x1000, RegionKind::Usable)));
        assert!(!push_region(&mut out, &mut n, r(0x5000, 0x1000, RegionKind::Usable)));
    }

    #[test]
    fn validate_checks_magic_version_and_order() {
        let regions = [r(0, 0x2000, RegionKind::Usable), r(0x1000, 0x1000, RegionKind::Kernel)];
        let mut bi = BootInfo::new(EntryLevel::NotApplicable);
        assert!(bi.validate().is_ok());
        bi.memory = Slice::new(regions.as_ptr(), 2);
        assert_eq!(bi.validate(), Err(Invalid::UnsortedMemory));
        bi.memory = Slice::new(regions.as_ptr(), 1);
        bi.version = 0;
        assert_eq!(bi.validate(), Err(Invalid::Version(0)));
        bi.magic = 0;
        assert_eq!(bi.validate(), Err(Invalid::Magic(0)));
    }

    #[test]
    fn framebuffer_and_log_ring_absent_by_default() {
        assert!(BootInfo::new(EntryLevel::El1).framebuffer().is_none());
        assert!(BootInfo::new(EntryLevel::El1).log_ring().is_none());
    }
}
