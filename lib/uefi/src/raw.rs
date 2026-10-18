//! Raw UEFI table layouts (UEFI 2.10), exactly as the firmware lays them out.
//!
//! Everything here is `#[repr(C)]` and uses raw pointers; the safe layer in
//! the rest of the crate is the only intended user. Function slots that 9boot
//! does not call are typed as [`Unused`] so the layout stays exact without
//! spelling out every signature.

use core::ffi::c_void;

use crate::{Guid, Handle, Status};

/// A function-pointer slot we never call.
pub type Unused = *const c_void;

/// `EFI_TABLE_HEADER`.
#[repr(C)]
#[derive(Debug)]
pub struct TableHeader {
    pub signature: u64,
    pub revision: u32,
    pub header_size: u32,
    pub crc32: u32,
    pub reserved: u32,
}

/// `EFI_SYSTEM_TABLE`.
#[repr(C)]
#[derive(Debug)]
pub struct SystemTable {
    pub hdr: TableHeader,
    pub firmware_vendor: *const u16,
    pub firmware_revision: u32,
    pub console_in_handle: Handle,
    pub con_in: *mut c_void,
    pub console_out_handle: Handle,
    pub con_out: *mut SimpleTextOutput,
    pub standard_error_handle: Handle,
    pub std_err: *mut SimpleTextOutput,
    pub runtime_services: *mut RuntimeServices,
    pub boot_services: *mut BootServices,
    pub number_of_table_entries: usize,
    pub configuration_table: *const ConfigurationTable,
}

/// `EFI_SYSTEM_TABLE.Hdr.Signature`: "IBI SYST".
pub const SYSTEM_TABLE_SIGNATURE: u64 = 0x5453_5953_2049_4249;

/// `EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL`.
#[repr(C)]
#[derive(Debug)]
pub struct SimpleTextOutput {
    pub reset: unsafe extern "efiapi" fn(*mut SimpleTextOutput, bool) -> Status,
    pub output_string: unsafe extern "efiapi" fn(*mut SimpleTextOutput, *const u16) -> Status,
    pub test_string: Unused,
    pub query_mode: Unused,
    pub set_mode: Unused,
    pub set_attribute: Unused,
    pub clear_screen: Unused,
    pub set_cursor_position: Unused,
    pub enable_cursor: Unused,
    pub mode: *mut c_void,
}

/// `EFI_BOOT_SERVICES`.
#[repr(C)]
#[derive(Debug)]
pub struct BootServices {
    pub hdr: TableHeader,
    // Task priority
    pub raise_tpl: Unused,
    pub restore_tpl: Unused,
    // Memory
    pub allocate_pages: unsafe extern "efiapi" fn(u32, u32, usize, *mut u64) -> Status,
    pub free_pages: Unused,
    pub get_memory_map: unsafe extern "efiapi" fn(
        map_size: *mut usize,
        map: *mut u8,
        map_key: *mut usize,
        descriptor_size: *mut usize,
        descriptor_version: *mut u32,
    ) -> Status,
    pub allocate_pool: unsafe extern "efiapi" fn(u32, usize, *mut *mut u8) -> Status,
    pub free_pool: unsafe extern "efiapi" fn(*mut u8) -> Status,
    // Events and timers
    pub create_event: Unused,
    pub set_timer: Unused,
    pub wait_for_event: Unused,
    pub signal_event: Unused,
    pub close_event: Unused,
    pub check_event: Unused,
    // Protocol handlers
    pub install_protocol_interface: Unused,
    pub reinstall_protocol_interface: Unused,
    pub uninstall_protocol_interface: Unused,
    pub handle_protocol: unsafe extern "efiapi" fn(Handle, *const Guid, *mut *mut c_void) -> Status,
    pub reserved: Unused,
    pub register_protocol_notify: Unused,
    pub locate_handle: Unused,
    pub locate_device_path: Unused,
    pub install_configuration_table: Unused,
    // Images
    pub load_image: Unused,
    pub start_image: Unused,
    pub exit: Unused,
    pub unload_image: Unused,
    pub exit_boot_services: unsafe extern "efiapi" fn(Handle, usize) -> Status,
    // Miscellaneous
    pub get_next_monotonic_count: Unused,
    pub stall: unsafe extern "efiapi" fn(usize) -> Status,
    pub set_watchdog_timer: unsafe extern "efiapi" fn(usize, u64, usize, *const u16) -> Status,
    // Driver support
    pub connect_controller: Unused,
    pub disconnect_controller: Unused,
    // Open and close protocol
    pub open_protocol: Unused,
    pub close_protocol: Unused,
    pub open_protocol_information: Unused,
    // Library
    pub protocols_per_handle: Unused,
    pub locate_handle_buffer: Unused,
    pub locate_protocol: unsafe extern "efiapi" fn(*const Guid, *mut c_void, *mut *mut c_void) -> Status,
    pub install_multiple_protocol_interfaces: Unused,
    pub uninstall_multiple_protocol_interfaces: Unused,
    // CRC
    pub calculate_crc32: Unused,
    // Memory utilities
    pub copy_mem: Unused,
    pub set_mem: Unused,
    pub create_event_ex: Unused,
}

/// `EFI_RUNTIME_SERVICES`.
#[repr(C)]
#[derive(Debug)]
pub struct RuntimeServices {
    pub hdr: TableHeader,
    pub get_time: Unused,
    pub set_time: Unused,
    pub get_wakeup_time: Unused,
    pub set_wakeup_time: Unused,
    pub set_virtual_address_map: Unused,
    pub convert_pointer: Unused,
    pub get_variable: Unused,
    pub get_next_variable_name: Unused,
    pub set_variable: Unused,
    pub get_next_high_monotonic_count: Unused,
    pub reset_system: unsafe extern "efiapi" fn(u32, Status, usize, *const c_void) -> !,
    pub update_capsule: Unused,
    pub query_capsule_capabilities: Unused,
    pub query_variable_info: Unused,
}

/// `EFI_CONFIGURATION_TABLE`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ConfigurationTable {
    pub vendor_guid: Guid,
    pub vendor_table: *const c_void,
}

/// `EFI_MEMORY_DESCRIPTOR`. The firmware's stride may be larger than this
/// struct; always step by the reported descriptor size.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MemoryDescriptor {
    pub ty: u32,
    pub physical_start: u64,
    pub virtual_start: u64,
    pub number_of_pages: u64,
    pub attribute: u64,
}

/// `EFI_FILE_PROTOCOL` (revision 1 prefix; revision 2 appends async calls).
#[repr(C)]
#[derive(Debug)]
pub struct FileProtocol {
    pub revision: u64,
    pub open: unsafe extern "efiapi" fn(*mut FileProtocol, *mut *mut FileProtocol, *const u16, u64, u64) -> Status,
    pub close: unsafe extern "efiapi" fn(*mut FileProtocol) -> Status,
    pub delete: Unused,
    pub read: unsafe extern "efiapi" fn(*mut FileProtocol, *mut usize, *mut c_void) -> Status,
    pub write: unsafe extern "efiapi" fn(*mut FileProtocol, *mut usize, *const c_void) -> Status,
    pub get_position: Unused,
    pub set_position: Unused,
    pub get_info: unsafe extern "efiapi" fn(*mut FileProtocol, *const Guid, *mut usize, *mut c_void) -> Status,
    pub set_info: unsafe extern "efiapi" fn(*mut FileProtocol, *const Guid, usize, *const c_void) -> Status,
    pub flush: unsafe extern "efiapi" fn(*mut FileProtocol) -> Status,
}

/// `EFI_SIMPLE_FILE_SYSTEM_PROTOCOL`.
#[repr(C)]
#[derive(Debug)]
pub struct SimpleFileSystem {
    pub revision: u64,
    pub open_volume: unsafe extern "efiapi" fn(*mut SimpleFileSystem, *mut *mut FileProtocol) -> Status,
}

/// `EFI_LOADED_IMAGE_PROTOCOL`.
#[repr(C)]
#[derive(Debug)]
pub struct LoadedImage {
    pub revision: u32,
    pub parent_handle: Handle,
    pub system_table: *mut SystemTable,
    pub device_handle: Handle,
    pub file_path: *const c_void,
    pub reserved: *const c_void,
    pub load_options_size: u32,
    pub load_options: *const c_void,
    pub image_base: *const c_void,
    pub image_size: u64,
    pub image_code_type: u32,
    pub image_data_type: u32,
    pub unload: Unused,
}

/// `EFI_GRAPHICS_OUTPUT_PROTOCOL`.
#[repr(C)]
#[derive(Debug)]
pub struct GraphicsOutput {
    pub query_mode: Unused,
    pub set_mode: Unused,
    pub blt: Unused,
    pub mode: *const GraphicsOutputMode,
}

/// `EFI_GRAPHICS_OUTPUT_PROTOCOL_MODE`.
#[repr(C)]
#[derive(Debug)]
pub struct GraphicsOutputMode {
    pub max_mode: u32,
    pub mode: u32,
    pub info: *const GraphicsOutputModeInformation,
    pub size_of_info: usize,
    pub frame_buffer_base: u64,
    pub frame_buffer_size: usize,
}

/// `EFI_GRAPHICS_OUTPUT_MODE_INFORMATION`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GraphicsOutputModeInformation {
    pub version: u32,
    pub horizontal_resolution: u32,
    pub vertical_resolution: u32,
    pub pixel_format: u32,
    pub pixel_information: [u32; 4],
    pub pixels_per_scan_line: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    const P: usize = size_of::<usize>();

    #[test]
    fn system_table_layout() {
        assert_eq!(size_of::<TableHeader>(), 24);
        assert_eq!(offset_of!(SystemTable, firmware_vendor), 24);
        assert_eq!(offset_of!(SystemTable, configuration_table), 24 + 11 * P);
    }

    #[test]
    fn boot_services_layout() {
        // 44 function slots after the header (UEFI 2.10, through CreateEventEx).
        assert_eq!(size_of::<BootServices>(), 24 + 44 * P);
        assert_eq!(offset_of!(BootServices, get_memory_map), 24 + 4 * P);
        assert_eq!(offset_of!(BootServices, exit_boot_services), 24 + 26 * P);
        assert_eq!(offset_of!(BootServices, locate_protocol), 24 + 37 * P);
    }

    #[test]
    fn runtime_services_layout() {
        assert_eq!(size_of::<RuntimeServices>(), 24 + 14 * P);
        assert_eq!(offset_of!(RuntimeServices, reset_system), 24 + 10 * P);
    }

    #[test]
    fn memory_descriptor_layout() {
        assert_eq!(size_of::<MemoryDescriptor>(), 40);
        assert_eq!(offset_of!(MemoryDescriptor, physical_start), 8);
    }
}
