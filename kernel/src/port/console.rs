//! The early console. Output goes to three places:
//!
//! - the persistent log ring (`lib/logring`), which 9boot saves to the boot
//!   volume after a warm reset;
//! - the framebuffer, as text (`lib/fbtext`);
//! - one UART, chosen from ACPI SPCR, then DBG2, then the architecture's
//!   default.

use core::cell::UnsafeCell;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use acpi::{AddressSpace, Gas, UartKind};
use bootinfo::{Framebuffer, LogRing, PixelFormat};
use logring::Ring;

use crate::arch;
use crate::drivers::{pl011::Pl011, uart16550::Uart16550};
use crate::port::acpi::{System, Unavailable};
use crate::port::lock::SpinLock;
use crate::port::mm::{self, MapKind};

/// A UART the console can drive.
#[derive(Debug)]
pub(crate) enum Uart {
    Ns16550(Uart16550),
    Pl011(Pl011),
}

impl Uart {
    fn putc(&mut self, b: u8) {
        match self {
            Uart::Ns16550(u) => u.putc(b),
            Uart::Pl011(u) => u.putc(b),
        }
    }

    /// Builds a driver for a firmware-described UART, mapping its registers.
    fn from_acpi(kind: UartKind, base: Gas) -> Option<Uart> {
        match (base.space, kind) {
            (AddressSpace::SystemIo, k) if k.is_16550_like() => {
                Some(Uart::Ns16550(Uart16550::io(u16::try_from(base.address).ok()?)))
            }
            (AddressSpace::SystemMemory, k) if k.is_16550_like() => {
                let va = mm::map_phys(base.address, mm::PAGE, MapKind::Device)?;
                // access_size 3 (dword) means 32-bit registers, 4 bytes apart.
                let shift = if base.access_size == 3 { 2 } else { 0 };
                Some(Uart::Ns16550(Uart16550::mmio(va, shift)))
            }
            (AddressSpace::SystemMemory, k) if k.is_pl011_like() => {
                let va = mm::map_phys(base.address, mm::PAGE, MapKind::Device)?;
                Some(Uart::Pl011(Pl011::new(va)))
            }
            _ => None,
        }
    }
}

/// Where the console came from, for the boot report.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ConsoleSource {
    Spcr(UartKind),
    Dbg2(UartKind),
    Default(UartKind),
}

/// Where output goes.
#[derive(Debug)]
struct Outputs {
    ring: Option<Ring>,
    screen: Option<fbtext::Console<'static>>,
    uart: Option<Uart>,
}

static CONSOLE: SpinLock<Outputs> = SpinLock::new(Outputs {
    ring: None,
    screen: None,
    uart: None,
});
/// The index of the CPU printing, or [`NOBODY`].
static HOLDER: AtomicUsize = AtomicUsize::new(NOBODY);
const NOBODY: usize = usize::MAX;

pub(crate) fn init(acpi: Result<&System, &Unavailable>) -> ConsoleSource {
    let mut chosen = None;
    if let Ok(sys) = acpi {
        if let Some(s) = sys.tables.spcr()
            && let Some(u) = Uart::from_acpi(s.kind, s.base)
        {
            chosen = Some((u, ConsoleSource::Spcr(s.kind)));
        }
        if chosen.is_none() {
            chosen = sys.tables.dbg2().find_map(|d| {
                let kind = d.uart()?;
                Uart::from_acpi(kind, d.base?).map(|u| (u, ConsoleSource::Dbg2(kind)))
            });
        }
    }
    let (uart, source) = chosen.unwrap_or_else(arch::default_console);
    CONSOLE.lock().uart = Some(uart);
    source
}

/// The text grid's backing store: room for 480×270 cells, enough for a
/// 4K screen at scale 1 or an 8K one at scale 4.
struct Cells(UnsafeCell<[u8; 480 * 270]>);

// SAFETY: handed out once, by `take_cells`.
unsafe impl Sync for Cells {}

static CELLS: Cells = Cells(UnsafeCell::new([0; 480 * 270]));

/// [`CELLS`], the first time only.
fn take_cells() -> Option<&'static mut [u8]> {
    static TAKEN: AtomicBool = AtomicBool::new(false);
    // SAFETY: the flag makes this the only reference ever made.
    (!TAKEN.swap(true, Ordering::AcqRel)).then(|| unsafe { &mut *CELLS.0.get() }.as_mut_slice())
}

/// The text console on the framebuffer, for the boot report.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Screen {
    pub(crate) cols: usize,
    pub(crate) rows: usize,
    pub(crate) scale: usize,
}

/// Starts drawing console output on the firmware's framebuffer.
pub(crate) fn attach_screen(fb: Framebuffer) -> Option<Screen> {
    let width = usize::try_from(fb.width).ok()?;
    let height = usize::try_from(fb.height).ok()?;
    let stride = usize::try_from(fb.stride).ok()?;
    let bytes = (stride * height * 4) as u64;
    // A framebuffer in allocatable RAM would be handed out as free pages
    // (and is cacheable through the direct map).
    if bytes > fb.size || mm::overlaps_usable(fb.phys, bytes) {
        return None;
    }
    let va = mm::map_phys(fb.phys, bytes, MapKind::WriteCombining)?;
    let surface = fbtext::Surface {
        base: va as *mut u32,
        width,
        height,
        stride,
        format: match fb.format {
            PixelFormat::Rgbx => fbtext::Format::Rgbx,
            PixelFormat::Bgrx => fbtext::Format::Bgrx,
        },
    };
    // SAFETY: the mapping covers `stride * height` pixels, and nothing else
    // in the kernel touches the framebuffer.
    let screen = unsafe { fbtext::Console::new(surface, take_cells()?) }?;
    let info = Screen {
        cols: screen.cols(),
        rows: screen.rows(),
        scale: screen.scale(),
    };
    CONSOLE.lock().screen = Some(screen);
    Some(info)
}

/// Starts copying console output into the log ring 9boot set up, and
/// returns the ring's boot number.
pub(crate) fn attach_ring(r: LogRing) -> Option<u64> {
    let va = mm::map_phys(r.phys, r.size, MapKind::WriteCombining)?;
    // SAFETY: the mapping covers the ring, which is outside the direct map
    // and used by nothing else.
    let ring = unsafe { Ring::new(va as *mut u8, usize::try_from(r.size).ok()?) }?;
    let boot = ring.found()?.boot;
    CONSOLE.lock().ring = Some(ring);
    Some(boot)
}

struct Writer<'a>(&'a mut Outputs);

impl Write for Writer<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let o = &mut *self.0;
        if let Some(r) = o.ring.as_mut() {
            r.write(s.as_bytes());
        }
        if let Some(screen) = o.screen.as_mut() {
            screen.write(s.as_bytes());
        }
        if let Some(u) = o.uart.as_mut() {
            for b in s.bytes() {
                if b == b'\n' {
                    u.putc(b'\r');
                }
                u.putc(b);
            }
        }
        Ok(())
    }
}

static PANICKING: AtomicBool = AtomicBool::new(false);

/// Enters panic mode: from now on, output takes the console even if the
/// code that panicked holds it. Returns false if already panicking, so a
/// nested panic can give up instead of recursing.
pub(crate) fn begin_panic() -> bool {
    !PANICKING.swap(true, Ordering::AcqRel)
}

/// How many times a panicking CPU tries for a console another CPU holds
/// before taking it over.
const PANIC_PATIENCE: u32 = 10_000_000;

pub(crate) fn print(args: fmt::Arguments<'_>) {
    let me = arch::cpu_index();
    let panicking = PANICKING.load(Ordering::Acquire);
    let guard = if HOLDER.load(Ordering::Relaxed) == me {
        // Re-entered on this CPU: a trap or panic while printing. Outside
        // panic mode the output is dropped; in it the console is taken over.
        if !panicking {
            return;
        }
        // SAFETY: the panic path never returns to the interrupted printer.
        unsafe { CONSOLE.force_unlock() };
        CONSOLE.try_lock()
    } else if panicking {
        // Another CPU may have stopped while holding it.
        (0..PANIC_PATIENCE).find_map(|_| CONSOLE.try_lock()).or_else(|| {
            // SAFETY: as above; a CPU stuck that long is not coming back.
            unsafe { CONSOLE.force_unlock() };
            CONSOLE.try_lock()
        })
    } else {
        Some(CONSOLE.lock())
    };
    if let Some(mut c) = guard {
        HOLDER.store(me, Ordering::Relaxed);
        let _ = Writer(&mut c).write_fmt(args);
        HOLDER.store(NOBODY, Ordering::Relaxed);
    }
}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use super::CONSOLE;
    use crate::port::ktest::{Fail, TestResult};
    use crate::println;

    /// A line printed now is drawn on the screen.
    pub(crate) fn screen_shows_text() -> TestResult {
        if CONSOLE.lock().screen.is_none() {
            return Err(Fail::Skipped("no framebuffer"));
        }
        println!("ktest: drawing on the screen");
        let outputs = CONSOLE.lock();
        let screen = outputs.screen.as_ref().ok_or("screen gone")?;
        // The cursor is at the start of the line after the one printed.
        let (_, row) = screen.cursor();
        let row = row.checked_sub(1).ok_or("cursor did not move")?;
        if !screen.text(row).starts_with(b"ktest: drawing on the screen") {
            return Err("line not in the text grid".into());
        }
        // Read back the first inked pixel of the line's first glyph, `k`.
        let (left, top) = screen.cell_origin(0, row);
        let glyph = fbtext::glyph(b'k');
        let gy = glyph.iter().position(|&r| r != 0).ok_or("blank glyph")?;
        let gx = glyph[gy].leading_zeros() as usize;
        let scale = screen.scale();
        if screen.pixel(left + gx * scale, top + gy * scale) == Some(screen.ink()) {
            Ok(())
        } else {
            Err("glyph not on the screen".into())
        }
    }

    /// A line printed now is in the log ring.
    pub(crate) fn ring_records_output() -> TestResult {
        const LINE: &[u8] = b"ktest: checking the log ring\n";
        if CONSOLE.lock().ring.is_none() {
            return Err(Fail::Skipped("no log ring"));
        }
        println!("ktest: checking the log ring");
        let outputs = CONSOLE.lock();
        let (older, newer) = outputs.ring.as_ref().ok_or("ring gone")?.contents();
        if joined_ends_with(older, newer, LINE) {
            Ok(())
        } else {
            Err("line not at the end of the ring".into())
        }
    }

    /// True if `a` followed by `b` ends with `want`.
    fn joined_ends_with(a: &[u8], b: &[u8], want: &[u8]) -> bool {
        match want.len().checked_sub(b.len()) {
            None | Some(0) => b.ends_with(want),
            Some(n) => want.ends_with(b) && a.ends_with(&want[..n]),
        }
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::port::console::print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::port::console::print(format_args!("{}\n", format_args!($($arg)*))) };
}
