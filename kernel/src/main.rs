//! The Plan R kernel.
//!
//! P1 bring-up: entered from 9boot with the boot page tables live (ADR
//! 0010), the kernel validates the handoff, finds a console through ACPI,
//! and reports what it sees. Interrupts, timers and SMP come next.

#![no_std]
#![no_main]

mod arch;
mod drivers;
mod port;

use bootinfo::{BootInfo, EntryLevel, RegionKind};

use port::console::{self, ConsoleSource};
use port::{acpi, config::Config, mm};

/// Marker lines `cargo xtask test` scans the serial log for.
const OK_MARKER: &str = "kernel: ok";
const PANIC_MARKER: &str = "kernel: PANIC";

/// Called by the architecture entry stub with the stack set up.
extern "C" fn kmain(info: &'static BootInfo) -> ! {
    if info.validate().is_err() {
        // No console yet; nothing useful to say.
        arch::halt();
    }
    mm::init(info);
    let acpi = acpi::System::new(info.rsdp());
    let source = console::init(acpi.as_ref());
    arch::init(acpi.as_ref().ok());

    println!();
    println!("Plan R kernel {} ({})", env!("CARGO_PKG_VERSION"), arch::NAME);
    match info.entry_level {
        EntryLevel::El2 => println!("cpu: entered at EL2, running at EL{}", arch::current_el()),
        EntryLevel::El1 => println!("cpu: entered at EL1"),
        EntryLevel::NotApplicable => {}
    }
    match source {
        ConsoleSource::Spcr(k) => println!("console: {k:?} from SPCR"),
        ConsoleSource::Dbg2(k) => println!("console: {k:?} from DBG2"),
        ConsoleSource::Default(k) => println!("console: {k:?} (default)"),
    }
    report_memory(info);
    if let Some(fb) = info.framebuffer() {
        println!("display: {}x{} {:?} at {:#x}", fb.width, fb.height, fb.format, fb.phys);
    }
    match &acpi {
        Ok(sys) => sys.report(),
        Err(e) => println!("acpi: {e}"),
    }

    let config = Config::parse(info.config());
    if config.test_mode() {
        println!("{OK_MARKER}");
        arch::exit(arch::ExitCode::Success);
    }
    println!("P1 bring-up complete; halting");
    arch::halt()
}

fn report_memory(info: &BootInfo) {
    let mut usable = 0;
    let mut acpi = 0;
    for r in info.memory() {
        match r.kind {
            RegionKind::Usable => usable += r.len,
            RegionKind::AcpiReclaimable | RegionKind::AcpiNvs => acpi += r.len,
            _ => {}
        }
    }
    println!(
        "memory: {} regions, {} MiB usable, {} KiB ACPI; kernel {:#x}+{:#x}",
        info.memory().len(),
        usable >> 20,
        acpi >> 10,
        info.kernel_phys,
        info.kernel_size
    );
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    println!("{PANIC_MARKER}: {info}");
    arch::exit(arch::ExitCode::Failure)
}
