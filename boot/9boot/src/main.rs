//! 9boot: the Plan R UEFI loader.
//!
//! Loads `\planr\kernel` (a static ELF) and `\planr\plan9.ini` from the
//! volume it was started from, builds the boot page tables, collects the
//! memory map and firmware facts into a `BootInfo`, exits boot services, and
//! jumps to the kernel (ADR 0010). On the way it saves the log ring the
//! previous boot left in RAM to `\planr\last.log`.

#![no_std]
#![no_main]

extern crate alloc;

mod bootnext;
mod handoff;
mod logring;
mod memmap;
mod paging;
mod pool;

use alloc::vec;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicPtr, Ordering};

use bootinfo::{BootInfo, Framebuffer, HHDM_BASE, Region, Slice, phys_to_virt, uefi_type};
use elf::{Elf, Machine};
use handoff::Handoff;
use paging::{Native, Perm, Tables};
use uefi::{
    Boot, File, GraphicsOutput, Handle, LoadedImage, MemoryType, PixelFormat, ResetType, SimpleFileSystem, Status,
    SystemTable, guid, raw,
};

/// Marker lines `cargo xtask test` scans for.
const PANIC_MARKER: &str = "9boot: PANIC";
const ERROR_MARKER: &str = "9boot: error";

const KERNEL_PATH: &str = "planr\\kernel";
const CONFIG_PATH: &str = "planr\\plan9.ini";
const STACK_PAGES: usize = 16;
/// Extra memory-map entries to allow for, since allocating changes the map.
const MAP_SLACK: usize = 64;
const PAGE: u64 = 4096;
const PAGE_BYTES: usize = 4096;

#[cfg(target_arch = "x86_64")]
const ARCH: (&str, Machine) = ("amd64", Machine::X86_64);
#[cfg(target_arch = "aarch64")]
const ARCH: (&str, Machine) = ("arm64", Machine::Aarch64);

/// The system table, kept for the panic handler.
static SYSTEM_TABLE: AtomicPtr<raw::SystemTable> = AtomicPtr::new(core::ptr::null_mut());

macro_rules! println {
    ($st:expr, $($arg:tt)*) => {{
        let _ = writeln!($st.stdout(), $($arg)*);
    }};
}

/// Why booting failed.
#[derive(Debug)]
enum Error {
    Uefi(&'static str, Status),
    Elf(elf::Error),
    Kernel(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Uefi(what, s) => write!(f, "{what}: {s}"),
            Error::Elf(e) => write!(f, "{KERNEL_PATH}: {e}"),
            Error::Kernel(why) => write!(f, "{KERNEL_PATH}: {why}"),
        }
    }
}

/// Adds context to a UEFI status.
trait Context<T> {
    fn ctx(self, what: &'static str) -> Result<T, Error>;
}

impl<T> Context<T> for uefi::Result<T> {
    fn ctx(self, what: &'static str) -> Result<T, Error> {
        self.map_err(|s| Error::Uefi(what, s))
    }
}

#[unsafe(no_mangle)]
extern "efiapi" fn efi_main(image: Handle, st_ptr: *mut raw::SystemTable) -> Status {
    SYSTEM_TABLE.store(st_ptr, Ordering::Relaxed);
    // SAFETY: `st_ptr` is the table the firmware passed to our entry point,
    // and this is the only `SystemTable<Boot>` we create.
    let Some(st) = (unsafe { SystemTable::<Boot>::from_raw(st_ptr) }) else {
        return Status::INVALID_PARAMETER;
    };
    match prepare(&st, image, st_ptr as u64) {
        Ok(p) => exit_and_jump(st, image, p),
        Err(e) => {
            println!(st, "{ERROR_MARKER}: {e}");
            st.boot_services().stall(5_000_000);
            Status::LOAD_ERROR
        }
    }
}

/// Everything ready for the jump, built while boot services were up.
struct Prepared {
    handoff: Handoff,
    info: *mut BootInfo,
    regions: *mut Region,
    capacity: usize,
    map_buf: Vec<u8>,
}

fn prepare(st: &SystemTable<Boot>, image: Handle, st_phys: u64) -> Result<Prepared, Error> {
    let bs = st.boot_services();
    bs.disable_watchdog().ctx("disabling watchdog")?;
    pool::enable(&bs);

    println!(st, "\nPlan R 9boot {} ({})", env!("CARGO_PKG_VERSION"), ARCH.0);
    if let Some(v) = st.firmware_vendor() {
        println!(
            st,
            "firmware: {v} rev {:#x}, UEFI {}",
            st.firmware_revision(),
            st.uefi_revision()
        );
    }

    // Files from the boot volume.
    let li = bs.handle_protocol::<LoadedImage>(image).ctx("loaded image")?;
    let fs = bs.handle_protocol::<SimpleFileSystem>(li.device()).ctx("boot volume")?;
    let root = fs.open_volume().ctx("opening boot volume")?;
    let config = match read_file(&root, CONFIG_PATH) {
        Ok(c) => c,
        Err(Status::NOT_FOUND) => Vec::new(),
        Err(e) => return Err(Error::Uefi("reading plan9.ini", e)),
    };
    let kernel_file = read_file(&root, KERNEL_PATH).ctx("reading kernel")?;
    // Before anything else allocates much: the previous ring is in free
    // memory until it is reserved.
    // A reset test: read its marker before `setup` saves over it.
    let reset_test = bootnext::config_is(&config, "test", "reset");
    let pending = if reset_test { bootnext::pending(&root) } else { None };
    // What the old ring's memory holds, read before `setup` can reuse it.
    let before = pending.map(|p| (p.ring, bootnext::survey(st, &p)));
    let log_ring = logring::setup(st, &root);
    if reset_test {
        match (log_ring, before) {
            // A fresh ring after the first boot: the old one was lost.
            (Some((_, 1)), Some((at, what))) => bootnext::lost(st, &root, at, &what),
            // The first boot arranges the second.
            (Some((ring, 1)), None) => match bootnext::arrange(st, &root, ring.phys) {
                Ok(n) => println!(st, "9boot: BootNext = {n:04X}, so Plan R starts again after the reset"),
                Err(e) => println!(
                    st,
                    "9boot: cannot arrange a second boot ({e}); the reset may boot another OS"
                ),
            },
            _ => {}
        }
    }

    let kernel = load_kernel(st, &kernel_file)?;
    let elf = &kernel.elf;

    // Page tables: identity + direct map over RAM, then the kernel.
    let mut map_buf = vec![0u8; bs.memory_map_size() * 2 + MAP_SLACK * 64];
    let (ram, descriptors) = {
        let map = bs.memory_map(&mut map_buf).ctx("memory map")?;
        (memmap::ram_ranges(&map), map.len())
    };
    let fb = framebuffer(st);
    match fb {
        Some(f) => println!(
            st,
            "9boot: display {}x{} {:?} at {:#x}", f.width, f.height, f.format, f.phys
        ),
        None => println!(st, "9boot: no linear framebuffer; the kernel will have no screen"),
    }
    let tables = build_tables(st, &ram, &kernel, fb)?;

    // Boot stack and the BootInfo block: [BootInfo][Region; capacity][config].
    let stack = bs
        .allocate_pages(MemoryType::Other(uefi_type::BOOT), STACK_PAGES)
        .ctx("boot stack")?;
    let capacity = descriptors + MAP_SLACK;
    let regions_off = size_of::<BootInfo>().next_multiple_of(16);
    let config_off = regions_off + capacity * size_of::<Region>();
    let block_pages = (config_off + config.len()).div_ceil(PAGE_BYTES);
    let block = bs
        .allocate_pages(MemoryType::Other(uefi_type::BOOT), block_pages)
        .ctx("boot info")?;
    // SAFETY: freshly allocated and identity-mapped.
    unsafe { core::ptr::write_bytes(block as *mut u8, 0, block_pages * PAGE_BYTES) };
    let config_phys = block + config_off as u64;
    // SAFETY: the block holds `config.len()` bytes at `config_off`.
    unsafe { core::ptr::copy_nonoverlapping(config.as_ptr(), config_phys as *mut u8, config.len()) };

    let mut info = BootInfo::new(handoff::entry_level());
    info.rsdp = st.config_table(guid::ACPI_20_TABLE).map_or(0, |p| p as u64);
    info.uefi_system_table = st_phys;
    info.set_framebuffer(fb);
    info.set_log_ring(log_ring.map(|(r, _)| r));
    info.kernel_phys = kernel.phys;
    info.kernel_size = kernel.size;
    info.config = Slice::new(phys_to_virt(config_phys) as *const u8, config.len());
    let info_ptr = block as *mut BootInfo;
    // SAFETY: the block starts with room for a BootInfo, page aligned.
    unsafe { info_ptr.write(info) };

    println!(
        st,
        "9boot: {} RAM ranges mapped, {} table pages; starting kernel",
        ram.len(),
        tables.pages_used
    );
    // The final GetMemoryMap must fit without allocating.
    let need = bs.memory_map_size() + MAP_SLACK * 64;
    if map_buf.len() < need {
        map_buf = vec![0u8; need * 2];
    }
    Ok(Prepared {
        handoff: Handoff {
            roots: tables.roots,
            entry: elf.entry(),
            stack_top: phys_to_virt(stack + STACK_PAGES as u64 * PAGE),
            info: phys_to_virt(block),
        },
        info: info_ptr,
        regions: (block + regions_off as u64) as *mut Region,
        capacity,
        map_buf,
    })
}

/// The kernel image, loaded contiguously at `phys` (linked at `lo`).
struct Kernel<'a> {
    elf: Elf<'a>,
    lo: u64,
    phys: u64,
    size: u64,
}

fn load_kernel<'a>(st: &SystemTable<Boot>, file: &'a [u8]) -> Result<Kernel<'a>, Error> {
    let bs = st.boot_services();
    let elf = Elf::parse(file, ARCH.1).map_err(Error::Elf)?;
    let (lo, hi) = elf.span();
    let lo = lo & !(PAGE - 1);
    let size = hi.next_multiple_of(PAGE) - lo;
    let pages = usize::try_from(size / PAGE).map_err(|_| Error::Kernel("too large"))?;
    let phys = bs
        .allocate_pages(MemoryType::Other(uefi_type::KERNEL), pages)
        .ctx("kernel memory")?;
    // SAFETY: freshly allocated, identity-mapped, `pages` pages.
    let mem = unsafe { core::slice::from_raw_parts_mut(phys as *mut u8, pages * PAGE_BYTES) };
    mem.fill(0);
    for s in elf.segments() {
        let off = usize::try_from(s.vaddr - lo).map_err(|_| Error::Kernel("bad segment"))?;
        let data = elf.file_data(&s);
        mem[off..off + data.len()].copy_from_slice(data);
    }
    #[cfg(target_arch = "aarch64")]
    handoff::prepare_exec(phys, size);
    println!(
        st,
        "kernel: {} KiB, entry {:#x}, loaded at {phys:#x}",
        file.len() >> 10,
        elf.entry()
    );
    Ok(Kernel { elf, lo, phys, size })
}

/// Identity and direct maps over `ram`, then the kernel segments with
/// their own permissions (W^X).
fn build_tables<'a>(
    st: &'a SystemTable<Boot>,
    ram: &[(u64, u64)],
    k: &Kernel<'_>,
    fb: Option<Framebuffer>,
) -> Result<Tables<'a, Native>, Error> {
    let mut tables = Tables::<Native>::new(st.boot_services()).ctx("page tables")?;
    for &(start, end) in ram {
        tables
            .map(start, start, end - start, Perm::ReadWriteExec)
            .ctx("identity map")?;
        tables
            .map(HHDM_BASE + start, start, end - start, Perm::ReadWrite)
            .ctx("direct map")?;
    }
    // The framebuffer in the direct map too, uncached, so the kernel can
    // mark its progress before it maps anything (`kernel/src/port/early.rs`).
    // Not if it is RAM (QEMU's ramfb): that is mapped already.
    if let Some(fb) = fb {
        let (start, end) = (fb.phys & !(PAGE - 1), (fb.phys + fb.size).next_multiple_of(PAGE));
        if !ram.iter().any(|&(s, e)| start < e && s < end) {
            tables
                .map(HHDM_BASE + start, start, end - start, Perm::Uncached)
                .ctx("framebuffer map")?;
        }
    }
    let mut prev_end = 0;
    for s in k.elf.segments() {
        let va = s.vaddr & !(PAGE - 1);
        let end = (s.vaddr + s.memsz).next_multiple_of(PAGE);
        if va < prev_end {
            return Err(Error::Kernel("segments share a page"));
        }
        prev_end = end;
        let perm = match (s.perms.write, s.perms.exec) {
            (true, true) => return Err(Error::Kernel("writable and executable segment (W^X)")),
            (false, true) => Perm::ReadExec,
            (true, false) => Perm::ReadWrite,
            (false, false) => Perm::ReadOnly,
        };
        tables.map(va, k.phys + (va - k.lo), end - va, perm).ctx("kernel map")?;
    }
    Ok(tables)
}

/// Collects the final memory map, exits boot services and enters the
/// kernel. Nothing here may allocate.
fn exit_and_jump(mut st: SystemTable<Boot>, image: Handle, mut p: Prepared) -> ! {
    pool::disable();
    // SAFETY: `regions` points at `capacity` Regions inside the BootInfo block.
    let regions = unsafe { core::slice::from_raw_parts_mut(p.regions, p.capacity) };
    for _ in 0..4 {
        let fetched = {
            let bs = st.boot_services();
            bs.memory_map(&mut p.map_buf)
                .ok()
                .and_then(|map| Some((memmap::convert(&map, regions)?, map.key())))
        };
        let Some((n, key)) = fetched else { break };
        // SAFETY: `info` was written in `prepare`; nothing else aliases it.
        unsafe { (*p.info).memory = Slice::new(phys_to_virt(p.regions as u64) as *const Region, n) };
        match st.exit_boot_services(image, key) {
            // SAFETY: boot services are gone; the handoff was built for this.
            Ok(_runtime) => unsafe { handoff::jump(&p.handoff) },
            // The map changed under us (an event fired): fetch it again.
            Err((again, _)) => st = again,
        }
    }
    println!(st, "{ERROR_MARKER}: could not exit boot services");
    st.runtime_services().reset(ResetType::Shutdown, Status::ABORTED)
}

pub(crate) fn read_file(root: &File<'_>, path: &str) -> uefi::Result<Vec<u8>> {
    let mut f = root.open(path)?;
    let len = usize::try_from(f.size()?).map_err(|_| Status::BAD_BUFFER_SIZE)?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// The framebuffer of the display the console is on. With several GPUs
/// (the Ryzen: an RTX 4070 driving the monitor, a Radeon iGPU idle), the
/// first Graphics Output the firmware lists may be one nothing shows, so
/// the console output handle's comes first.
fn framebuffer(st: &SystemTable<Boot>) -> Option<Framebuffer> {
    let bs = st.boot_services();
    let gop = bs
        .handle_protocol::<GraphicsOutput>(st.console_out_handle())
        .or_else(|_| bs.locate_protocol::<GraphicsOutput>())
        .ok()?;
    let m = gop.mode_info()?;
    let format = match m.format {
        PixelFormat::Rgbx => bootinfo::PixelFormat::Rgbx,
        PixelFormat::Bgrx => bootinfo::PixelFormat::Bgrx,
        PixelFormat::Bitmask { .. } | PixelFormat::BltOnly => return None,
    };
    Some(Framebuffer {
        phys: m.fb_base,
        size: m.fb_size as u64,
        width: m.width,
        height: m.height,
        stride: m.stride,
        format,
    })
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    let st = SYSTEM_TABLE.load(Ordering::Relaxed);
    // SAFETY: if set, this is the firmware's table. The code after
    // ExitBootServices cannot panic (no allocation, no fallible calls).
    if let Some(st) = unsafe { SystemTable::<Boot>::from_raw(st) } {
        println!(st, "{PANIC_MARKER}: {info}");
        st.runtime_services().reset(ResetType::Shutdown, Status::ABORTED);
    }
    loop {
        core::hint::spin_loop();
    }
}
