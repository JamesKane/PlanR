//! 9boot: the Plan R UEFI loader.
//!
//! P0 scope: prove the toolchain, image and QEMU path end to end. 9boot
//! reports what the firmware handed it (vendor, memory map, ACPI RSDP,
//! framebuffer), prints the `9boot: ok` marker that `cargo xtask test`
//! looks for, and powers the machine off. Loading the kernel comes in P1.

#![no_std]
#![no_main]

use core::fmt::Write;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicPtr, Ordering};

use uefi::{Boot, GraphicsOutput, Handle, MemoryType, ResetType, Status, SystemTable, guid, raw};

/// Marker lines `cargo xtask test` scans the serial log for.
const OK_MARKER: &str = "9boot: ok";
const PANIC_MARKER: &str = "9boot: PANIC";

#[cfg(target_arch = "x86_64")]
const ARCH: &str = "amd64";
#[cfg(target_arch = "aarch64")]
const ARCH: &str = "arm64";

/// The system table, kept for the panic handler.
static SYSTEM_TABLE: AtomicPtr<raw::SystemTable> = AtomicPtr::new(core::ptr::null_mut());

/// Writes a formatted line to the firmware console, ignoring errors.
macro_rules! println {
    ($st:expr, $($arg:tt)*) => {{
        let _ = writeln!($st.stdout(), $($arg)*);
    }};
}

#[unsafe(no_mangle)]
extern "efiapi" fn efi_main(image: Handle, st: *mut raw::SystemTable) -> Status {
    SYSTEM_TABLE.store(st, Ordering::Relaxed);
    // SAFETY: `st` is the table the firmware passed to our entry point, and
    // this is the only `SystemTable<Boot>` we create.
    let Some(st) = (unsafe { SystemTable::<Boot>::from_raw(st) }) else {
        return Status::INVALID_PARAMETER;
    };
    let _ = image; // needed for ExitBootServices in P1

    match run(&st) {
        Ok(()) => {
            println!(st, "{OK_MARKER}");
            st.runtime_services().reset(ResetType::Shutdown, Status::SUCCESS)
        }
        Err(e) => {
            println!(st, "9boot: error: {e}");
            e
        }
    }
}

fn run(st: &SystemTable<Boot>) -> uefi::Result<()> {
    let bs = st.boot_services();
    bs.disable_watchdog()?;

    println!(st, "\nPlan R 9boot {} ({ARCH})", env!("CARGO_PKG_VERSION"));
    match st.firmware_vendor() {
        Some(v) => println!(
            st,
            "firmware: {v} rev {:#x}, UEFI {}",
            st.firmware_revision(),
            st.uefi_revision()
        ),
        None => println!(st, "firmware: unknown, UEFI {}", st.uefi_revision()),
    }

    report_memory(st)?;
    report_acpi(st);
    report_display(st);
    println!(st, "no kernel yet (P1); powering off");
    Ok(())
}

fn report_memory(st: &SystemTable<Boot>) -> uefi::Result<()> {
    let bs = st.boot_services();
    // Allocating the buffer can add descriptors, so leave room for a few.
    let size = bs.memory_map_size() + 8 * 64;
    let buf = bs.allocate_pool(MemoryType::LoaderData, size)?;
    // SAFETY: the pool allocation is `size` bytes and exclusively ours.
    let slice = unsafe { core::slice::from_raw_parts_mut(buf.as_ptr(), size) };
    let result = bs.memory_map(slice).map(|map| {
        let usable: u64 = map
            .iter()
            .filter(|d| d.ty.is_usable_after_exit())
            .map(|d| d.bytes())
            .sum();
        let top = map.iter().map(|d| d.phys_start + d.bytes()).max().unwrap_or(0);
        println!(
            st,
            "memory: {} descriptors, {} MiB usable after exit, top {top:#x}",
            map.len(),
            usable >> 20
        );
    });
    // SAFETY: `buf` came from allocate_pool and `slice` is no longer used.
    unsafe { bs.free_pool(buf) }?;
    result
}

fn report_acpi(st: &SystemTable<Boot>) {
    let Some(rsdp) = st.config_table(guid::ACPI_20_TABLE) else {
        println!(st, "acpi: no ACPI 2.0 RSDP (ACPI is required; see ADR 0003)");
        return;
    };
    // SAFETY: the firmware's ACPI 2.0 config table points at an RSDP, which
    // is at least 36 bytes long.
    let bytes = unsafe { core::slice::from_raw_parts(rsdp.cast::<u8>(), 36) };
    let sig_ok = &bytes[..8] == b"RSD PTR ";
    let oem = core::str::from_utf8(&bytes[9..15]).unwrap_or("?");
    println!(
        st,
        "acpi: RSDP at {:p}, revision {}, OEM \"{oem}\"{}",
        rsdp,
        bytes[15],
        if sig_ok { "" } else { " (BAD SIGNATURE)" }
    );
}

fn report_display(st: &SystemTable<Boot>) {
    match st
        .boot_services()
        .locate_protocol::<GraphicsOutput>()
        .map(GraphicsOutput::mode_info)
    {
        Ok(Some(m)) => println!(
            st,
            "display: {}x{} {:?} stride {} fb {:#x}+{:#x}", m.width, m.height, m.format, m.stride, m.fb_base, m.fb_size
        ),
        Ok(None) => println!(st, "display: GOP present, no mode"),
        Err(e) => println!(st, "display: none ({e})"),
    }
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    let st = SYSTEM_TABLE.load(Ordering::Relaxed);
    // SAFETY: if set, this is the firmware's table. We may be panicking after
    // creating the first wrapper, but we never return to it.
    if let Some(st) = unsafe { SystemTable::<Boot>::from_raw(st) } {
        println!(st, "{PANIC_MARKER}: {info}");
        st.runtime_services().reset(ResetType::Shutdown, Status::ABORTED);
    }
    loop {
        core::hint::spin_loop();
    }
}
