//! `plan9.ini`-style boot configuration: `key=value` lines, `#` comments.

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

    pub(crate) fn test_mode(&self) -> Option<TestMode> {
        match self.get("test")? {
            "1" => Some(TestMode::Run),
            "trap" => Some(TestMode::Trap),
            _ => None,
        }
    }
}
