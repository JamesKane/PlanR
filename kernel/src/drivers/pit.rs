//! The 8254 programmable interval timer. Plan R uses only channel 2, whose
//! gate and output are wired to port 0x61 rather than to an interrupt, to
//! calibrate the TSC on machines with neither a CPUID-reported frequency
//! nor an HPET.

use crate::arch::{inb, outb};

/// The PIT's input clock.
pub(crate) const HZ: u64 = 1_193_182;

const CHANNEL2: u16 = 0x42;
const COMMAND: u16 = 0x43;
/// NMI status and control: channel 2's gate, the speaker, channel 2's output.
const PORT_B: u16 = 0x61;
const GATE2: u8 = 1 << 0;
const SPEAKER: u8 = 1 << 1;
const OUT2: u8 = 1 << 5;

/// Channel 2, low then high byte, mode 0 (interrupt on terminal count),
/// binary.
const CHANNEL2_ONESHOT: u8 = 0b1011_0000;

/// The calibration window. The 16-bit count allows up to 54 ms.
const WINDOW_MS: u64 = 50;

/// Fewer polls than this before the output rises means the PIT did not
/// count (a missing PIT's port reads as all ones). Each poll is a port
/// read of about a microsecond.
const MIN_POLLS: u32 = 1_000;
/// More than this and the output never rose: about 2 s.
const MAX_POLLS: u32 = 2_000_000;

/// The rate of `counter`, measured against a one-shot count on channel 2,
/// or `None` if the PIT does not respond.
pub(crate) fn frequency_of(counter: impl Fn() -> u64) -> Option<u64> {
    let count = u16::try_from(HZ * WINDOW_MS / 1000).ok()?;
    let saved = inb(PORT_B);
    // Gate on, speaker off. Mode 0 takes its output low on the command
    // and starts counting once the count is loaded, gate high.
    outb(PORT_B, (saved & !SPEAKER) | GATE2);
    outb(COMMAND, CHANNEL2_ONESHOT);
    let [lo, hi] = count.to_le_bytes();
    outb(CHANNEL2, lo);
    outb(CHANNEL2, hi);
    let start = counter();
    let mut end = start;
    let mut polls = 0;
    while inb(PORT_B) & OUT2 == 0 && polls <= MAX_POLLS {
        end = counter();
        polls += 1;
    }
    outb(PORT_B, saved & !SPEAKER);
    if !(MIN_POLLS..=MAX_POLLS).contains(&polls) {
        return None;
    }
    u64::try_from(u128::from(end - start) * u128::from(HZ) / u128::from(count)).ok()
}
