//! UEFI support for 9boot: raw table layouts plus a thin, typed safe layer.
//!
//! Written for Plan R under the core+alloc rule (ADR 0001). Only what the
//! loader needs is modelled; grow it as 9boot grows.
//!
//! The system table is a typestate: [`SystemTable<Boot>`] exposes boot
//! services, and [`SystemTable::exit_boot_services`] consumes it and yields a
//! [`SystemTable<Runtime>`], so boot services cannot be touched afterwards.

#![cfg_attr(not(test), no_std)]

pub mod raw;

mod console;
mod fs;
mod gop;
mod memory;

use core::ffi::c_void;
use core::fmt;
use core::marker::PhantomData;
use core::ptr::{self, NonNull};

pub use console::Console;
pub use fs::{File, LoadedImage, SimpleFileSystem};
pub use gop::{GraphicsOutput, ModeInfo, PixelFormat};
pub use memory::{MapKey, MemoryAttribute, MemoryDescriptor, MemoryMap, MemoryType};

/// An opaque firmware handle.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handle(pub *mut c_void);

/// `EFI_STATUS`: zero is success, the top bit marks an error.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Status(pub usize);

const ERROR_BIT: usize = 1 << (usize::BITS - 1);

impl Status {
    pub const SUCCESS: Status = Status(0);
    pub const LOAD_ERROR: Status = Status(ERROR_BIT | 1);
    pub const INVALID_PARAMETER: Status = Status(ERROR_BIT | 2);
    pub const UNSUPPORTED: Status = Status(ERROR_BIT | 3);
    pub const BAD_BUFFER_SIZE: Status = Status(ERROR_BIT | 4);
    pub const BUFFER_TOO_SMALL: Status = Status(ERROR_BIT | 5);
    pub const NOT_READY: Status = Status(ERROR_BIT | 6);
    pub const DEVICE_ERROR: Status = Status(ERROR_BIT | 7);
    pub const WRITE_PROTECTED: Status = Status(ERROR_BIT | 8);
    pub const OUT_OF_RESOURCES: Status = Status(ERROR_BIT | 9);
    pub const NOT_FOUND: Status = Status(ERROR_BIT | 0x0e);
    pub const ABORTED: Status = Status(ERROR_BIT | 0x15);

    #[must_use]
    pub fn is_error(self) -> bool {
        self.0 & ERROR_BIT != 0
    }

    /// Errors become `Err`; success and warnings become `Ok`.
    pub fn to_result(self) -> Result<()> {
        if self.is_error() { Err(self) } else { Ok(()) }
    }

    fn name(self) -> Option<&'static str> {
        Some(match self {
            Status::SUCCESS => "success",
            Status::LOAD_ERROR => "load error",
            Status::INVALID_PARAMETER => "invalid parameter",
            Status::UNSUPPORTED => "unsupported",
            Status::BAD_BUFFER_SIZE => "bad buffer size",
            Status::BUFFER_TOO_SMALL => "buffer too small",
            Status::NOT_READY => "not ready",
            Status::DEVICE_ERROR => "device error",
            Status::WRITE_PROTECTED => "write protected",
            Status::OUT_OF_RESOURCES => "out of resources",
            Status::NOT_FOUND => "not found",
            Status::ABORTED => "aborted",
            _ => return None,
        })
    }
}

impl fmt::Debug for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(n) => f.write_str(n),
            None if self.is_error() => write!(f, "error {}", self.0 & !ERROR_BIT),
            None => write!(f, "warning {}", self.0),
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

pub type Result<T> = core::result::Result<T, Status>;

/// `EFI_GUID`, in its in-memory (mixed-endian) layout.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    /// Builds a GUID from its canonical textual groups,
    /// `data1-data2-data3-data4[0..2]-data4[2..8]`.
    #[must_use]
    pub const fn new(data1: u32, data2: u16, data3: u16, d4hi: u16, d4lo: u64) -> Guid {
        let h = d4hi.to_be_bytes();
        let l = d4lo.to_be_bytes();
        Guid {
            data1,
            data2,
            data3,
            data4: [h[0], h[1], l[2], l[3], l[4], l[5], l[6], l[7]],
        }
    }
}

impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = &self.data4;
        write!(
            f,
            "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            self.data1, self.data2, self.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
        )
    }
}

impl fmt::Debug for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Well-known configuration-table GUIDs.
pub mod guid {
    use super::Guid;

    pub const ACPI_20_TABLE: Guid = Guid::new(0x8868_e871, 0xe4f1, 0x11d3, 0xbc22, 0x0080_c73c_8881);
    pub const ACPI_10_TABLE: Guid = Guid::new(0xeb9d_2d30, 0x2d88, 0x11d3, 0x9a16, 0x0090_273f_c14d);
    pub const SMBIOS3_TABLE: Guid = Guid::new(0xf2fd_1544, 0x9794, 0x4a2c, 0x992e, 0xe5bb_cf20_e394);
}

/// A protocol interface that can be found with `LocateProtocol`.
///
/// # Safety
///
/// `GUID` must identify a protocol whose interface has exactly the layout
/// of `Self`.
pub unsafe trait Protocol {
    const GUID: Guid;
}

/// A NUL-terminated UCS-2 string owned by the firmware.
#[derive(Clone, Copy)]
pub struct Ucs2Str<'a> {
    ptr: NonNull<u16>,
    _life: PhantomData<&'a [u16]>,
}

impl Ucs2Str<'_> {
    fn units(self) -> impl Iterator<Item = u16> {
        let mut p = self.ptr.as_ptr();
        core::iter::from_fn(move || {
            // SAFETY: the string is NUL-terminated (constructor contract) and
            // we stop at the terminator.
            let u = unsafe { p.read() };
            if u == 0 {
                return None;
            }
            // SAFETY: `u` was not the terminator, so the next unit exists.
            p = unsafe { p.add(1) };
            Some(u)
        })
    }
}

impl fmt::Display for Ucs2Str<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in char::decode_utf16(self.units()) {
            fmt::Write::write_char(f, c.unwrap_or(char::REPLACEMENT_CHARACTER))?;
        }
        Ok(())
    }
}

impl fmt::Debug for Ucs2Str<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"{self}\"")
    }
}

/// UEFI revision, as `major.minor` (minor is in tenths and hundredths, e.g. 2.70).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Revision {
    pub major: u16,
    pub minor: u16,
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:02}", self.major, self.minor)
    }
}

/// Typestate: boot services are available.
#[derive(Debug)]
pub enum Boot {}
/// Typestate: boot services have been exited; only runtime services remain.
#[derive(Debug)]
pub enum Runtime {}

/// The UEFI system table, in boot or runtime state.
#[derive(Debug)]
pub struct SystemTable<State> {
    raw: NonNull<raw::SystemTable>,
    _state: PhantomData<State>,
}

impl<S> SystemTable<S> {
    fn raw(&self) -> &raw::SystemTable {
        // SAFETY: validated in `from_raw`; the firmware keeps it mapped.
        unsafe { self.raw.as_ref() }
    }

    #[must_use]
    pub fn firmware_vendor(&self) -> Option<Ucs2Str<'_>> {
        NonNull::new(self.raw().firmware_vendor.cast_mut()).map(|ptr| Ucs2Str {
            ptr,
            _life: PhantomData,
        })
    }

    #[must_use]
    pub fn firmware_revision(&self) -> u32 {
        self.raw().firmware_revision
    }

    #[must_use]
    pub fn uefi_revision(&self) -> Revision {
        let r = self.raw().hdr.revision;
        #[allow(clippy::cast_possible_truncation)]
        Revision {
            major: (r >> 16) as u16,
            minor: (r & 0xffff) as u16,
        }
    }

    /// The firmware's configuration tables (ACPI, SMBIOS, …).
    #[must_use]
    pub fn config_tables(&self) -> &[raw::ConfigurationTable] {
        let st = self.raw();
        if st.configuration_table.is_null() {
            return &[];
        }
        // SAFETY: the firmware provides `number_of_table_entries` entries.
        unsafe { core::slice::from_raw_parts(st.configuration_table, st.number_of_table_entries) }
    }

    /// Looks up a configuration table by GUID.
    #[must_use]
    pub fn config_table(&self, guid: Guid) -> Option<*const c_void> {
        self.config_tables()
            .iter()
            .find(|t| t.vendor_guid == guid)
            .map(|t| t.vendor_table)
    }

    #[must_use]
    pub fn runtime_services(&self) -> RuntimeServices<'_> {
        // SAFETY: runtime services stay valid for the life of the system.
        RuntimeServices(unsafe { &*self.raw().runtime_services })
    }
}

impl SystemTable<Boot> {
    /// Wraps the table handed to `efi_main`.
    ///
    /// # Safety
    ///
    /// `ptr` must be the system table passed by the firmware to the image's
    /// entry point, boot services must not yet have been exited, and only one
    /// `SystemTable<Boot>` may exist at a time.
    #[must_use]
    pub unsafe fn from_raw(ptr: *mut raw::SystemTable) -> Option<Self> {
        let raw = NonNull::new(ptr)?;
        // SAFETY: caller guarantees a firmware-provided table.
        if unsafe { raw.as_ref() }.hdr.signature != raw::SYSTEM_TABLE_SIGNATURE {
            return None;
        }
        Some(SystemTable {
            raw,
            _state: PhantomData,
        })
    }

    /// The console output device.
    #[must_use]
    pub fn stdout(&self) -> Console<'_> {
        // SAFETY: valid while boot services are active, which `self` proves.
        unsafe { Console::new(self.raw().con_out) }
    }

    #[must_use]
    pub fn boot_services(&self) -> BootServices<'_> {
        // SAFETY: valid while boot services are active, which `self` proves.
        BootServices(unsafe { &*self.raw().boot_services })
    }

    /// Exits boot services. On success the firmware's boot-time memory and
    /// drivers are gone and only runtime services remain.
    ///
    /// `key` must come from the most recent [`BootServices::memory_map`]
    /// call; if the map changed since, the firmware refuses and the boot
    /// state is handed back so the caller can fetch a fresh map and retry.
    pub fn exit_boot_services(
        self,
        image: Handle,
        key: MapKey,
    ) -> core::result::Result<SystemTable<Runtime>, (Self, Status)> {
        let bs = self.boot_services();
        // SAFETY: arguments come from the firmware; `self` is consumed so
        // boot services cannot be reached again through this table.
        let status = unsafe { (bs.0.exit_boot_services)(image, key.0) };
        match status.to_result() {
            Ok(()) => Ok(SystemTable {
                raw: self.raw,
                _state: PhantomData,
            }),
            Err(e) => Err((self, e)),
        }
    }
}

/// Boot services, borrowed from a [`SystemTable<Boot>`].
#[derive(Debug, Clone, Copy)]
pub struct BootServices<'a>(&'a raw::BootServices);

/// `EFI_ALLOCATE_TYPE::AllocateAnyPages`.
const ALLOCATE_ANY_PAGES: u32 = 0;
/// `EFI_ALLOCATE_TYPE::AllocateAddress`.
const ALLOCATE_ADDRESS: u32 = 2;

impl BootServices<'_> {
    /// The raw table, for code (such as a global allocator) that must reach
    /// boot services without a borrow of the system table.
    #[must_use]
    pub fn as_raw(&self) -> *const raw::BootServices {
        self.0
    }

    /// Rebuilds a `BootServices` from [`as_raw`](Self::as_raw).
    ///
    /// # Safety
    ///
    /// `p` must come from `as_raw`, and boot services must still be active
    /// for the returned lifetime.
    #[must_use]
    pub unsafe fn from_raw<'a>(p: *const raw::BootServices) -> BootServices<'a> {
        // SAFETY: caller contract.
        BootServices(unsafe { &*p })
    }

    /// Allocates `pages` contiguous 4 KiB pages anywhere, tagged as `ty`.
    /// Returns the physical (= identity-mapped) address.
    pub fn allocate_pages(&self, ty: MemoryType, pages: usize) -> Result<u64> {
        let mut addr = 0u64;
        // SAFETY: valid out-pointer.
        unsafe { (self.0.allocate_pages)(ALLOCATE_ANY_PAGES, ty.to_raw(), pages, &raw mut addr) }.to_result()?;
        Ok(addr)
    }

    /// Allocates `pages` contiguous 4 KiB pages at physical address `addr`,
    /// tagged as `ty`. Fails with `NOT_FOUND` if any of them is taken.
    pub fn allocate_pages_at(&self, ty: MemoryType, pages: usize, addr: u64) -> Result<u64> {
        let mut addr = addr;
        // SAFETY: valid in/out pointer.
        unsafe { (self.0.allocate_pages)(ALLOCATE_ADDRESS, ty.to_raw(), pages, &raw mut addr) }.to_result()?;
        Ok(addr)
    }

    /// Finds protocol `P` on a specific handle.
    pub fn handle_protocol<P: Protocol>(&self, handle: Handle) -> Result<&P> {
        let guid = P::GUID;
        let mut iface = ptr::null_mut();
        // SAFETY: valid GUID and out-pointer.
        unsafe { (self.0.handle_protocol)(handle, &raw const guid, &raw mut iface) }.to_result()?;
        // SAFETY: as in `locate_protocol`.
        unsafe { iface.cast::<P>().as_ref() }.ok_or(Status::NOT_FOUND)
    }

    /// Allocates `size` bytes from the firmware pool, tagged as `ty`.
    pub fn allocate_pool(&self, ty: MemoryType, size: usize) -> Result<NonNull<u8>> {
        let mut p = ptr::null_mut();
        // SAFETY: valid out-pointer.
        unsafe { (self.0.allocate_pool)(ty.to_raw(), size, &raw mut p) }.to_result()?;
        NonNull::new(p).ok_or(Status::OUT_OF_RESOURCES)
    }

    /// Returns pool memory to the firmware.
    ///
    /// # Safety
    ///
    /// `p` must have come from [`allocate_pool`](Self::allocate_pool) and
    /// must not be used afterwards.
    pub unsafe fn free_pool(&self, p: NonNull<u8>) -> Result<()> {
        // SAFETY: caller contract.
        unsafe { (self.0.free_pool)(p.as_ptr()) }.to_result()
    }

    /// The number of bytes needed for the memory map right now. Allocating
    /// can grow the map, so callers should add slack.
    #[must_use]
    pub fn memory_map_size(&self) -> usize {
        let (mut size, mut key, mut dsize, mut ver) = (0, 0, 0, 0);
        // SAFETY: a zero-sized buffer is allowed and reports the needed size.
        let _ = unsafe {
            (self.0.get_memory_map)(
                &raw mut size,
                ptr::null_mut(),
                &raw mut key,
                &raw mut dsize,
                &raw mut ver,
            )
        };
        size
    }

    /// Reads the current memory map into `buf`.
    pub fn memory_map<'b>(&self, buf: &'b mut [u8]) -> Result<MemoryMap<'b>> {
        let (mut size, mut key, mut dsize, mut ver) = (buf.len(), 0, 0, 0);
        // SAFETY: `buf` is writable for `size` bytes.
        unsafe {
            (self.0.get_memory_map)(
                &raw mut size,
                buf.as_mut_ptr(),
                &raw mut key,
                &raw mut dsize,
                &raw mut ver,
            )
        }
        .to_result()?;
        MemoryMap::new(&buf[..size], dsize, MapKey(key), ver)
    }

    /// Finds the first instance of protocol `P`.
    pub fn locate_protocol<P: Protocol>(&self) -> Result<&P> {
        let guid = P::GUID;
        let mut iface = ptr::null_mut();
        // SAFETY: valid GUID and out-pointer.
        unsafe { (self.0.locate_protocol)(&raw const guid, ptr::null_mut(), &raw mut iface) }.to_result()?;
        // SAFETY: `Protocol` guarantees the interface layout; it lives while
        // boot services do, which our borrow of the system table bounds.
        unsafe { iface.cast::<P>().as_ref() }.ok_or(Status::NOT_FOUND)
    }

    /// Busy-waits for at least `us` microseconds.
    pub fn stall(&self, us: usize) {
        // SAFETY: plain firmware call.
        let _ = unsafe { (self.0.stall)(us) };
    }

    /// Disables the 5-minute boot watchdog that would otherwise reset the
    /// machine while we run.
    pub fn disable_watchdog(&self) -> Result<()> {
        // SAFETY: timeout 0 disables the watchdog; no data string.
        unsafe { (self.0.set_watchdog_timer)(0, 0, 0, ptr::null()) }.to_result()
    }
}

/// `EFI_RESET_TYPE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ResetType {
    Cold = 0,
    Warm = 1,
    Shutdown = 2,
}

/// Runtime services; usable before and after exiting boot services.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeServices<'a>(&'a raw::RuntimeServices);

impl RuntimeServices<'_> {
    /// Resets or powers off the machine.
    pub fn reset(&self, ty: ResetType, status: Status) -> ! {
        // SAFETY: ResetSystem does not return.
        unsafe { (self.0.reset_system)(ty as u32, status, 0, ptr::null()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_display_round_trip() {
        assert_eq!(guid::ACPI_20_TABLE.to_string(), "8868e871-e4f1-11d3-bc22-0080c73c8881");
        assert_eq!(
            guid::ACPI_20_TABLE.data4,
            [0xbc, 0x22, 0x00, 0x80, 0xc7, 0x3c, 0x88, 0x81]
        );
    }

    #[test]
    fn status_classification() {
        assert!(Status::SUCCESS.to_result().is_ok());
        assert_eq!(Status::NOT_FOUND.to_result(), Err(Status::NOT_FOUND));
        assert!(Status(1).to_result().is_ok(), "warnings are not errors");
        assert_eq!(format!("{:?}", Status::BUFFER_TOO_SMALL), "buffer too small");
        assert_eq!(format!("{:?}", Status(ERROR_BIT | 0x63)), "error 99");
    }

    #[test]
    fn ucs2_display() {
        let s: Vec<u16> = "EDK II\u{e9}".encode_utf16().chain([0]).collect();
        let u = Ucs2Str {
            ptr: NonNull::new(s.as_ptr().cast_mut()).unwrap(),
            _life: PhantomData,
        };
        assert_eq!(u.to_string(), "EDK II\u{e9}");
    }
}
