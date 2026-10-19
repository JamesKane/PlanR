//! Text utilities from Plan 9's libc: `cleanname`, `tokenize`,
//! `gettokens`, `getfields`, and quoting (`quote(2)`).

use alloc::format;
use alloc::vec::Vec;

use crate::print::{Quoted, RcQuoted};
use crate::utf;

/// `name` cleaned lexically, as Plan 9's `cleanname`: repeated slashes
/// become one, `.` elements go, and each `..` takes away the element
/// before it. A rooted name (`/`, or a device such as `#c`) keeps `..`
/// from going above its root; a relative one keeps its leading `..`s. An
/// empty result is `.`.
#[must_use]
pub fn cleanname(name: &[u8]) -> Vec<u8> {
    if name.is_empty() {
        return b".".to_vec();
    }
    let (root, rest): (&[u8], &[u8]) = if name[0] == b'#' {
        // `#` and the device's rune(s) up to a slash; `#/` is a device too.
        let (_, n) = utf::decode(&name[1..]);
        let end = name[1 + n..]
            .iter()
            .position(|&b| b == b'/')
            .map_or(name.len(), |i| 1 + n + i);
        (&name[..end], &name[end..])
    } else if name[0] == b'/' {
        (b"/", &name[1..])
    } else {
        (b"", name)
    };
    let rooted = !root.is_empty();
    let mut elems: Vec<&[u8]> = Vec::new();
    for e in rest.split(|&b| b == b'/') {
        match e {
            b"" | b"." => {}
            b".." if elems.last().is_some_and(|&l| l != b"..") => {
                elems.pop();
            }
            b".." if rooted => {}
            e => elems.push(e),
        }
    }
    let mut out = root.to_vec();
    if root.len() > 1 && !elems.is_empty() {
        out.push(b'/');
    }
    for (i, e) in elems.iter().enumerate() {
        if i > 0 {
            out.push(b'/');
        }
        out.extend_from_slice(e);
    }
    if out.is_empty() {
        out.push(b'.');
    }
    out
}

/// The separators [`tokenize`] splits at.
pub const QSEP: &str = " \t\r\n";

/// Splits `s` at runs of `sep` runes, outside single quotes; a token is
/// left as written, quotes and all (Plan 9's `gettokens`).
#[must_use]
pub fn gettokens<'a>(s: &'a [u8], sep: &str) -> Vec<&'a [u8]> {
    let mut out = Vec::new();
    let mut at = 0;
    loop {
        while let Some(n) = sep_at(s, at, sep) {
            at += n;
        }
        if at >= s.len() {
            return out;
        }
        let start = at;
        let mut quoting = false;
        while at < s.len() && (quoting || sep_at(s, at, sep).is_none()) {
            if s[at] == b'\'' {
                if !quoting {
                    quoting = true;
                } else if s.get(at + 1) == Some(&b'\'') {
                    at += 1;
                } else {
                    quoting = false;
                }
            }
            at += 1;
        }
        out.push(&s[start..at]);
    }
}

/// Whether a rune of `sep` is at `s[at]`, and its length if it is.
fn sep_at(s: &[u8], at: usize, sep: &str) -> Option<usize> {
    let &b = s.get(at)?;
    if b < utf::RUNE_SELF {
        return sep.as_bytes().contains(&b).then_some(1);
    }
    let (c, n) = utf::decode(&s[at..]);
    (c != utf::RUNE_ERROR && sep.contains(c)).then_some(n)
}

/// Splits `s` into words at blanks, as rc would: single quotes group, and
/// `''` inside them is one quote; the quotes are taken out (Plan 9's
/// `tokenize`).
#[must_use]
pub fn tokenize(s: &[u8]) -> Vec<Vec<u8>> {
    gettokens(s, QSEP).into_iter().map(unquote).collect()
}

/// `t` with the quoting of its first word taken out: quotes open and
/// close quoted parts, and `''` within one is a quote; from the first
/// blank or tab outside quotes, the rest is as it was (Plan 9's
/// `unquotestrdup`).
#[must_use]
pub fn unquote(t: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(t.len());
    let mut quoting = false;
    let mut i = 0;
    while i < t.len() && (quoting || (t[i] != b' ' && t[i] != b'\t')) {
        if t[i] != b'\'' {
            out.push(t[i]);
        } else if !quoting {
            quoting = true;
        } else if t.get(i + 1) == Some(&b'\'') {
            out.push(b'\'');
            i += 1;
        } else {
            quoting = false;
        }
        i += 1;
    }
    out.extend_from_slice(&t[i..]);
    out
}

/// Splits `s` at runes in `set` into at most `max` fields, the last
/// holding the rest (Plan 9's `getfields`). With `multi`, a run of
/// separators is one and no field is empty; without it, each separator
/// ends a field, so fields may be empty. `s` ends at a NUL, if it has one.
#[must_use]
pub fn getfields<'a>(s: &'a [u8], max: usize, multi: bool, set: &str) -> Vec<&'a [u8]> {
    let s = &s[..s.iter().position(|&b| b == 0).unwrap_or(s.len())];
    if max == 0 {
        return Vec::new();
    }
    // Plan 9's own loop, on offsets: `starts` are its `args`, and a
    // separator it overwrote with NUL ends the field before it.
    let mut starts = alloc::vec![0usize; max];
    let mut cut = alloc::vec![false; s.len()];
    let mut narg = usize::from(!multi);
    let mut intok = false;
    let mut at = 0;
    while at < s.len() {
        let (r, n) = utf::decode(&s[at..]);
        // A byte that is not UTF-8 is never a separator.
        if set.contains(r) && !(r == utf::RUNE_ERROR && n == 1) {
            if narg >= max {
                break;
            }
            cut[at] = true;
            intok = false;
            starts[narg] = at + n;
            if !multi {
                narg += 1;
            }
        } else {
            if !intok && multi {
                narg += 1;
            }
            intok = true;
        }
        at += n;
    }
    starts[..narg.min(max)]
        .iter()
        .map(|&st| {
            let end = (st..s.len()).find(|&i| cut[i]).unwrap_or(s.len());
            &s[st.min(s.len())..end]
        })
        .collect()
}

/// `s` as [`Quoted`] formats it: quoted only if it must be.
#[must_use]
pub fn quote(s: &[u8]) -> Vec<u8> {
    format!("{}", Quoted(s)).into_bytes()
}

/// `s` as [`RcQuoted`] formats it, for rc to read back as one word.
#[must_use]
pub fn rc_quote(s: &[u8]) -> Vec<u8> {
    format!("{}", RcQuoted(s)).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(s: &str) -> String {
        String::from_utf8(cleanname(s.as_bytes())).unwrap()
    }

    #[test]
    fn cleans_as_plan9() {
        for (name, want) in [
            ("", "."),
            (".", "."),
            ("/", "/"),
            ("//", "/"),
            ("a//b/./c/..", "a/b"),
            ("a/", "a"),
            ("./a", "a"),
            ("a/..", "."),
            ("../a/..", ".."),
            ("../../a", "../../a"),
            ("a/../../b", "../b"),
            ("/../a", "/a"),
            ("/a/b/../../..", "/"),
            ("#c", "#c"),
            ("#c/", "#c"),
            ("#c/../x", "#c/x"),
            ("#c//cons/.", "#c/cons"),
            ("#/", "#/"),
            ("#//boot", "#//boot"),
            // The device rune, then its spec up to a slash, as in `#S/sdC0`.
            ("#/../boot", "#/../boot"),
            ("#Ssdc0/../x", "#Ssdc0/x"),
        ] {
            assert_eq!(clean(name), want, "cleanname({name:?})");
        }
    }

    fn strs(v: &[&[u8]]) -> Vec<String> {
        v.iter().map(|b| String::from_utf8(b.to_vec()).unwrap()).collect()
    }

    #[test]
    fn tokens() {
        let t: Vec<String> = tokenize(b"  a 'b c' d''e 'it''s' ''\tlast\n")
            .into_iter()
            .map(|b| String::from_utf8(b).unwrap())
            .collect();
        assert_eq!(t, ["a", "b c", "de", "it's", "", "last"]);
        assert_eq!(strs(&gettokens(b"x:'a:b'::y", ":")), ["x", "'a:b'", "y"]);
        // A separator of more than one byte is passed over whole.
        assert_eq!(strs(&gettokens("aébéc".as_bytes(), "é")), ["a", "b", "c"]);
        // Plan 9's unquotestrdup: the first word only.
        assert_eq!(unquote(b"'a b' 'c d'"), b"a b 'c d'");
        assert_eq!(unquote(b"\tb' :"), b"\tb' :");
        assert_eq!(tokenize(b" \t "), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn fields() {
        let f = |s: &str, max, multi| strs(&getfields(s.as_bytes(), max, multi, ":"));
        assert_eq!(f("a::b:", 10, false), ["a", "", "b", ""]);
        assert_eq!(f("a::b:", 10, true), ["a", "b"]);
        assert_eq!(f("::a::b", 10, true), ["a", "b"]);
        assert_eq!(f("a:b:c:d", 2, false), ["a", "b:c:d"]);
        assert_eq!(f("a::b::c", 2, true), ["a", "b::c"]);
        assert_eq!(f("", 10, false), [""]);
        assert_eq!(f("", 10, true), Vec::<String>::new());
        assert_eq!(f("a:b", 0, false), Vec::<String>::new());
        assert_eq!(strs(&getfields("a·b·c".as_bytes(), 9, false, "·")), ["a", "b", "c"]);
        assert_eq!(strs(&getfields(b"a b\0c d", 9, false, " ")), ["a", "b"]);
    }

    #[test]
    fn quoting_round_trips() {
        for s in ["plain", "", "two words", "it's", "a$b", "é è"] {
            let q = rc_quote(s.as_bytes());
            assert_eq!(tokenize(&q), [s.as_bytes().to_vec()], "{s:?} quoted {q:?}");
        }
        assert_eq!(quote(b"a$b"), b"a$b");
    }
}
