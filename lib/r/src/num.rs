//! Numbers in text, as Plan 9's libc reads and prints them: `strtol`,
//! `strtoul`, `atol` and `atof` (each reading the longest prefix that is
//! a number, and saying how much it read), and the `%e`, `%f` and `%g`
//! conversions of `print` for one floating-point value ([`printf_float`]),
//! which `seq` takes from its user.

use alloc::string::String;
use alloc::vec::Vec;

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// The sign and digits of an integer at the start of `s`, in `base` (2
/// to 36; 0 for C's rule: `0x` is hex, a leading `0` octal, else
/// decimal), after blanks: whether it is negative, its magnitude
/// (saturated), and how many bytes it took (0 if there are no digits).
fn integer(s: &[u8], base: u32) -> (bool, u64, usize) {
    let mut i = 0;
    while i < s.len() && is_space(s[i]) {
        i += 1;
    }
    let neg = s.get(i) == Some(&b'-');
    if matches!(s.get(i), Some(b'-' | b'+')) {
        i += 1;
    }
    let hex_prefix = s.get(i) == Some(&b'0')
        && matches!(s.get(i + 1), Some(b'x' | b'X'))
        && s.get(i + 2).is_some_and(u8::is_ascii_hexdigit);
    let base = match base {
        0 | 16 if hex_prefix => {
            i += 2;
            16
        }
        0 if s.get(i) == Some(&b'0') => 8,
        0 => 10,
        b => b,
    };
    let start = i;
    let mut v: u64 = 0;
    while let Some(d) = s.get(i).and_then(|&c| char::from(c).to_digit(base)) {
        v = v.saturating_mul(u64::from(base)).saturating_add(u64::from(d));
        i += 1;
    }
    if i == start {
        return (false, 0, 0);
    }
    (neg, v, i)
}

/// `strtol`: the integer at the start of `s` and the bytes it took;
/// `(0, 0)` if there is none. Out of range, it is `i64::MAX` or
/// `i64::MIN`.
#[must_use]
pub fn strtol(s: &[u8], base: u32) -> (i64, usize) {
    let (neg, v, n) = integer(s, base);
    let v = if neg {
        0i64.checked_sub_unsigned(v).unwrap_or(i64::MIN)
    } else {
        i64::try_from(v).unwrap_or(i64::MAX)
    };
    (v, n)
}

/// `strtoul`: as [`strtol`], unsigned; a `-` negates, wrapping, as C's
/// does.
#[must_use]
pub fn strtoul(s: &[u8], base: u32) -> (u64, usize) {
    let (neg, v, n) = integer(s, base);
    (if neg { v.wrapping_neg() } else { v }, n)
}

/// `atol`: the integer at the start of `s`, in C's bases, or 0.
#[must_use]
pub fn atol(s: &[u8]) -> i64 {
    strtol(s, 0).0
}

/// `strtod`: the floating-point number at the start of `s` (blanks, a
/// sign, digits with perhaps a point, perhaps an exponent) and the bytes
/// it took; `(0.0, 0)` if there is none.
#[must_use]
pub fn strtod(s: &[u8]) -> (f64, usize) {
    let mut i = 0;
    while i < s.len() && is_space(s[i]) {
        i += 1;
    }
    let start = i;
    if matches!(s.get(i), Some(b'-' | b'+')) {
        i += 1;
    }
    let digits = |i: &mut usize| {
        let from = *i;
        while s.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        *i - from
    };
    let mut n = digits(&mut i);
    if s.get(i) == Some(&b'.') {
        i += 1;
        n += digits(&mut i);
    }
    if n == 0 {
        return (0.0, 0);
    }
    if matches!(s.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        if matches!(s.get(j), Some(b'-' | b'+')) {
            j += 1;
        }
        if digits(&mut j) > 0 {
            i = j;
        }
    }
    let text = core::str::from_utf8(&s[start..i]).unwrap_or("0");
    // Rust's parser takes neither a leading `+` nor a bare trailing `.`;
    // both mean nothing.
    let text = text.strip_prefix('+').unwrap_or(text);
    let v = match text.strip_suffix('.') {
        Some(t) => t.parse(),
        None => text.parse(),
    };
    (v.unwrap_or(0.0), i)
}

/// `atof`: the number at the start of `s`, or 0.
#[must_use]
pub fn atof(s: &[u8]) -> f64 {
    strtod(s).0
}

/// A conversion of `print` for a floating-point value: `%[-+ #0][width][.prec]{e,E,f,g,G}`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // print's flags
pub struct FloatSpec {
    /// `-`: pad on the right.
    pub left: bool,
    /// `+`: a sign even when positive.
    pub plus: bool,
    /// ` `: a blank where a plus sign would be.
    pub space: bool,
    /// `#`: always a point; for `g`, keep trailing zeros.
    pub alt: bool,
    /// `0`: pad with zeros after the sign.
    pub zero: bool,
    pub width: usize,
    pub prec: Option<usize>,
    /// `e`, `E`, `f`, `g` or `G`.
    pub verb: u8,
}

/// `v` in `%e` style with `prec` digits after the point: the digits, and
/// the exponent.
fn exp_form(v: f64, prec: usize) -> (String, i32) {
    let s = alloc::format!("{v:.prec$e}");
    let (m, e) = s.split_once('e').unwrap_or((&s, "0"));
    (String::from(m), e.parse().unwrap_or(0))
}

/// Takes away trailing zeros after a point, and the point if it is left
/// last.
fn trim_zeros(s: &mut String) {
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
}

/// `v` formatted as `spec` says, as C's `printf` does it.
#[must_use]
pub fn format_float(spec: &FloatSpec, v: f64) -> String {
    let neg = v.is_sign_negative() && !v.is_nan();
    let a = if neg { -v } else { v };
    let upper = spec.verb.is_ascii_uppercase();
    let mut body = if a.is_infinite() {
        String::from("inf")
    } else if a.is_nan() {
        String::from("nan")
    } else {
        let e_style = |prec: usize, trim: bool| {
            let (mut m, x) = exp_form(a, prec);
            if spec.alt && prec == 0 {
                m.push('.');
            }
            if trim {
                trim_zeros(&mut m);
            }
            let sign = if x < 0 { '-' } else { '+' };
            alloc::format!("{m}e{sign}{:02}", x.unsigned_abs())
        };
        let f_style = |prec: usize, trim: bool| {
            let mut s = alloc::format!("{a:.prec$}");
            if spec.alt && prec == 0 {
                s.push('.');
            }
            if trim {
                trim_zeros(&mut s);
            }
            s
        };
        match spec.verb.to_ascii_lowercase() {
            b'e' => e_style(spec.prec.unwrap_or(6), false),
            b'f' => f_style(spec.prec.unwrap_or(6), false),
            _ => {
                let p = spec.prec.unwrap_or(6).max(1);
                let x = if a == 0.0 { 0 } else { exp_form(a, p - 1).1 };
                let p = i32::try_from(p).unwrap_or(i32::MAX);
                if x < p && x >= -4 {
                    f_style(usize::try_from(p - 1 - x).unwrap_or(0), !spec.alt)
                } else {
                    e_style(usize::try_from(p - 1).unwrap_or(0), !spec.alt)
                }
            }
        }
    };
    if upper {
        body.make_ascii_uppercase();
    }
    let sign = if neg {
        "-"
    } else if spec.plus {
        "+"
    } else if spec.space {
        " "
    } else {
        ""
    };
    let len = sign.len() + body.len();
    let pad = spec.width.saturating_sub(len);
    let mut out = String::with_capacity(len + pad);
    if spec.left {
        out.push_str(sign);
        out.push_str(&body);
        out.extend(core::iter::repeat_n(' ', pad));
    } else if spec.zero && a.is_finite() {
        out.push_str(sign);
        out.extend(core::iter::repeat_n('0', pad));
        out.push_str(&body);
    } else {
        out.extend(core::iter::repeat_n(' ', pad));
        out.push_str(sign);
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
                b'#' => spec.alt = true,
                b'0' => spec.zero = true,
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

    #[test]
    fn integers() {
        assert_eq!(strtol(b"  42xyz", 0), (42, 4));
        assert_eq!(strtol(b"-0x1F", 0), (-31, 5));
        assert_eq!(strtol(b"017", 0), (15, 3));
        assert_eq!(strtol(b"017", 10), (17, 3));
        assert_eq!(strtol(b"0x", 0), (0, 1), "a 0 then junk");
        assert_eq!(strtol(b"abc", 0), (0, 0));
        assert_eq!(strtol(b"+7", 10), (7, 2));
        assert_eq!(strtol(b"99999999999999999999", 10).0, i64::MAX);
        assert_eq!(strtol(b"777", 8), (511, 3));
        assert_eq!(strtoul(b"-1", 10), (u64::MAX, 2));
        assert_eq!(atol(b"12 monkeys"), 12);
    }

    #[test]
    fn floats_in() {
        assert_eq!(strtod(b"1.5e3x"), (1500.0, 5));
        assert_eq!(strtod(b"-.5"), (-0.5, 3));
        assert_eq!(strtod(b"7."), (7.0, 2));
        assert_eq!(strtod(b"+2e"), (2.0, 2), "an e without digits is not taken");
        assert_eq!(strtod(b"."), (0.0, 0));
        assert_eq!(atof(b"x"), 0.0);
    }

    fn pf(fmt: &str, v: f64) -> String {
        String::from_utf8(printf_float(fmt.as_bytes(), v)).unwrap()
    }

    #[test]
    fn floats_out() {
        assert_eq!(g(1.0), "1");
        assert_eq!(g(0.1), "0.1");
        assert_eq!(g(0.300_000_000_000_000_04), "0.3");
        assert_eq!(g(100_000.0), "100000");
        assert_eq!(g(1_000_000.0), "1e+06");
        assert_eq!(g(0.0001), "0.0001");
        assert_eq!(g(0.00001), "1e-05");
        assert_eq!(g(-2.5), "-2.5");
        assert_eq!(g(0.0), "0");
        assert_eq!(g(123_456_789.0), "1.23457e+08");
        assert_eq!(pf("%5.2f|%-6g|%e", 1.23456), " 1.23|1.23456|1.234560e+00");
        assert_eq!(pf("%05.1f %%", -2.25), "-02.2 %");
        assert_eq!(pf("%+.3e", 12345.0), "+1.234e+04");
        assert_eq!(pf("%#g", 2.0), "2.00000");
        assert_eq!(pf("%G", 1e-10), "1E-10");
        assert_eq!(pf("n=%d %g", 1.0), "n=%d 1");
    }
}
