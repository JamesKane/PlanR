//! amd64 support.

pub(crate) mod mmu;
mod trap;

pub(crate) use trap::{breakpoint, frame_pointer, probe_read};

use core::arch::{asm, global_asm};

use acpi::UartKind;

use super::ExitCode;
use crate::drivers::uart16550::Uart16550;
use crate::port::acpi::System;
use crate::port::console::{ConsoleSource, Uart};

/// Installs the trap vectors. First thing the kernel does, so even an
/// early fault powers off with a failure instead of resetting.
pub(crate) fn init_traps() {
    trap::init();
}

pub(crate) const NAME: &str = "amd64";

// Entry from 9boot: rdi = &BootInfo, rsp = top of the boot stack, boot page
// tables live, interrupts off (ADR 0010).
global_asm!(
    ".section .text.boot, \"ax\"",
    ".global _start",
    "_start:",
    "    and rsp, -16",
    "    xor ebp, ebp",
    "    call {kmain}",
    "2:  cli",
    "    hlt",
    "    jmp 2b",
    kmain = sym crate::kmain,
);

pub(crate) fn init(_acpi: Option<&System>) {}

/// No exception levels on amd64.
pub(crate) fn current_el() -> u8 {
    0
}

/// COM1, the conventional PC serial port. Harmless if absent.
pub(crate) fn default_console() -> (Uart, ConsoleSource) {
    (
        Uart::Ns16550(Uart16550::io(0x3f8)),
        ConsoleSource::Default(UartKind::Ns16550),
    )
}

pub(crate) fn halt() -> ! {
    loop {
        // SAFETY: stops this CPU until an interrupt; interrupts are off.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

/// QEMU's `isa-debug-exit` device (port 0xf4) makes QEMU exit with status
/// `(value << 1) | 1`: 99 for success, 97 for failure. On real hardware the
/// write goes nowhere and we halt.
pub(crate) fn exit(code: ExitCode) -> ! {
    let v: u32 = match code {
        ExitCode::Success => 0x31,
        ExitCode::Failure => 0x30,
    };
    // SAFETY: a write to an unused I/O port has no effect.
    unsafe { asm!("out dx, eax", in("dx") 0xf4u16, in("eax") v, options(nomem, nostack)) };
    halt()
}

pub(crate) fn inb(port: u16) -> u8 {
    let v: u8;
    // SAFETY: port I/O to a device the caller owns.
    unsafe { asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack)) };
    v
}

pub(crate) fn outb(port: u16, v: u8) {
    // SAFETY: port I/O to a device the caller owns.
    unsafe { asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack)) };
}
