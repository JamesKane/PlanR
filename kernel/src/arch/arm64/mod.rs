//! arm64 support.

pub(crate) mod mmu;
mod trap;

pub(crate) use trap::{breakpoint, frame_pointer, probe_read};

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU8, Ordering};

use acpi::{PsciConduit, UartKind};

use super::ExitCode;
use crate::drivers::uart16550::Uart16550;
use crate::port::acpi::System;
use crate::port::console::{ConsoleSource, Uart};

/// Installs the trap vectors. First thing the kernel does, so even an
/// early fault powers off with a failure instead of resetting.
pub(crate) fn init_traps() {
    trap::init();
}

pub(crate) const NAME: &str = "arm64";

// Entry from 9boot: x0 = &BootInfo, sp = top of the boot stack, EL1 with the
// boot page tables live, DAIF masked (ADR 0010).
global_asm!(
    ".section .text.boot, \"ax\"",
    ".global _start",
    "_start:",
    "    mov x29, xzr",
    "    mov x30, xzr",
    "    bl {kmain}",
    "1:  wfe",
    "    b 1b",
    kmain = sym crate::kmain,
);

const PSCI_NONE: u8 = 0;
const PSCI_SMC: u8 = 1;
const PSCI_HVC: u8 = 2;
static PSCI: AtomicU8 = AtomicU8::new(PSCI_NONE);

/// Records how to reach PSCI firmware, from the FADT.
pub(crate) fn init(acpi: Option<&System>) {
    let c = match acpi.and_then(|s| s.tables.fadt()).and_then(|f| f.psci()) {
        Some(PsciConduit::Smc) => PSCI_SMC,
        Some(PsciConduit::Hvc) => PSCI_HVC,
        None => PSCI_NONE,
    };
    PSCI.store(c, Ordering::Relaxed);
}

pub(crate) fn current_el() -> u8 {
    let el: u64;
    // SAFETY: reading CurrentEL has no side effects.
    unsafe { asm!("mrs {}, CurrentEL", out(reg) el, options(nomem, nostack)) };
    ((el >> 2) & 3) as u8
}

/// No conventional default on arm64: without SPCR or DBG2 there is no
/// console. A dead UART keeps the rest of the code uniform.
pub(crate) fn default_console() -> (Uart, ConsoleSource) {
    (
        Uart::Ns16550(Uart16550::io(0)),
        ConsoleSource::Default(UartKind::Other(0xffff)),
    )
}

pub(crate) fn halt() -> ! {
    loop {
        // SAFETY: waits for an interrupt; DAIF is masked, so this idles.
        unsafe { asm!("wfi", options(nomem, nostack)) };
    }
}

/// PSCI `SYSTEM_OFF`. QEMU exits with status 0 either way; the test harness
/// tells success from failure by the console markers.
pub(crate) fn exit(_code: ExitCode) -> ! {
    const SYSTEM_OFF: u64 = 0x8400_0008;
    match PSCI.load(Ordering::Relaxed) {
        // SAFETY: SYSTEM_OFF does not return on success; on failure the
        // firmware returns an error code in x0, which we ignore.
        PSCI_SMC => unsafe { asm!("smc #0", inout("x0") SYSTEM_OFF => _, options(nomem, nostack)) },
        // SAFETY: as above.
        PSCI_HVC => unsafe { asm!("hvc #0", inout("x0") SYSTEM_OFF => _, options(nomem, nostack)) },
        _ => {}
    }
    halt()
}

/// arm64 has no I/O ports. Reads float high, so a 16550 "at port 0" looks
/// absent and its output is dropped.
pub(crate) fn inb(_port: u16) -> u8 {
    0xff
}

pub(crate) fn outb(_port: u16, _v: u8) {}
