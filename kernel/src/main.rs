//! The Plan R kernel.
//!
//! P1 bring-up: entered from 9boot with the boot page tables live (ADR
//! 0010), the kernel validates the handoff, finds a console through ACPI
//! (and draws it on the framebuffer and keeps it in the log ring),
//! installs its trap vectors, takes over the interrupt controller, starts
//! the clock and the other CPUs, and reports what it sees.

#![no_std]
#![no_main]

extern crate alloc;

mod arch;
mod drivers;
mod port;

use bootinfo::{BootInfo, EntryLevel, RegionKind};

use port::config::{self, Config, TestMode};
use port::console::{self, ConsoleSource};
use port::early::{self, Stage};
use port::{acpi, clock, cpu, devcons, devroot, devrtc, exec, intr, ktest, mm, timer, trap};

/// Marker lines `cargo xtask test` scans the serial log for.
const OK_MARKER: &str = "kernel: ok";
pub(crate) const PANIC_MARKER: &str = "kernel: PANIC";
/// The last line before `test=reset` resets; 9boot must echo it.
const RESET_LINE: &str = "ktest: warm reset to check the log ring";

/// Called by the architecture entry stub with the stack set up.
extern "C" fn kmain(info: &'static BootInfo) -> ! {
    early::mark(info, Stage::Entered, true);
    arch::init_traps();
    early::mark(info, Stage::Traps, true);
    arch::mmu::init_cpu();
    arch::protect::init_cpu();
    arch::context::init_cpu();
    cpu::init_boot();
    if info.validate().is_err() {
        // No console yet: the magenta mark is all there is to say.
        early::mark(info, Stage::BootInfoValid, false);
        arch::halt();
    }
    early::mark(info, Stage::BootInfoValid, true);
    if mm::init(info).is_err() {
        early::mark(info, Stage::Memory, false);
        arch::halt();
    }
    early::mark(info, Stage::Memory, true);
    // Leave 9boot's stack, which has no guard below it, for a kernel stack.
    let Some(stack) = mm::alloc_stack(mm::STACK_PAGES) else {
        early::mark(info, Stage::KernelStack, false);
        arch::halt();
    };
    arch::run_on_stack(stack, boot_main, core::ptr::from_ref(info) as usize)
}

/// The rest of the boot CPU's start-up, on a kernel stack.
extern "C" fn boot_main(info: usize) -> ! {
    // SAFETY: `kmain` passed the address of its `&'static BootInfo`.
    let info: &'static BootInfo = unsafe { &*(info as *const BootInfo) };
    early::mark(info, Stage::KernelStack, true);
    let acpi = acpi::System::new(info.rsdp());
    early::mark(info, Stage::Acpi, acpi.is_ok());
    let source = console::init(acpi.as_ref());
    early::mark(info, Stage::Console, true);
    let screen = info.framebuffer().map(|fb| (fb, console::attach_screen(fb)));
    if matches!(screen, Some((_, None))) {
        early::mark(info, Stage::Screen, false);
    }
    let ring = info.log_ring().map(|r| (r, console::attach_ring(r)));
    arch::init(acpi.as_ref().ok());

    println!();
    println!("Plan R kernel {} ({})", env!("CARGO_PKG_VERSION"), arch::NAME);
    match info.entry_level {
        EntryLevel::El2 => println!("cpu: entered at EL2, running at EL{}", arch::current_el()),
        EntryLevel::El1 => println!("cpu: entered at EL1"),
        EntryLevel::NotApplicable => {}
    }
    arch::protect::report();
    match source {
        ConsoleSource::Spcr(k) => println!("console: {k:?} from SPCR"),
        ConsoleSource::Dbg2(k) => println!("console: {k:?} from DBG2"),
        ConsoleSource::Default(k) => println!("console: {k:?} (default)"),
    }
    report_memory(info);
    if let Some((fb, screen)) = screen {
        print!("display: {}x{} {:?} at {:#x}", fb.width, fb.height, fb.format, fb.phys);
        match screen {
            Some(s) => println!("; text {}x{} at scale {}", s.cols, s.rows, s.scale),
            None => println!("; not usable for text"),
        }
    }
    let ring_boot = match ring {
        Some((r, Some(boot))) => {
            println!("log: ring at {:#x}, {} KiB, boot {boot}", r.phys, r.size >> 10);
            Some(boot)
        }
        Some((r, None)) => {
            println!("log: ring at {:#x} not usable", r.phys);
            None
        }
        None => {
            println!("log: no ring");
            None
        }
    };
    match &acpi {
        Ok(sys) => sys.report(),
        Err(e) => println!("acpi: {e}"),
    }
    devroot::init(info);
    let config = Config::parse(info.config());
    config.apply();
    let interrupts = arch::init_interrupts(acpi.as_ref().ok(), &config);
    match interrupts {
        Ok(counter_hz) => {
            clock::init(counter_hz);
            clock::start();
            timer::start();
            arch::spllo();
            println!("clock: ticking at {} Hz", clock::HZ);
            cpu::start_others(acpi.as_ref().ok());
        }
        Err(e) => println!("intr: {e}; continuing without interrupts"),
    }
    devrtc::init(info.boot_time);
    if interrupts.is_ok() {
        devcons::start_input();
    }

    match config.test_mode() {
        Some(TestMode::Run) => {
            let failed = ktest::run();
            assert!(failed == 0, "{failed} kernel tests failed");
            run_init(&["init"], INIT_SECS)
        }
        Some(TestMode::Cons) => run_init(&["init", "-i"], INIT_SECS),
        Some(TestMode::Oracle) => run_init(&["init", "-o"], ORACLE_SECS),
        Some(TestMode::Trap) => trap_test(),
        Some(TestMode::Overflow) => overflow_test(),
        Some(TestMode::AcpiDump) => {
            match &acpi {
                Ok(sys) => sys.dump(),
                Err(e) => panic!("no ACPI tables to dump: {e}"),
            }
            println!("{OK_MARKER}");
            arch::exit(arch::ExitCode::Success);
        }
        Some(TestMode::Reset) => reset_test(ring_boot, acpi.as_ref().ok()),
        None => {}
    }
    report_interrupts();
    // There is no shell yet: init runs the test programs and exits, and
    // the kernel idles on.
    if let Err(e) = exec::start_init(&["init"]) {
        println!("kernel: cannot start /boot/init: {e}");
    }
    println!("kernel: boot complete; idling");
    cpu::idle_loop(cpu::current())
}

/// `test=trap`: a deliberate fatal page fault, for the harness to check
/// the report.
fn trap_test() -> ! {
    println!("ktest: deliberate page fault at {:#x}", trap::UNMAPPED);
    // SAFETY: none; this is meant to fault, and the fault is fatal.
    let v = unsafe { core::ptr::read_volatile(trap::UNMAPPED as *const u64) };
    panic!("read of unmapped {:#x} returned {v:#x}", trap::UNMAPPED);
}

/// How long init may take to run the test programs.
const INIT_SECS: u64 = 60;
/// How long it may take to run the oracle's scripts, on arm64 under
/// emulation too.
const ORACLE_SECS: u64 = 600;

/// After the in-kernel tests: what a real boot does, but waited for. Init
/// runs the test programs (with `-i`, the one that reads the console) and
/// must exit normally within `secs` seconds.
fn run_init(argv: &'static [&'static str], secs: u64) -> ! {
    let exit = match exec::start_init(argv) {
        Ok(exit) => exit,
        Err(e) => panic!("cannot start /boot/init: {e}"),
    };
    assert!(
        clock::wait_until(|| exit.lock().is_some(), secs * 1_000_000_000),
        "init did not finish within {secs} s"
    );
    let msg = exit.lock().take().unwrap_or_default();
    assert!(
        msg.is_empty(),
        "init exited: {}",
        alloc::string::String::from_utf8_lossy(&msg)
    );
    println!("{OK_MARKER}");
    arch::exit(arch::ExitCode::Success);
}

/// `test=reset`: the first boot of a fresh log ring resets warm; the next
/// passes if 9boot handed the ring on.
fn reset_test(ring_boot: Option<u64>, acpi: Option<&acpi::System>) -> ! {
    match ring_boot {
        Some(1) => {
            println!("ktest: resetting through {}", arch::reset_method(acpi));
            // On a board (exit=halt: the screen is the console), leave time
            // to read how it resets.
            if config::halt_on_exit() {
                println!("ktest: resetting in 10 s");
                clock::spin_until(|| false, 10_000_000_000);
            }
            println!("{RESET_LINE}");
            arch::reset(acpi);
        }
        Some(_) => {
            println!("{OK_MARKER}");
            arch::exit(arch::ExitCode::Success);
        }
        None => panic!("test=reset needs a log ring"),
    }
}

/// `test=overflow`: overflows another CPU's stack if one is online, else
/// the boot CPU's. The overflow report powers the machine off.
fn overflow_test() -> ! {
    let me = cpu::current().index();
    if let Some(i) = cpu::online().map(cpu::Cpu::index).filter(|&i| i != me).last() {
        println!("ktest: overflowing CPU {i}'s stack");
        let called = cpu::call(i, overflow_stack);
        assert!(called.is_ok(), "CPU {i} refused the call");
    } else {
        println!("ktest: overflowing the boot CPU's stack");
        overflow_stack();
    }
    arch::halt()
}

/// Recurses until the stack overflows.
fn overflow_stack() {
    #[allow(unconditional_recursion)]
    fn recurse(depth: u64) -> u64 {
        let frame = core::hint::black_box([depth; 32]);
        recurse(depth + 1) + frame[0]
    }
    core::hint::black_box(recurse(0));
}

fn report_interrupts() {
    intr::each(|v, name, n| println!("intr: {v:#x} {name}: {n}"));
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
    let (free, total, arenas) = mm::frame_counts();
    println!(
        "memory: frames {} MiB free of {} MiB in {arenas} arenas",
        (free * mm::PAGE) >> 20,
        (total * mm::PAGE) >> 20
    );
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    if console::begin_panic() {
        println!("{PANIC_MARKER}: {info}");
        trap::backtrace(None, arch::frame_pointer());
    }
    arch::exit(arch::ExitCode::Failure)
}
