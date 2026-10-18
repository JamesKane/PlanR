//! `plan9.ini`-style boot configuration: `key=value` lines, `#` comments.

#[derive(Debug, Clone, Copy)]
pub(crate) struct Config<'a> {
    text: &'a str,
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

    /// `test=1`: run the boot self-checks, report, and power off.
    pub(crate) fn test_mode(&self) -> bool {
        self.get("test") == Some("1")
    }
}
