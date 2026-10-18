//! The console font: 8×16 cells for printable ASCII, drawn for Plan R.
//!
//! `font.txt` holds the drawings: for each character from space to `~` in
//! order, a `:c` header line and 16 rows of 8 cells, `#` for ink and `.`
//! for none. Edit it directly; it is parsed at compile time, and a
//! malformed or misordered glyph fails the build.

/// Cell width in pixels.
pub const WIDTH: usize = 8;
/// Cell height in pixels.
pub const HEIGHT: usize = 16;

const FIRST: u8 = b' ';
const COUNT: usize = 95;

/// One glyph: a byte per row, the most significant bit leftmost.
pub type Glyph = [u8; HEIGHT];

static GLYPHS: [Glyph; COUNT] = parse(include_str!("font.txt"));

/// Drawn for bytes outside printable ASCII: an empty box.
const REPLACEMENT: Glyph = [
    0, 0, 0, 0x7c, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x7c, 0, 0, 0, 0,
];

/// The glyph for `c`.
#[must_use]
pub fn glyph(c: u8) -> &'static Glyph {
    match c.checked_sub(FIRST) {
        Some(i) if usize::from(i) < COUNT => &GLYPHS[i as usize],
        _ => &REPLACEMENT,
    }
}

const fn parse(src: &str) -> [Glyph; COUNT] {
    let b = src.as_bytes();
    let mut out = [[0; HEIGHT]; COUNT];
    let mut i = 0;
    let mut g = 0;
    while g < COUNT {
        assert!(b[i] == b':', "font.txt: expected a `:c` glyph header");
        #[allow(clippy::cast_possible_truncation)] // g < 95
        let want = FIRST + g as u8;
        assert!(b[i + 1] == want, "font.txt: glyph out of order");
        assert!(b[i + 2] == b'\n', "font.txt: header must be `:c` alone");
        i += 3;
        let mut row = 0;
        while row < HEIGHT {
            let mut bits = 0u8;
            let mut col = 0;
            while col < WIDTH {
                match b[i + col] {
                    b'#' => bits |= 0x80 >> col,
                    b'.' => {}
                    _ => panic!("font.txt: rows are 8 of `#` or `.`"),
                }
                col += 1;
            }
            assert!(b[i + WIDTH] == b'\n', "font.txt: rows are 8 of `#` or `.`");
            i += WIDTH + 1;
            out[g][row] = bits;
            row += 1;
        }
        g += 1;
    }
    assert!(i == b.len(), "font.txt: text after the last glyph");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_have_ink_and_space_has_none() {
        assert!(glyph(b' ').iter().all(|&r| r == 0));
        for c in (b'!'..=b'~').filter(u8::is_ascii_graphic) {
            assert!(glyph(c).iter().any(|&r| r != 0), "{:?} is blank", c as char);
        }
    }

    #[test]
    fn rightmost_column_is_spacing() {
        // Every glyph but `_` leaves column 7 empty, so letters never touch.
        for c in (b' '..=b'~').filter(|&c| c != b'_') {
            assert!(glyph(c).iter().all(|&r| r & 1 == 0), "{:?} touches column 7", c as char);
        }
    }

    #[test]
    fn known_rows() {
        let a = glyph(b'A');
        assert_eq!(a[3], 0b0001_0000);
        assert_eq!(a[8], 0b1111_1110);
        assert_eq!(glyph(0x7f), &REPLACEMENT);
        assert_eq!(glyph(0xc3), &REPLACEMENT);
    }

    #[test]
    fn glyphs_are_distinct() {
        for a in b' '..=b'~' {
            for b in a + 1..=b'~' {
                assert_ne!(glyph(a), glyph(b), "{:?} and {:?}", a as char, b as char);
            }
        }
    }
}
