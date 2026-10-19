//! Substitution, as `regsub.c` and `rregsub.c`: in the template, `&` is
//! the match, `\0` to `\9` a subexpression (one not matched, or not
//! asked for, is nothing), `\\` a backslash and `\` any other rune that
//! rune; a `\` at the end is nothing. As in C, the result holds at most
//! one less than `dlen`, the room for its NUL.

use alloc::vec::Vec;

use crate::Resub;

/// The extent in the text of match `i` (0 the whole), if any.
fn part(i: usize, mp: &[Resub]) -> Option<(usize, usize)> {
    let m = mp.get(i)?;
    Some((m.sp?, m.ep.or(m.sp)?))
}

/// The template `sp` with the matches `mp` of `text` substituted.
#[must_use]
pub fn regsub(sp: &[u8], text: &[u8], dlen: usize, mp: &[Resub]) -> Vec<u8> {
    subst(sp, text, dlen, mp, b'\\', b'&', b'0')
}

/// [`regsub`] in runes; `dlen` is in runes (C's is bytes, a rune four).
#[must_use]
pub fn rregsub(sp: &[char], text: &[char], dlen: usize, mp: &[Resub]) -> Vec<char> {
    subst(sp, text, dlen, mp, '\\', '&', '0')
}

fn subst<T: Copy + PartialEq + Into<u32>>(
    sp: &[T],
    text: &[T],
    dlen: usize,
    mp: &[Resub],
    bs: T,
    amp: T,
    zero: T,
) -> Vec<T> {
    let room = dlen.saturating_sub(1);
    let mut d = Vec::new();
    let put = |d: &mut Vec<T>, c: T| {
        if d.len() < room {
            d.push(c);
        }
    };
    let zero = zero.into();
    // The template ends at its end or a NUL.
    let end = sp.iter().position(|&c| c.into() == 0).unwrap_or(sp.len());
    let sp = &sp[..end];
    let mut i = 0;
    while i < sp.len() {
        let c = sp[i];
        if c == bs {
            i += 1;
            let Some(&n) = sp.get(i) else { break };
            match n.into().checked_sub(zero) {
                Some(digit @ 0..=9) => {
                    if let Some((a, b)) = part(digit as usize, mp) {
                        for &t in text.get(a..b).unwrap_or_default() {
                            put(&mut d, t);
                        }
                    }
                }
                _ => put(&mut d, n),
            }
        } else if c == amp {
            if let Some((a, b)) = part(0, mp) {
                for &t in text.get(a..b).unwrap_or_default() {
                    put(&mut d, t);
                }
            }
        } else {
            put(&mut d, c);
        }
        i += 1;
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{regcomp, regexec};

    #[test]
    fn substitutes() {
        let p = regcomp(b"(b+)(c)").unwrap();
        let text = b"abbcd";
        let mut mp = [Resub::default(); 3];
        assert_eq!(regexec(&p, text, &mut mp), 1);
        assert_eq!(regsub(b"[&] \\2\\1 \\\\ \\x \\5", text, 100, &mp), b"[bbc] cbb \\ x ");
        assert_eq!(regsub(b"&&&", text, 5, &mp), b"bbcb");
        assert_eq!(regsub(b"end\\", text, 100, &mp), b"end");
        let rt: Vec<char> = "abbcd".chars().collect();
        assert_eq!(rregsub(&['<', '&', '>'], &rt, 10, &mp), ['<', 'b', 'b', 'c', '>']);
    }
}
