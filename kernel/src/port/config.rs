//! `plan9.ini`-style boot configuration: `key=value` lines, `#` comments.
//!
//! Keys the kernel reads:
//!
//! - `test=1` runs the in-kernel tests and powers off; `test=trap` takes a
//!   deliberate fatal fault; `test=reset` resets warm once and checks that
//!   9boot recovered the log ring.
//! - `apic=xapic` (amd64) keeps the local APIC in xAPIC mode even if the
//!   CPU has x2APIC.
//! - `apictimer=oneshot` (amd64) uses the local APIC timer's one-shot count
//!   mode even if the CPU has TSC-deadline mode.
//! - `tsc=hpet` or `tsc=pit` (amd64) calibrates the TSC against that timer
//!   only, even if CPUID reports the frequency or an HPET is present.

#[derive(Debug, Clone, Copy)]
pub(crate) struct Config<'a> {
    text: &'a str,
}

/// What a test image asks the kernel to do (`test=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TestMode {
    /// `test=1`: run the in-kernel tests, report, and power off.
    Run,
    /// `test=trap`: take a deliberate fatal page fault, so the harness can
    /// check the trap report.
    Trap,
    /// `test=reset`: on the first boot of a fresh log ring, print a line
    /// and reset warm; on the next, power off. The harness checks that
    /// 9boot reported saving the first boot's log, ending with that line.
    Reset,
}

impl<'a> Config<'a> {
    pub(crate) fn parse(text: &'a str) -> Self {
        Config { text }
    }

    /// The last value given for `key`, if any.
    pub(crate) fn get(&self, key: &str) -> Option<&'a str> {
        self.text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|l| l.split_once('='))
            .filter(|(k, _)| k.trim() == key)
            .map(|(_, v)| v.trim())
            .next_back()
    }

    /// True if `key` is set to `value`.
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))] // only amd64 has switches so far
    pub(crate) fn is(&self, key: &str, value: &str) -> bool {
        self.get(key) == Some(value)
    }

    pub(crate) fn test_mode(&self) -> Option<TestMode> {
        match self.get("test")? {
            "1" => Some(TestMode::Run),
            "trap" => Some(TestMode::Trap),
            "reset" => Some(TestMode::Reset),
            _ => None,
        }
    }
}
