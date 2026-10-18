//! The High Precision Event Timer (IA-PC HPET specification 1.0a): a
//! fixed-frequency main counter and a few comparators. Plan R uses it to
//! calibrate the TSC and to test interrupt routing; the TSC and local APIC
//! do the real timekeeping.

const CAPABILITIES: u64 = 0x000;
const CONFIG: u64 = 0x010;
const MAIN_COUNTER: u64 = 0x0f0;

const CAP_COUNT_64: u64 = 1 << 13;
const CONFIG_ENABLE: u64 = 1 << 0;

/// Per-comparator registers, 0x20 apart from 0x100.
const TIMER_CONFIG: u64 = 0x100;
const TIMER_COMPARATOR: u64 = 0x108;
const TIMER_STRIDE: u64 = 0x20;

const TN_INT_ENABLE: u64 = 1 << 2;
const TN_PERIODIC: u64 = 1 << 3;
const TN_32BIT: u64 = 1 << 8;
const TN_ROUTE_SHIFT: u64 = 9;
const TN_ROUTE_MASK: u64 = 0x1f << TN_ROUTE_SHIFT;
const TN_FSB: u64 = 1 << 14;

const FS_PER_SEC: u128 = 1_000_000_000_000_000;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Hpet {
    base: u64,
}

impl Hpet {
    /// `base` is the virtual address of the register block (Device memory).
    /// Returns `None` if the block reads as absent or nonsensical.
    pub(crate) fn new(base: u64) -> Option<Hpet> {
        let h = Hpet { base };
        let period = h.period_fs();
        // The specification caps the period at 100 ns; all-ones is no device.
        (period != 0 && period <= 100_000_000).then_some(h)
    }

    fn read(self, reg: u64) -> u64 {
        // SAFETY: `base` maps the HPET register block.
        unsafe { ((self.base + reg) as *const u64).read_volatile() }
    }

    fn write(self, reg: u64, v: u64) {
        // SAFETY: as above.
        unsafe { ((self.base + reg) as *mut u64).write_volatile(v) }
    }

    /// Main counter tick period, in femtoseconds.
    pub(crate) fn period_fs(self) -> u64 {
        self.read(CAPABILITIES) >> 32
    }

    pub(crate) fn frequency(self) -> u64 {
        u64::try_from(FS_PER_SEC / u128::from(self.period_fs().max(1))).unwrap_or(0)
    }

    pub(crate) fn comparators(self) -> usize {
        ((self.read(CAPABILITIES) >> 8) & 0x1f) as usize + 1
    }

    /// Starts the main counter if the firmware left it stopped.
    pub(crate) fn enable(self) {
        let c = self.read(CONFIG);
        if c & CONFIG_ENABLE == 0 {
            self.write(CONFIG, c | CONFIG_ENABLE);
        }
    }

    /// Main counter ticks between two readings, allowing for the wrap of a
    /// 32-bit counter.
    #[allow(clippy::cast_possible_truncation)] // a 32-bit counter's value
    pub(crate) fn elapsed(self, from: u64, to: u64) -> u64 {
        if self.read(CAPABILITIES) & CAP_COUNT_64 != 0 {
            to.wrapping_sub(from)
        } else {
            u64::from((to as u32).wrapping_sub(from as u32))
        }
    }

    pub(crate) fn counter(self) -> u64 {
        self.read(MAIN_COUNTER)
    }

    /// Main counter ticks in `ns` nanoseconds.
    pub(crate) fn ticks(self, ns: u64) -> u64 {
        u64::try_from(u128::from(ns) * 1_000_000 / u128::from(self.period_fs().max(1))).unwrap_or(u64::MAX)
    }

    /// The interrupt inputs (I/O APIC pins) comparator `n` can drive.
    pub(crate) fn route_capability(self, n: usize) -> u32 {
        (self.read(TIMER_CONFIG + TIMER_STRIDE * n as u64) >> 32) as u32
    }

    /// Arms comparator `n` to raise one edge on I/O APIC input `pin` when
    /// the main counter reaches `at`.
    pub(crate) fn arm(self, n: usize, pin: u32, at: u64) {
        let reg = TIMER_CONFIG + TIMER_STRIDE * n as u64;
        let mut c = self.read(reg);
        c &= !(TN_PERIODIC | TN_32BIT | TN_FSB | TN_ROUTE_MASK | 1 << 1);
        c |= (u64::from(pin) << TN_ROUTE_SHIFT) | TN_INT_ENABLE;
        self.write(reg, c);
        self.write(TIMER_COMPARATOR + TIMER_STRIDE * n as u64, at);
    }

    /// Stops comparator `n` interrupting.
    pub(crate) fn disarm(self, n: usize) {
        let reg = TIMER_CONFIG + TIMER_STRIDE * n as u64;
        self.write(reg, self.read(reg) & !TN_INT_ENABLE);
    }
}
