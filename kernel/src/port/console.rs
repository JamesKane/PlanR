//! The early console: one UART chosen from ACPI SPCR, then DBG2, then the
//! architecture's default.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

use acpi::{AddressSpace, Gas, UartKind};

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

static CONSOLE: SpinLock<Option<Uart>> = SpinLock::new(None);

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
    *CONSOLE.lock() = Some(uart);
    source
}

struct Writer<'a>(&'a mut Uart);

impl Write for Writer<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                self.0.putc(b'\r');
            }
            self.0.putc(b);
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

pub(crate) fn print(args: fmt::Arguments<'_>) {
    // Never wait on the lock: bring-up is single-CPU, so contention means
    // re-entry (a trap or panic while printing). Outside panic mode the
    // output is dropped; in panic mode the lock is taken over.
    let guard = CONSOLE.try_lock().or_else(|| {
        if !PANICKING.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: the panic path never returns to the interrupted printer.
        unsafe { CONSOLE.force_unlock() };
        CONSOLE.try_lock()
    });
    if let Some(mut c) = guard
        && let Some(u) = c.as_mut()
    {
        let _ = Writer(u).write_fmt(args);
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
