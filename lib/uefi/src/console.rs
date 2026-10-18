//! Text console output (`EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL`).

use core::fmt;
use core::marker::PhantomData;

use crate::raw;

/// A console that implements [`fmt::Write`], translating UTF-8 to UCS-2 and
/// `\n` to `\r\n`.
#[derive(Debug)]
pub struct Console<'a> {
    out: *mut raw::SimpleTextOutput,
    _life: PhantomData<&'a mut raw::SimpleTextOutput>,
}

/// Code units sent to the firmware per `OutputString` call (plus a NUL).
const CHUNK: usize = 126;

impl Console<'_> {
    /// # Safety
    ///
    /// `out` must be a valid text output protocol, or null (output is then
    /// discarded), for the lifetime of the console.
    pub(crate) unsafe fn new(out: *mut raw::SimpleTextOutput) -> Self {
        Console {
            out,
            _life: PhantomData,
        }
    }

    fn emit(&mut self, units: &[u16]) {
        if self.out.is_null() {
            return;
        }
        // SAFETY: `units` is NUL-terminated by `encode`; `out` is valid.
        let _ = unsafe { ((*self.out).output_string)(self.out, units.as_ptr()) };
    }
}

/// Encodes `s` into NUL-terminated UCS-2 chunks of at most [`CHUNK`] units,
/// calling `sink` for each. Characters outside the BMP become U+FFFD.
fn encode(s: &str, mut sink: impl FnMut(&[u16])) {
    let mut buf = [0u16; CHUNK + 2];
    let mut n = 0;
    for c in s.chars() {
        if c == '\n' {
            buf[n] = u16::from(b'\r');
            n += 1;
        }
        buf[n] = u16::try_from(u32::from(c)).unwrap_or(0xfffd);
        n += 1;
        if n >= CHUNK {
            buf[n] = 0;
            sink(&buf[..=n]);
            n = 0;
        }
    }
    if n > 0 {
        buf[n] = 0;
        sink(&buf[..=n]);
    }
}

impl fmt::Write for Console<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        encode(s, |chunk| self.emit(chunk));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(s: &str) -> Vec<Vec<u16>> {
        let mut v = Vec::new();
        encode(s, |c| v.push(c.to_vec()));
        v
    }

    #[test]
    fn newline_becomes_crlf() {
        assert_eq!(collect("a\nb"), vec![vec![97, 13, 10, 98, 0]]);
    }

    #[test]
    fn non_bmp_is_replaced() {
        assert_eq!(collect("\u{1f600}"), vec![vec![0xfffd, 0]]);
    }

    #[test]
    fn long_strings_are_chunked_and_terminated() {
        let s = "x".repeat(300);
        let chunks = collect(&s);
        assert!(chunks.iter().all(|c| c.len() <= CHUNK + 1 && *c.last().unwrap() == 0));
        assert_eq!(chunks.iter().map(|c| c.len() - 1).sum::<usize>(), 300);
    }
}
