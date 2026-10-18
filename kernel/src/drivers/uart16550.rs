//! NS16550-compatible UART, polled transmit only.

/// How the registers are reached.
#[derive(Debug, Clone, Copy)]
enum Regs {
    /// x86 I/O ports starting at the given base.
    Io(u16),
    /// Memory-mapped, register `n` at `base + (n << shift)`.
    Mmio { base: u64, shift: u32 },
}

#[derive(Debug)]
pub(crate) struct Uart16550 {
    regs: Regs,
    /// Set if the device looks absent (LSR reads all ones); output is then
    /// dropped instead of spinning forever.
    absent: bool,
}

const THR: u16 = 0;
const IER: u16 = 1;
const FCR: u16 = 2;
const LCR: u16 = 3;
const MCR: u16 = 4;
const LSR: u16 = 5;
const LSR_THRE: u8 = 1 << 5;

impl Uart16550 {
    /// An I/O-port UART, reprogrammed to 115200 8N1 with FIFOs on.
    pub(crate) fn io(base: u16) -> Self {
        let mut u = Uart16550 {
            regs: Regs::Io(base),
            absent: false,
        };
        u.write(IER, 0x00);
        u.write(LCR, 0x80); // DLAB: divisor follows
        u.write(0, 0x01); // 115200 = 1.8432 MHz / 16 / 1
        u.write(1, 0x00);
        u.write(LCR, 0x03); // 8N1
        u.write(FCR, 0xc7); // enable and clear FIFOs
        u.write(MCR, 0x03); // DTR, RTS; no interrupts
        u.absent = u.read(LSR) == 0xff;
        u
    }

    /// A memory-mapped UART at virtual address `base`, left as the firmware
    /// configured it.
    pub(crate) fn mmio(base: u64, shift: u32) -> Self {
        let mut u = Uart16550 {
            regs: Regs::Mmio { base, shift },
            absent: false,
        };
        u.absent = u.read(LSR) == 0xff;
        u
    }

    fn read(&self, reg: u16) -> u8 {
        match self.regs {
            Regs::Io(base) => crate::arch::inb(base + reg),
            // SAFETY: `base` maps the device's registers (Device memory).
            Regs::Mmio { base, shift } => unsafe { ((base + (u64::from(reg) << shift)) as *const u8).read_volatile() },
        }
    }

    fn write(&mut self, reg: u16, v: u8) {
        match self.regs {
            Regs::Io(base) => crate::arch::outb(base + reg, v),
            // SAFETY: as in `read`.
            Regs::Mmio { base, shift } => unsafe {
                ((base + (u64::from(reg) << shift)) as *mut u8).write_volatile(v);
            },
        }
    }

    pub(crate) fn putc(&mut self, b: u8) {
        if self.absent {
            return;
        }
        for _ in 0..100_000 {
            if self.read(LSR) & LSR_THRE != 0 {
                self.write(THR, b);
                return;
            }
            core::hint::spin_loop();
        }
    }
}
