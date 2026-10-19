//! Formatted output: Rust's formatting, written as Plan 9's `print`
//! writes. A call formats into a buffer and writes it in one piece (in
//! pieces of [`PRINT_MAX`] if longer), so lines from processes sharing a
//! descriptor do not interleave. The macros are [`print!`](crate::print),
//! [`println!`](crate::println), [`eprint!`](crate::eprint),
//! [`eprintln!`](crate::eprintln) and [`fprint!`](crate::fprint).
//!
//! Plan 9's verbs that Rust lacks are types to format: [`Quoted`] (`%q`),
//! [`RcQuoted`] (`%q` with rc's `needsrcquote`), [`Text`] (bytes shown as
//! UTF-8, `%s` on bytes) and [`Rerr`] (`%r`).

use core::fmt;

use crate::io::{File, Write};
use crate::utf;
use crate::{Error, Fd};

/// The most one write of a print carries.
pub const PRINT_MAX: usize = 1024;

/// Formats into a buffer, writing it out when full.
struct Out<W> {
    w: W,
    buf: [u8; PRINT_MAX],
    len: usize,
    /// Bytes written so far.
    done: usize,
    err: Option<Error>,
}

impl<W: Write> Out<W> {
    fn flush(&mut self) {
        if self.err.is_none() && self.len > 0 {
            match self.w.write_all(&self.buf[..self.len]) {
                Ok(()) => self.done += self.len,
                Err(e) => self.err = Some(e),
            }
        }
        self.len = 0;
    }
}

impl<W: Write> fmt::Write for Out<W> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut b = s.as_bytes();
        while !b.is_empty() {
            if self.len == PRINT_MAX {
                self.flush();
            }
            let n = b.len().min(PRINT_MAX - self.len);
            self.buf[self.len..self.len + n].copy_from_slice(&b[..n]);
            self.len += n;
            b = &b[n..];
        }
        if self.err.is_some() { Err(fmt::Error) } else { Ok(()) }
    }
}

/// Writes `args` to `w` as described above; returns the bytes written.
pub fn write_fmt_to(w: impl Write, args: fmt::Arguments<'_>) -> Result<usize, Error> {
    let mut o = Out {
        w,
        buf: [0; PRINT_MAX],
        len: 0,
        done: 0,
        err: None,
    };
    let r = fmt::write(&mut o, args);
    o.flush();
    match (o.err, r) {
        (Some(e), _) => Err(e),
        (None, Err(_)) => Err(Error::from_text(b"formatting failed")),
        (None, Ok(())) => Ok(o.done),
    }
}

/// Writes `args` to descriptor `fd`; what [`fprint!`](crate::fprint) calls.
pub fn fprint(fd: Fd, args: fmt::Arguments<'_>) -> Result<usize, Error> {
    write_fmt_to(File(fd), args)
}

/// Prints to standard output; errors are ignored.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{ let _ = $crate::print::fprint(1, format_args!($($arg)*)); }};
}

/// Prints a line to standard output; errors are ignored.
#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {{ let _ = $crate::print::fprint(1, format_args!("{}\n", format_args!($($arg)*))); }};
}

/// Prints to standard error; errors are ignored.
#[macro_export]
macro_rules! eprint {
    ($($arg:tt)*) => {{ let _ = $crate::print::fprint(2, format_args!($($arg)*)); }};
}

/// Prints a line to standard error; errors are ignored.
#[macro_export]
macro_rules! eprintln {
    () => { $crate::eprint!("\n") };
    ($($arg:tt)*) => {{ let _ = $crate::print::fprint(2, format_args!("{}\n", format_args!($($arg)*))); }};
}

/// Prints to a descriptor: `fprint!(fd, …)` gives `Result<usize, Error>`.
#[macro_export]
macro_rules! fprint {
    ($fd:expr, $($arg:tt)*) => { $crate::print::fprint($fd, format_args!($($arg)*)) };
}

/// Bytes shown as UTF-8, each byte that is not as U+FFFD (as
/// [`utf::decode`] reads it).
#[derive(Debug, Clone, Copy)]
pub struct Text<'a>(pub &'a [u8]);

impl fmt::Display for Text<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = self.0;
        while !s.is_empty() {
            match core::str::from_utf8(s) {
                Ok(t) => return f.write_str(t),
                Err(e) => {
                    let (good, bad) = s.split_at(e.valid_up_to());
                    // SAFETY: `from_utf8` checked this much.
                    f.write_str(unsafe { core::str::from_utf8_unchecked(good) })?;
                    f.write_str("\u{fffd}")?;
                    s = &bad[1..];
                }
            }
        }
        Ok(())
    }
}

/// `%q`: the text as is if it needs no quotes, else in single quotes with
/// each quote doubled. Empty text, a rune at or below space, or a quote
/// needs them.
#[derive(Debug, Clone, Copy)]
pub struct Quoted<'a>(pub &'a [u8]);

/// `%q` as rc sets it up: rc's special characters need quotes too.
#[derive(Debug, Clone, Copy)]
pub struct RcQuoted<'a>(pub &'a [u8]);

/// The characters besides those [`Quoted`] quotes that rc's
/// `needsrcquote` quotes.
pub const RC_SPECIAL: &str = "`^#*[]=|\\?${}()'<>&;";

fn quote(f: &mut fmt::Formatter<'_>, s: &[u8], special: &str) -> fmt::Result {
    let needs = |c: char| c <= ' ' || c == '\'' || special.contains(c);
    if !s.is_empty() && !utf::runes(s).any(needs) {
        return fmt::Display::fmt(&Text(s), f);
    }
    f.write_str("'")?;
    for part in s.split(|&b| b == b'\'').enumerate() {
        if part.0 > 0 {
            f.write_str("''")?;
        }
        fmt::Display::fmt(&Text(part.1), f)?;
    }
    f.write_str("'")
}

impl fmt::Display for Quoted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        quote(f, self.0, "")
    }
}

impl fmt::Display for RcQuoted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        quote(f, self.0, RC_SPECIAL)
    }
}

/// `%M`: a file's mode as `ls -l` shows it, `d-rwxr-xr-x`: `d` for a
/// directory, `a` for append-only, `A` for an authentication file, `l`
/// for exclusive use, then the owner's, group's and others' permissions.
#[derive(Debug, Clone, Copy)]
pub struct Mode(pub u32);

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.0;
        let kind = if m & abi::DMDIR != 0 {
            'd'
        } else if m & abi::DMAPPEND != 0 {
            'a'
        } else if m & abi::DMAUTH != 0 {
            'A'
        } else {
            '-'
        };
        let mut s = [0u8; 11];
        s[0] = kind as u8;
        s[1] = if m & abi::DMEXCL != 0 { b'l' } else { b'-' };
        for (i, shift) in [6, 3, 0].into_iter().enumerate() {
            let p = (m >> shift) & 7;
            s[2 + 3 * i] = if p & 4 != 0 { b'r' } else { b'-' };
            s[3 + 3 * i] = if p & 2 != 0 { b'w' } else { b'-' };
            s[4 + 3 * i] = if p & 1 != 0 { b'x' } else { b'-' };
        }
        f.write_str(core::str::from_utf8(&s).unwrap_or("?"))
    }
}

/// `%r`: the process's error text, which is left as it was.
#[derive(Debug, Clone, Copy)]
pub struct Rerr;

impl fmt::Display for Rerr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&crate::rerrstr(), f)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Text(self.as_bytes()), f)
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;

    use super::*;

    #[test]
    fn quoting() {
        let q = |s: &str| format!("{}", Quoted(s.as_bytes()));
        assert_eq!(q("plain"), "plain");
        assert_eq!(q(""), "''");
        assert_eq!(q("two words"), "'two words'");
        assert_eq!(q("it's"), "'it''s'");
        assert_eq!(q("a$b"), "a$b");
        assert_eq!(q("tab\there"), "'tab\there'");
        let rq = |s: &str| format!("{}", RcQuoted(s.as_bytes()));
        assert_eq!(rq("a$b"), "'a$b'");
        assert_eq!(rq("x=1"), "'x=1'");
        assert_eq!(rq("é"), "é");
    }

    #[test]
    fn modes() {
        assert_eq!(format!("{}", Mode(abi::DMDIR | 0o775)), "d-rwxrwxr-x");
        assert_eq!(format!("{}", Mode(abi::DMAPPEND | abi::DMEXCL | 0o640)), "alrw-r-----");
        assert_eq!(format!("{}", Mode(0o4)), "--------r--");
    }

    #[test]
    fn text_replaces_bad_bytes() {
        assert_eq!(format!("{}", Text(b"a\xffb\xe4\xb8")), "a\u{fffd}b\u{fffd}\u{fffd}");
        assert_eq!(format!("{}", Text("世".as_bytes())), "世");
    }

    #[test]
    fn writes_in_pieces() {
        /// Counts writes.
        struct Calls(Vec<usize>);
        impl Write for &mut Calls {
            fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
                self.0.push(buf.len());
                Ok(buf.len())
            }
        }
        let mut c = Calls(Vec::new());
        assert_eq!(write_fmt_to(&mut c, format_args!("{} {}\n", "short", 42)), Ok(9));
        assert_eq!(c.0, [9], "one write");
        let long: String = "x".repeat(PRINT_MAX + 10);
        c.0.clear();
        assert_eq!(write_fmt_to(&mut c, format_args!("{long}")), Ok(PRINT_MAX + 10));
        assert_eq!(c.0, [PRINT_MAX, 10]);
    }

    #[test]
    fn write_errors_come_back() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> Result<usize, Error> {
                Err(Error::from_text(b"write on closed pipe"))
            }
        }
        let e = write_fmt_to(Broken, format_args!("x")).unwrap_err();
        assert_eq!(e.as_bytes(), b"write on closed pipe");
    }
}
