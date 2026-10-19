//! UTF-8 and runes, as Plan 9's `utf(6)` and `rune(2)` have them, with
//! Rust's `char` as the rune. Bytes that are not UTF-8 (a bad or overlong
//! sequence, a surrogate, a sequence cut short) decode one byte at a time
//! as [`RUNE_ERROR`], so text of any bytes can be read.

/// What a byte that is not UTF-8 reads as.
pub const RUNE_ERROR: char = '\u{fffd}';
/// The longest encoding, in bytes.
pub const UTF_MAX: usize = 4;
/// Bytes below this are runes of their own.
pub const RUNE_SELF: u8 = 0x80;

/// The first rune in `s` and its length in bytes; `(RUNE_ERROR, 1)` if `s`
/// does not start with valid UTF-8, `(RUNE_ERROR, 0)` if it is empty.
#[must_use]
pub fn decode(s: &[u8]) -> (char, usize) {
    let Some(&b0) = s.first() else {
        return (RUNE_ERROR, 0);
    };
    if b0 < RUNE_SELF {
        return (char::from(b0), 1);
    }
    let n = match b0 {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return (RUNE_ERROR, 1),
    };
    let Some(seq) = s.get(..n) else {
        return (RUNE_ERROR, 1);
    };
    match core::str::from_utf8(seq) {
        Ok(t) => (t.chars().next().unwrap_or(RUNE_ERROR), n),
        Err(_) => (RUNE_ERROR, 1),
    }
}

/// Whether `c` is white space, as Plan 9's `isspacerune` (Unicode 6.2's
/// list) says.
#[must_use]
pub fn is_space_rune(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}'
            | ' '
            | '\u{85}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{180e}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'..='\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// Whether `s` is as long as its first byte says a rune is, so that
/// [`decode`] need not wait for more (Plan 9's `fullrune`): one byte below
/// 0x80, two below 0xe0, three below 0xf0, else four, whether or not the
/// bytes make a rune.
#[must_use]
pub fn full_rune(s: &[u8]) -> bool {
    match s.first() {
        None => false,
        Some(&c) if c < 0x80 => true,
        Some(&c) if c < 0xe0 => s.len() >= 2,
        Some(&c) if c < 0xf0 => s.len() >= 3,
        Some(_) => s.len() >= 4,
    }
}

/// The bytes `c` takes.
#[must_use]
pub fn rune_len(c: char) -> usize {
    c.len_utf8()
}

/// The runes in `s`.
#[must_use]
pub fn utf_len(s: &[u8]) -> usize {
    runes(s).count()
}

/// The runes of `s`, decoded as [`decode`] does.
#[must_use]
pub fn runes(s: &[u8]) -> Runes<'_> {
    Runes { s }
}

/// The byte offset of the first `c` in `s`.
#[must_use]
pub fn utf_rune(s: &[u8], c: char) -> Option<usize> {
    rune_offsets(s).find(|&(_, r)| r == c).map(|(i, _)| i)
}

/// The byte offset of the last `c` in `s`.
#[must_use]
pub fn utf_rrune(s: &[u8], c: char) -> Option<usize> {
    rune_offsets(s).filter(|&(_, r)| r == c).last().map(|(i, _)| i)
}

/// Each rune of `s` with its byte offset.
pub fn rune_offsets(s: &[u8]) -> impl Iterator<Item = (usize, char)> + '_ {
    let mut at = 0;
    core::iter::from_fn(move || {
        let (c, n) = decode(&s[at..]);
        if n == 0 {
            return None;
        }
        let i = at;
        at += n;
        Some((i, c))
    })
}

/// An iterator over runes; see [`runes`].
#[derive(Debug, Clone)]
pub struct Runes<'a> {
    s: &'a [u8],
}

impl Runes<'_> {
    /// What is not yet decoded.
    #[must_use]
    pub fn rest(&self) -> &[u8] {
        self.s
    }
}

impl Iterator for Runes<'_> {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        let (c, n) = decode(self.s);
        if n == 0 {
            return None;
        }
        self.s = &self.s[n..];
        Some(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_valid() {
        assert_eq!(decode(b"a"), ('a', 1));
        assert_eq!(decode("é".as_bytes()), ('é', 2));
        assert_eq!(decode("世".as_bytes()), ('世', 3));
        assert_eq!(decode("𝄞x".as_bytes()), ('𝄞', 4));
        assert_eq!(decode(b""), (RUNE_ERROR, 0));
    }

    #[test]
    fn bad_bytes_are_errors_one_at_a_time() {
        // A lone continuation byte, an overlong '/', a surrogate, beyond
        // U+10FFFF, and a sequence cut short.
        for bad in [
            &b"\x80"[..],
            b"\xc0\xaf",
            b"\xed\xa0\x80",
            b"\xf4\x90\x80\x80",
            b"\xe4\xb8",
        ] {
            assert_eq!(decode(bad), (RUNE_ERROR, 1), "{bad:?}");
        }
        let all: Vec<char> = runes(b"a\xffb\xe4\xb8").collect();
        assert_eq!(all, ['a', RUNE_ERROR, 'b', RUNE_ERROR, RUNE_ERROR]);
    }

    #[test]
    fn full_runes() {
        assert!(!full_rune(b""));
        assert!(full_rune(b"a"));
        assert!(!full_rune(b"\xe4\xb8"));
        assert!(full_rune("世".as_bytes()));
        assert!(!full_rune(b"\xe4a"), "Plan 9's: by the first byte alone");
        assert!(full_rune(b"\xe4ab"));
        assert!(!full_rune(b"\xff"));
        assert!(full_rune(b"\xffabc"));
        assert!(!full_rune(b"\x80"));
    }

    #[test]
    fn searching() {
        let s = "aé世é".as_bytes();
        assert_eq!(utf_len(s), 4);
        assert_eq!(utf_rune(s, 'é'), Some(1));
        assert_eq!(utf_rrune(s, 'é'), Some(6));
        assert_eq!(utf_rune(s, 'z'), None);
        assert_eq!(rune_len('世'), 3);
    }
}
