//! Arm PL011 and SBSA generic UART: polled transmit, interrupt-driven
//! receive. The firmware has already configured the line.

#[derive(Debug)]
pub(crate) struct Pl011 {
    base: u64,
}

const DR: u64 = 0x00;
const FR: u64 = 0x18;
const FR_RXFE: u32 = 1 << 4;
const FR_TXFF: u32 = 1 << 5;
const IMSC: u64 = 0x38;
const ICR: u64 = 0x44;
/// Receive, and receive timeout (a byte waits below the FIFO level).
const RX_INTERRUPTS: u32 = (1 << 4) | (1 << 6);

impl Pl011 {
    /// `base` is the virtual address of the register block (Device memory).
    pub(crate) fn new(base: u64) -> Self {
        Pl011 { base }
    }

    fn reg(&self, off: u64) -> *mut u32 {
        (self.base + off) as *mut u32
    }

    /// Interrupts when a byte arrives.
    pub(crate) fn enable_rx(&mut self) {
        // SAFETY: `base` maps the UART's registers.
        unsafe {
            self.reg(ICR).write_volatile(RX_INTERRUPTS);
            let m = self.reg(IMSC).read_volatile();
            self.reg(IMSC).write_volatile(m | RX_INTERRUPTS);
        }
    }

    /// The next byte received, if any. Reading it clears the receive
    /// interrupts once the FIFO is empty.
    pub(crate) fn getc(&mut self) -> Option<u8> {
        // SAFETY: as above.
        unsafe {
            if self.reg(FR).read_volatile() & FR_RXFE != 0 {
                return None;
            }
            #[allow(clippy::cast_possible_truncation)] // the data byte; the rest are error bits
            Some(self.reg(DR).read_volatile() as u8)
        }
    }

    pub(crate) fn putc(&mut self, b: u8) {
        // SBSA generic UARTs require 32-bit accesses, which PL011 accepts.
        let fr = (self.base + FR) as *const u32;
        let dr = (self.base + DR) as *mut u32;
        for _ in 0..100_000 {
            // SAFETY: `base` maps the UART's registers.
            if unsafe { fr.read_volatile() } & FR_TXFF == 0 {
                // SAFETY: as above.
                unsafe { dr.write_volatile(u32::from(b)) };
                return;
            }
            core::hint::spin_loop();
        }
    }
}
