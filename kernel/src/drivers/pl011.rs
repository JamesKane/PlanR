//! Arm PL011 and SBSA generic UART, polled transmit only. The firmware has
//! already configured the line; we only write characters.

#[derive(Debug)]
pub(crate) struct Pl011 {
    base: u64,
}

const DR: u64 = 0x00;
const FR: u64 = 0x18;
const FR_TXFF: u32 = 1 << 5;

impl Pl011 {
    /// `base` is the virtual address of the register block (Device memory).
    pub(crate) fn new(base: u64) -> Self {
        Pl011 { base }
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
