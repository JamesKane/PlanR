//! Numbers in text, as Plan 9's libc reads and prints them: `strtol`,
//! `strtoul`, `atol` and `strtod` (each reading the longest prefix that is
//! a number, and saying how much it read), and `print`'s `%e`, `%f` and
//! `%g` for a floating-point value ([`format_float`], [`printf_float`];
//! `seq -f` takes the format from its user). Each is a port of Plan 9's
//! code (`libc/port/strtol.c`, `strtoul.c`, `atol.c`, `strtod.c`,
//! `libc/fmt/fltfmt.c`), so its corners are Plan 9's: `%g` keeps
//! `prec + 1` digits, prints `.5` with no leading zero, rounds half up in
//! decimal, and is not padded with zeros. Integers are 64 bits wide,
//! where Plan 9's `long` is 32.

use alloc::string::String;
use alloc::vec::Vec;

/// `strtol`'s and `strtoul`'s blanks.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0c | b'\r' | 0x0b)
}

/// The value of digit `c` in bases up to 36; 36 or more if it is none.
fn digit(c: u8) -> u32 {
    match c {
        b'0'..=b'9' => u32::from(c - b'0'),
        b'a'..=b'z' => u32::from(c - b'a') + 10,
        b'A'..=b'Z' => u32::from(c - b'A') + 10,
        _ => 36,
    }
}

/// The digits at `s[p..]` in `base`: their value (and whether it
/// overflowed), and where they end; `None` if there are none.
fn digits(s: &[u8], mut p: usize, base: u32) -> Option<(u64, bool, usize)> {
    let start = p;
    let (mut value, mut ovfl) = (0u64, false);
    while let Some(&byte) = s.get(p) {
        let v = digit(byte);
        if v >= base {
            break;
        }
        match value
            .checked_mul(u64::from(base))
            .and_then(|m| m.checked_add(u64::from(v)))
        {
            Some(m) => value = m,
            None => ovfl = true,
        }
        p += 1;
    }
    (p > start).then_some((value, ovfl, p))
}

/// The blanks and sign `strtol` and `strtoul` start with: whether it is
/// negative, and where the number starts.
fn sign(s: &[u8]) -> (bool, usize) {
    let mut p = 0;
    while s.get(p).is_some_and(|&b| is_space(b)) {
        p += 1;
    }
    let neg = s.get(p) == Some(&b'-');
    if matches!(s.get(p), Some(b'-' | b'+')) {
        p += 1;
    }
    (neg, p)
}

fn is_x(c: Option<&u8>) -> bool {
    matches!(c, Some(b'x' | b'X'))
}

/// `strtol`: the integer at the start of `s`, in `base` (2 to 36; 0 for
/// `0x` hex, a leading `0` octal, else decimal), and the bytes it took:
/// none if there are no digits, so `0x` alone is `(0, 0)`. Out of range,
/// it is `i64::MAX` or `i64::MIN`.
#[must_use]
pub fn strtol(s: &[u8], base: u32) -> (i64, usize) {
    let (neg, mut p) = sign(s);
    let mut base = base;
    if base == 0 {
        base = 10;
        if s.get(p) == Some(&b'0') {
            base = 8;
            if is_x(s.get(p + 1)) {
                p += 2;
                base = 16;
            }
        }
    } else if base == 16 && s.get(p) == Some(&b'0') && is_x(s.get(p + 1)) {
        p += 2;
    } else if base > 36 {
        return (0, 0);
    }
    let Some((n, ovfl, end)) = digits(s, p, base) else {
        return (0, 0);
    };
    #[allow(clippy::cast_possible_wrap)] // as C's, the magnitude wraps
    let v = match (ovfl, neg) {
        (true, true) => i64::MIN,
        (true, false) => i64::MAX,
        (false, true) => (n as i64).wrapping_neg(),
        (false, false) => n as i64,
    };
    (v, end)
}

/// `strtoul`: as [`strtol`], unsigned; a `-` negates, wrapping. Unlike
/// `strtol`'s, its `0x` is taken only before a hex digit, so `0x` alone
/// is the 0.
#[must_use]
pub fn strtoul(s: &[u8], base: u32) -> (u64, usize) {
    let (neg, mut p) = sign(s);
    let mut base = base;
    if base == 0 {
        base = if s.get(p) != Some(&b'0') {
            10
        } else if is_x(s.get(p + 1)) {
            16
        } else {
            8
        };
    }
    if !(2..=36).contains(&base) {
        return (0, 0);
    }
    if base == 16 && s.get(p) == Some(&b'0') && is_x(s.get(p + 1)) && s.get(p + 2).is_some_and(u8::is_ascii_hexdigit) {
        p += 2;
    }
    let Some((n, ovfl, end)) = digits(s, p, base) else {
        return (0, 0);
    };
    let v = if ovfl {
        u64::MAX
    } else if neg {
        n.wrapping_neg()
    } else {
        n
    };
    (v, end)
}

/// `atol` (and `atoi`): the integer at the start of `s`, after blanks and
/// tabs and a sign (and blanks and tabs after it): hex after `0x`, octal
/// after a `0`, else decimal; 0 if there is none.
#[must_use]
pub fn atol(s: &[u8]) -> i64 {
    let blank = |b: Option<&u8>| matches!(b, Some(b' ' | b'\t'));
    let mut p = 0;
    while blank(s.get(p)) {
        p += 1;
    }
    let neg = s.get(p) == Some(&b'-');
    if matches!(s.get(p), Some(b'-' | b'+')) {
        p += 1;
        while blank(s.get(p)) {
            p += 1;
        }
    }
    let base = if s.get(p) == Some(&b'0') && s.get(p + 1).is_some() {
        if is_x(s.get(p + 1)) {
            p += 2;
            16
        } else {
            8
        }
    } else {
        10
    };
    let mut n: i64 = 0;
    while let Some(&c) = s.get(p) {
        // Hex digits are only a to f; digit() would take g to z.
        let v = if base == 16 && !c.is_ascii_hexdigit() {
            36
        } else {
            digit(c)
        };
        if v >= base {
            break;
        }
        n = n.wrapping_mul(i64::from(base)).wrapping_add(i64::from(v));
        p += 1;
    }
    if neg { n.wrapping_neg() } else { n }
}

/// `strtod`: the floating-point number at the start of `s` (blanks, a
/// sign, digits with perhaps a point, perhaps an exponent; or `nan`,
/// `inf`, `infinity` in any case, the first unsigned) and the bytes it
/// took; `(0.0, 0)` if there is none. Zero is never negative.
#[must_use]
#[allow(clippy::match_same_arms)] // a row of Plan 9's table each
pub fn strtod(s: &[u8]) -> (f64, usize) {
    // Plan 9's scanner's states: before anything but blanks, after a
    // sign, in digits, after a point with no digits before it, in digits
    // after a point, after an `e`, after its sign, in its digits.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum St {
        S0,
        S1,
        S2,
        S3,
        S4,
        S5,
        S6,
        S7,
    }
    use St::{S0, S1, S2, S3, S4, S5, S6, S7};
    let mut state = S0;
    let mut neg = false;
    let mut start = 0;
    let mut p = 0;
    while let Some(&c) = s.get(p) {
        state = match (c, state) {
            (b'0'..=b'9', S0 | S1 | S2) => S2,
            (b'0'..=b'9', S3 | S4) => S4,
            (b'0'..=b'9', _) => S7,
            (b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | b' ', S0) => {
                start = p + 1;
                S0
            }
            (b'-' | b'+', S0) => {
                neg = c == b'-';
                S1
            }
            (b'-' | b'+', S5) => S6,
            (b'.', S0 | S1) => S3,
            (b'.', S2) => S4,
            (b'e' | b'E', S2 | S4) => S5,
            _ => break,
        };
        p += 1;
    }
    let rest = &s[p..];
    let is = |word: &str| rest.len() >= word.len() && rest[..word.len()].eq_ignore_ascii_case(word.as_bytes());
    let inf = if neg { f64::NEG_INFINITY } else { f64::INFINITY };
    let end = match state {
        S0 if is("nan") => return (f64::NAN, p + 3),
        S0 | S1 if is("infinity") => return (inf, p + 8),
        S0 | S1 if is("inf") => return (inf, p + 3),
        S0 | S1 | S3 => return (0.0, 0),
        S6 => p - 2,
        S5 => p - 1,
        _ => p,
    };
    // The digits are all there; Rust's parser rounds them as Plan 9's
    // does, correctly. It takes no `+`, nor a point with nothing after.
    let text = core::str::from_utf8(&s[start..end]).unwrap_or("0");
    let text = text.strip_prefix('+').unwrap_or(text);
    let text = text.replace(".e", "e").replace(".E", "E");
    let text = text.strip_suffix('.').unwrap_or(&text);
    let v: f64 = text.parse().unwrap_or(0.0);
    (if v == 0.0 { 0.0 } else { v }, end)
}

/// `atof`: the number at the start of `s`, or 0.
#[must_use]
pub fn atof(s: &[u8]) -> f64 {
    strtod(s).0
}

/// A conversion of `print` for a floating-point value:
/// `%[-+ #][width][.prec]{e,E,f,g,G}`. A `0` flag means nothing to it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // print's flags
pub struct FloatSpec {
    /// `-`: pad on the right.
    pub left: bool,
    /// `+`: a sign even when positive.
    pub plus: bool,
    /// ` `: a blank where a plus sign would be.
    pub space: bool,
    /// `#`: keep trailing zeros, and a point with no digits after it.
    pub sharp: bool,
    pub width: usize,
    pub prec: Option<usize>,
    /// `e`, `E`, `f`, `g` or `G`.
    pub verb: u8,
}

const FDIGIT: i32 = 30;
const FDEFLT: i32 = 6;
const NSIGNIF: usize = 17;
#[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)] // 17
const NSIG: i32 = NSIGNIF as i32;

/// Plan 9's `pow10`: from a table up to 1e159, products beyond.
fn pow10(n: i32) -> f64 {
    const TAB: [f64; 160] = [
        1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18, 1e19,
        1e20, 1e21, 1e22, 1e23, 1e24, 1e25, 1e26, 1e27, 1e28, 1e29, 1e30, 1e31, 1e32, 1e33, 1e34, 1e35, 1e36, 1e37,
        1e38, 1e39, 1e40, 1e41, 1e42, 1e43, 1e44, 1e45, 1e46, 1e47, 1e48, 1e49, 1e50, 1e51, 1e52, 1e53, 1e54, 1e55,
        1e56, 1e57, 1e58, 1e59, 1e60, 1e61, 1e62, 1e63, 1e64, 1e65, 1e66, 1e67, 1e68, 1e69, 1e70, 1e71, 1e72, 1e73,
        1e74, 1e75, 1e76, 1e77, 1e78, 1e79, 1e80, 1e81, 1e82, 1e83, 1e84, 1e85, 1e86, 1e87, 1e88, 1e89, 1e90, 1e91,
        1e92, 1e93, 1e94, 1e95, 1e96, 1e97, 1e98, 1e99, 1e100, 1e101, 1e102, 1e103, 1e104, 1e105, 1e106, 1e107, 1e108,
        1e109, 1e110, 1e111, 1e112, 1e113, 1e114, 1e115, 1e116, 1e117, 1e118, 1e119, 1e120, 1e121, 1e122, 1e123, 1e124,
        1e125, 1e126, 1e127, 1e128, 1e129, 1e130, 1e131, 1e132, 1e133, 1e134, 1e135, 1e136, 1e137, 1e138, 1e139, 1e140,
        1e141, 1e142, 1e143, 1e144, 1e145, 1e146, 1e147, 1e148, 1e149, 1e150, 1e151, 1e152, 1e153, 1e154, 1e155, 1e156,
        1e157, 1e158, 1e159,
    ];
    let a = n.abs();
    let p = match usize::try_from(a).ok().and_then(|i| TAB.get(i)) {
        Some(&t) => t,
        None => pow10(a / 2) * pow10(a - a / 2),
    };
    if n < 0 { 1.0 / p } else { p }
}

/// `frexp`'s exponent: `f` is a fraction in `[0.5, 1)` times 2 to it.
fn frexp_exp(f: f64) -> i32 {
    #[allow(clippy::cast_possible_truncation)] // 11 bits
    let biased = ((f.abs().to_bits() >> 52) & 0x7ff) as i32;
    if biased == 0 {
        // Subnormal: scale it up first.
        return frexp_exp(f * 4_503_599_627_370_496.0) - 52;
    }
    biased - 1022
}

/// Adds `v` to the decimal digit `a[n]`, carrying; true if it carried
/// out of `a[0]` (which is then `1`). Nothing for `n` out of range.
fn xadd(a: &mut [u8; NSIGNIF], n: i32, v: u8) -> bool {
    let Some(n) = usize::try_from(n).ok().filter(|&n| n < NSIGNIF) else {
        return false;
    };
    let mut v = v;
    for b in (0..=n).rev() {
        let c = a[b] + v;
        if c <= b'9' {
            a[b] = c;
            return false;
        }
        a[b] = b'0';
        v = 1;
    }
    a[0] = b'1';
    true
}

/// Takes 1 from the decimal digit `a[n]`, borrowing; true if it
/// borrowed out of `a[0]` (which is then `9`).
fn xsub(a: &mut [u8; NSIGNIF], n: usize) -> bool {
    for b in (0..=n).rev() {
        if a[b] > b'0' {
            a[b] -= 1;
            return false;
        }
        a[b] = b'9';
    }
    a[0] = b'9';
    true
}

/// The value of the digits `s1` times 10 to the `exp`, as `strtod`
/// reads it.
fn digits_value(s1: &[u8; NSIGNIF], exp: i32) -> f64 {
    let t = alloc::format!("{}e{exp}", core::str::from_utf8(s1).unwrap_or("0"));
    t.parse().unwrap_or(0.0)
}

/// Plan 9's `xdtoa`: `f` as `spec` says, before padding.
#[allow(
    clippy::too_many_lines,
    clippy::many_single_char_names, // Plan 9's
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::approx_constant // Plan 9's own log10(2), which sets the digits
)]
fn xdtoa(spec: &FloatSpec, f: f64) -> Vec<u8> {
    let prec = spec
        .prec
        .map_or(FDEFLT, |p| i32::try_from(p).unwrap_or(FDIGIT))
        .min(FDIGIT);
    if f.is_nan() {
        return b"NaN".to_vec();
    }
    if f.is_infinite() {
        return if f > 0.0 { b"+Inf".to_vec() } else { b"-Inf".to_vec() };
    }
    let sign = f < 0.0;
    let f = if sign { -f } else { f };
    let ucase = spec.verb.is_ascii_uppercase();
    let mut chr = spec.verb.to_ascii_lowercase();

    // The decimal exponent, and the digits by arithmetic.
    let mut e: i32 = 0;
    let mut g = f;
    if g != 0.0 {
        e = (f64::from(frexp_exp(f)) * 0.301_029_995_664) as i32;
        let (d, h) = if (-150..=150).contains(&e) {
            (0, f)
        } else {
            (e / 2, f * pow10(-(e / 2)))
        };
        g = h * pow10(d - e);
        while g < 1.0 {
            e -= 1;
            g = h * pow10(d - e);
        }
        while g >= 10.0 {
            e += 1;
            g = h * pow10(d - e);
        }
    }
    let mut s1 = [b'0'; NSIGNIF];
    for c in &mut s1 {
        let d = g as u8;
        *c = d + b'0';
        g = (g - f64::from(d)) * 10.0;
    }
    let exp_of = |e: i32| e - NSIG + 1;

    'found: {
        // Try decimal rounding to eliminate 9s.
        let c2 = prec + 1 + if chr == b'f' { e } else { 0 };
        if c2 >= NSIG - 2 {
            let (save, d) = (s1, e);
            s1[NSIGNIF - 2] = b'0';
            s1[NSIGNIF - 1] = b'0';
            if digits_value(&s1, exp_of(e)) == f {
                break 'found;
            }
            if xadd(&mut s1, NSIG - 3, 1) {
                e += 1;
            }
            if digits_value(&s1, exp_of(e)) == f {
                break 'found;
            }
            s1 = save;
            e = d;
        }
        // Convert back, so that s1 is exact.
        loop {
            let g = digits_value(&s1, exp_of(e));
            if f > g {
                if xadd(&mut s1, NSIG - 1, 1) {
                    e -= 1;
                }
            } else if f < g {
                if xsub(&mut s1, NSIGNIF - 1) {
                    e += 1;
                }
            } else {
                break;
            }
        }
    }

    let mut s2 = Vec::new();
    if sign {
        s2.push(b'-');
    } else if spec.plus {
        s2.push(b'+');
    } else if spec.space {
        s2.push(b' ');
    }
    // c1 leading zeros, c2 digits from s1, c3 trailing zeros, c4 digits
    // after the point.
    let (mut c1, mut c2, mut c3, mut c4) = (0, prec + 1, 0, prec);
    match chr {
        b'g' => {
            if xadd(&mut s1, c2, 5) {
                e += 1;
            }
            if e >= -5 && e <= prec {
                c1 = -e - 1;
                c4 = prec - e;
                chr = b'h'; // f style
            }
        }
        b'f' => {
            if xadd(&mut s1, c2 + e, 5) {
                e += 1;
            }
            c1 = -e;
            if c1 > prec {
                c1 = c2;
            }
            c2 += e;
        }
        _ => {
            if xadd(&mut s1, c2, 5) {
                e += 1;
            }
        }
    }
    c1 = c1.max(0);
    c2 = c2.max(0);
    if c2 > NSIG {
        c3 = c2 - NSIG;
        c2 = NSIG;
    }
    let mut i = 0;
    while c1 > 0 {
        if c1 + c2 + c3 == c4 {
            s2.push(b'.');
        }
        s2.push(b'0');
        c1 -= 1;
    }
    while c2 > 0 {
        if c2 + c3 == c4 {
            s2.push(b'.');
        }
        s2.push(s1[i]);
        i += 1;
        c2 -= 1;
    }
    while c3 > 0 {
        if c3 == c4 {
            s2.push(b'.');
        }
        s2.push(b'0');
        c3 -= 1;
    }
    if spec.sharp {
        if c4 == 0 {
            s2.push(b'.');
        }
    } else if chr == b'g' || chr == b'h' {
        // Strip trailing zeros after a point, and the point if it is left
        // last.
        if let Some(n) = s2.iter().rposition(|&c| c != b'0')
            && let Some(dot) = s2[..=n].iter().rposition(|&c| c == b'.')
        {
            s2.truncate(if dot == n { n } else { n + 1 });
        }
    }
    if chr == b'e' || chr == b'g' {
        s2.push(if ucase { b'E' } else { b'e' });
        let mut x = e;
        if x < 0 {
            s2.push(b'-');
            x = -x;
        } else {
            s2.push(b'+');
        }
        if x >= 100 {
            s2.push(b'0' + (x / 100) as u8);
            x %= 100;
        }
        s2.push(b'0' + (x / 10) as u8);
        s2.push(b'0' + (x % 10) as u8);
    }
    s2
}

/// `v` formatted as `spec` says, as Plan 9's `print` does it.
#[must_use]
pub fn format_float(spec: &FloatSpec, v: f64) -> String {
    let body = String::from_utf8_lossy(&xdtoa(spec, v)).into_owned();
    let pad = spec.width.saturating_sub(body.len());
    let mut out = String::with_capacity(body.len() + pad);
    if spec.left {
        out.push_str(&body);
        out.extend(core::iter::repeat_n(' ', pad));
    } else {
        out.extend(core::iter::repeat_n(' ', pad));
        out.push_str(&body);
    }
    out
}

/// Formats `v` by `fmt`, a `print` format whose conversions are all
/// floating-point (`%e %E %f %g %G`, each given `v`) or `%%`. Any other
/// conversion is copied as it is.
#[must_use]
pub fn printf_float(fmt: &[u8], v: f64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            out.push(fmt[i]);
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        let mut spec = FloatSpec::default();
        while let Some(&c) = fmt.get(i) {
            match c {
                b'-' => spec.left = true,
                b'+' => spec.plus = true,
                b' ' => spec.space = true,
                b'#' => spec.sharp = true,
                b'0' => {}
                _ => break,
            }
            i += 1;
        }
        let number = |i: &mut usize| {
            let mut n = 0usize;
            while let Some(d) = fmt.get(*i).and_then(|&c| char::from(c).to_digit(10)) {
                n = n.saturating_mul(10).saturating_add(d as usize);
                *i += 1;
            }
            n
        };
        spec.width = number(&mut i);
        if fmt.get(i) == Some(&b'.') {
            i += 1;
            spec.prec = Some(number(&mut i));
        }
        match fmt.get(i) {
            Some(b'%') if i == start + 1 => out.push(b'%'),
            Some(&c @ (b'e' | b'E' | b'f' | b'g' | b'G')) => {
                spec.verb = c;
                out.extend_from_slice(format_float(&spec, v).as_bytes());
            }
            Some(_) => out.extend_from_slice(&fmt[start..=i]),
            None => {
                out.extend_from_slice(&fmt[start..]);
                break;
            }
        }
        i += 1;
    }
    out
}

/// `v` as `%g` gives it.
#[must_use]
pub fn g(v: f64) -> String {
    let spec = FloatSpec {
        verb: b'g',
        ..FloatSpec::default()
    };
    format_float(&spec, v)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The expectations are what Plan 9's own code gives, built on the
    // host.

    #[test]
    fn integers() {
        assert_eq!(strtol(b"  42xyz", 0), (42, 4));
        assert_eq!(strtol(b"-0x1F", 0), (-31, 5));
        assert_eq!(strtol(b"017", 0), (15, 3));
        assert_eq!(strtol(b"017", 10), (17, 3));
        assert_eq!(strtol(b"0x", 0), (0, 0));
        assert_eq!(strtol(b"0xg", 16), (0, 0));
        assert_eq!(strtol(b"abc", 0), (0, 0));
        assert_eq!(strtol(b"+7", 10), (7, 2));
        assert_eq!(strtol(b"99999999999999999999", 10).0, i64::MAX);
        assert_eq!(strtol(b"777", 8), (511, 3));
        assert_eq!(strtoul(b"-1", 10), (u64::MAX, 2));
        assert_eq!(strtoul(b"0x", 0), (0, 1));
        assert_eq!(strtoul(b"0x1f", 0), (31, 4));
        assert_eq!(strtoul(b"089", 0), (0, 1));
        assert_eq!(atol(b"12 monkeys"), 12);
        assert_eq!(atol(b" - 0x1f"), -31);
        assert_eq!(atol(b"\n5"), 0);
        assert_eq!(atol(b"010"), 8);
        assert_eq!(atol(b"0"), 0);
        assert_eq!(atol(b"0xFg"), 15);
    }

    #[test]
    fn floats_in() {
        assert_eq!(strtod(b"1.5e3x"), (1500.0, 5));
        assert_eq!(strtod(b"-.5"), (-0.5, 3));
        assert_eq!(strtod(b"7."), (7.0, 2));
        assert_eq!(strtod(b"+2e"), (2.0, 2), "an e without digits is not taken");
        assert_eq!(strtod(b"3e+"), (3.0, 1));
        assert_eq!(strtod(b"."), (0.0, 0));
        assert_eq!(strtod(b" -inf"), (f64::NEG_INFINITY, 5));
        assert_eq!(strtod(b"Infinity!"), (f64::INFINITY, 8));
        assert!(strtod(b"NaN").0.is_nan());
        assert_eq!(strtod(b"-nan"), (0.0, 0));
        assert!(strtod(b"-0").0.is_sign_positive());
        assert_eq!(atof(b"x").to_bits(), 0);
    }

    fn pf(fmt: &str, v: f64) -> String {
        String::from_utf8(printf_float(fmt.as_bytes(), v)).unwrap()
    }

    #[test]
    fn floats_out() {
        assert_eq!(g(1.0), "1");
        assert_eq!(g(0.1), ".1");
        assert_eq!(g(0.300_000_000_000_000_04), ".3");
        assert_eq!(g(1_000_000.0), "1000000");
        assert_eq!(g(10_000_000.0), "1e+07");
        assert_eq!(g(0.000_01), ".00001");
        assert_eq!(g(0.000_001), "1e-06");
        assert_eq!(g(-2.5), "-2.5");
        assert_eq!(g(0.0), "0");
        assert_eq!(g(-0.0), "0");
        assert_eq!(g(123_456_789.0), "1.234568e+08");
        assert_eq!(g(1e300), "1e+300");
        assert_eq!(g(f64::NAN), "NaN");
        assert_eq!(g(f64::NEG_INFINITY), "-Inf");
        assert_eq!(pf("%.1f %.2f", 0.125), "0.1 0.13");
        assert_eq!(pf("%.1f", 1.25), "1.3");
        assert_eq!(pf("%5.2f|%-6g|%e", 1.234_56), " 1.23|1.23456|1.234560e+00");
        assert_eq!(pf("%05.1f %%", -2.25), " -2.3 %");
        assert_eq!(pf("%+.3e", 12345.0), "+1.235e+04");
        assert_eq!(pf("%#g", 2.0), "2.000000");
        assert_eq!(pf("%#.0f", 2.0), "2.");
        assert_eq!(pf("%G", 1e-10), "1E-10");
        assert_eq!(pf("%f", 1e22), "10000000000000000000000.000000");
        assert_eq!(pf("n=%d %g", 1.0), "n=%d 1");
    }
}
