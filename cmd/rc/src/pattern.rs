//! Words that may be patterns, and matching them, as Plan 9's `match.c`:
//! `*` matches any text, `?` one character, and `[…]` one character of a
//! class (`[a-z]`, `[~abc]` for the rest). Only characters written
//! unquoted are patterns; text from a variable never is.

use alloc::vec::Vec;

/// A word whose characters each may or may not be a pattern character.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pattern {
    pub text: Vec<u8>,
    /// For each byte, whether it is a pattern character; empty if none is.
    active: Vec<bool>,
}

impl Pattern {
    /// Text with no pattern characters.
    #[must_use]
    pub fn plain(text: Vec<u8>) -> Pattern {
        Pattern {
            text,
            active: Vec::new(),
        }
    }

    /// A word as written: unquoted, its `*`, `?` and `[` are patterns.
    #[must_use]
    pub fn written(text: &[u8], quoted: bool) -> Pattern {
        let active: Vec<bool> = if quoted {
            Vec::new()
        } else {
            text.iter().map(|c| b"*?[".contains(c)).collect()
        };
        let mut p = Pattern {
            text: text.to_vec(),
            active,
        };
        p.trim();
        p
    }

    fn trim(&mut self) {
        if !self.is_pattern() {
            self.active.clear();
        }
    }

    /// Whether any character is a pattern character.
    #[must_use]
    pub fn is_pattern(&self) -> bool {
        self.active.iter().any(|&a| a)
    }

    fn act(&self, i: usize) -> bool {
        self.active.get(i).copied().unwrap_or(false)
    }

    /// This followed by `other`.
    #[must_use]
    pub fn concat(&self, other: &Pattern) -> Pattern {
        let mut text = self.text.clone();
        text.extend_from_slice(&other.text);
        let mut active = Vec::new();
        if self.is_pattern() || other.is_pattern() {
            active = (0..self.text.len()).map(|i| self.act(i)).collect();
            active.extend((0..other.text.len()).map(|i| other.act(i)));
        }
        Pattern { text, active }
    }

    /// The parts between slashes, each with its own pattern characters.
    #[must_use]
    pub fn split_slashes(&self) -> Vec<Pattern> {
        let mut out = Vec::new();
        let mut start = 0;
        for i in 0..=self.text.len() {
            if i == self.text.len() || self.text[i] == b'/' {
                let mut p = Pattern {
                    text: self.text[start..i].to_vec(),
                    active: if self.active.is_empty() {
                        Vec::new()
                    } else {
                        self.active[start..i].to_vec()
                    },
                };
                p.trim();
                out.push(p);
                start = i + 1;
            }
        }
        out
    }

    /// Whether `s` matches the whole pattern.
    #[must_use]
    pub fn matches(&self, s: &[u8]) -> bool {
        self.match_from(s, 0)
    }

    fn match_from(&self, s: &[u8], mut pi: usize) -> bool {
        let mut si = 0;
        while pi < self.text.len() {
            let c = self.text[pi];
            if !self.act(pi) {
                if s.get(si) != Some(&c) {
                    return false;
                }
                si += 1;
                pi += 1;
                continue;
            }
            match c {
                b'*' => loop {
                    if self.match_from(&s[si..], pi + 1) {
                        return true;
                    }
                    if si >= s.len() {
                        return false;
                    }
                    si += rune(&s[si..]).1;
                },
                b'?' => {
                    if si >= s.len() {
                        return false;
                    }
                    si += rune(&s[si..]).1;
                    pi += 1;
                }
                _ => {
                    if si >= s.len() {
                        return false;
                    }
                    let (r, n) = rune(&s[si..]);
                    si += n;
                    match self.class(pi + 1, r) {
                        Some((true, next)) => pi = next,
                        _ => return false,
                    }
                }
            }
        }
        si == s.len()
    }

    /// Whether rune `r` is in the class that starts at `pi`, just after
    /// its `[`, and where the pattern goes on after it; `None` if it is
    /// not closed.
    fn class(&self, mut pi: usize, r: u32) -> Option<(bool, usize)> {
        let p = &self.text;
        let compl = p.get(pi) == Some(&b'~');
        if compl {
            pi += 1;
        }
        let mut hit = false;
        while *p.get(pi)? != b']' {
            let (lo, n) = rune(&p[pi..]);
            pi += n;
            let mut hi = lo;
            if p.get(pi) == Some(&b'-') {
                pi += 1;
                p.get(pi)?;
                let (h, n) = rune(&p[pi..]);
                pi += n;
                hi = h;
            }
            if lo <= r && r <= hi {
                hit = true;
            }
        }
        Some((hit != compl, pi + 1))
    }
}

/// The rune at the start of `b`, which is not empty, and its length; a
/// byte that does not start good UTF-8 is a rune of its own, above every
/// real one.
fn rune(b: &[u8]) -> (u32, usize) {
    let n = match b[0] {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    };
    match b.get(..n).map(core::str::from_utf8) {
        Some(Ok(s)) => (s.chars().next().map_or(0, u32::from), n),
        _ => (0x11_0000 + u32::from(b[0]), 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, s: &str) -> bool {
        Pattern::written(p.as_bytes(), false).matches(s.as_bytes())
    }

    #[test]
    fn matching() {
        assert!(m("*.c", "a.c") && m("*.c", ".c") && !m("*.c", "a.h"));
        assert!(m("a*b*c", "abc") && m("a*b*c", "axxbyyc") && !m("a*b*c", "acb"));
        assert!(m("?", "é") && !m("?", "") && !m("?", "ab") && m("??", "ab"));
        assert!(m("[a-c]x", "bx") && !m("[a-c]x", "dx"));
        assert!(m("[~a-c]", "d") && !m("[~a-c]", "a"));
        assert!(m("[αβ]", "β") && !m("[αβ]", "γ"));
        assert!(!m("[abc", "a"), "an unclosed class matches nothing");
        assert!(m("*", "") && m("*", "a/b"));
        assert!(m("", "") && !m("", "a"));
    }

    #[test]
    fn quoting() {
        let q = Pattern::written(b"*", true);
        assert!(!q.is_pattern() && q.matches(b"*") && !q.matches(b"x"));
        let p = Pattern::plain(b"a*".to_vec()).concat(&Pattern::written(b"*", false));
        assert!(p.matches(b"a*xyz") && !p.matches(b"abc"));
        let parts = Pattern::written(b"/a/*.c", false).split_slashes();
        assert_eq!(parts.len(), 3);
        assert!(!parts[0].is_pattern() && !parts[1].is_pattern() && parts[2].is_pattern());
    }
}
