//! Buffered I/O, as Plan 9's `bio(2)`: [`Bin`] reads bytes, runes and
//! lines; [`Bout`] gathers writes and formatted text into large writes.
//!
//! Unlike `Brdline`, [`Bin::line`] has no length limit: the buffer grows
//! to hold a long line.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::Error;
use crate::io::{Read, Write};
use crate::utf;

/// The buffer size, as Plan 9's `Bsize`.
pub const BSIZE: usize = 8 * 1024;

/// A buffered reader.
#[derive(Debug)]
pub struct Bin<R> {
    r: R,
    buf: Vec<u8>,
    /// Unread bytes are `buf[rp..wp]`.
    rp: usize,
    wp: usize,
    eof: bool,
    /// Bytes handed out so far.
    offset: u64,
}

impl<R: Read> Bin<R> {
    pub fn new(r: R) -> Bin<R> {
        Bin {
            r,
            buf: vec![0; BSIZE],
            rp: 0,
            wp: 0,
            eof: false,
            offset: 0,
        }
    }

    /// The bytes read so far through this reader (Plan 9's `Boffset`).
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Reads more, keeping what is unread and the byte before it (for
    /// [`ungetc`](Self::ungetc)); false at end of file.
    fn fill(&mut self) -> Result<bool, Error> {
        if self.eof {
            return Ok(false);
        }
        let keep = self.rp.min(1);
        if self.rp > keep {
            self.buf.copy_within(self.rp - keep..self.wp, 0);
            self.wp -= self.rp - keep;
            self.rp = keep;
        }
        if self.wp == self.buf.len() {
            let n = self.buf.len();
            self.buf.resize(2 * n, 0);
        }
        let n = self.r.read(&mut self.buf[self.wp..])?;
        if n == 0 {
            self.eof = true;
            return Ok(false);
        }
        self.wp += n;
        Ok(true)
    }

    /// The next byte; `None` at end of file.
    pub fn getc(&mut self) -> Result<Option<u8>, Error> {
        if self.rp == self.wp && !self.fill()? {
            return Ok(None);
        }
        let b = self.buf[self.rp];
        self.rp += 1;
        self.offset += 1;
        Ok(Some(b))
    }

    /// Puts back the byte just read by [`getc`](Self::getc) (or the last of
    /// what was read by any other call); false if there is none.
    pub fn ungetc(&mut self) -> bool {
        if self.rp == 0 {
            return false;
        }
        self.rp -= 1;
        self.offset -= 1;
        true
    }

    /// The next rune, as Plan 9's `Bgetrune` reads it: after a byte
    /// that starts a sequence, as many more as it says, all taken; if
    /// they are not UTF-8, the rune is [`utf::RUNE_ERROR`]. A sequence the
    /// file ends in the middle of is dropped. `None` at end of file.
    pub fn getrune(&mut self) -> Result<Option<char>, Error> {
        let Some(c) = self.getc()? else { return Ok(None) };
        if c < utf::RUNE_SELF {
            return Ok(Some(char::from(c)));
        }
        let mut s = [c, 0, 0, 0];
        let mut i = 1;
        loop {
            let Some(c) = self.getc()? else { return Ok(None) };
            if i >= utf::UTF_MAX {
                return Ok(Some(utf::RUNE_ERROR));
            }
            s[i] = c;
            i += 1;
            if full_rune(&s[..i]) {
                // A good sequence is all of the bytes, so none go back.
                return Ok(Some(utf::decode(&s[..i]).0));
            }
        }
    }

    /// The next line, with its `delim` unless the file ended first; `None`
    /// at end of file.
    pub fn line(&mut self, delim: u8) -> Result<Option<&[u8]>, Error> {
        let mut from = self.rp;
        let end = loop {
            if let Some(i) = self.buf[from..self.wp].iter().position(|&b| b == delim) {
                break from + i + 1;
            }
            let scanned = self.wp - self.rp;
            if !self.fill()? {
                if self.rp == self.wp {
                    return Ok(None);
                }
                break self.wp;
            }
            from = self.rp + scanned;
        };
        let start = self.rp;
        self.rp = end;
        self.offset += (end - start) as u64;
        Ok(Some(&self.buf[start..end]))
    }

    /// Reads at most `out.len()` bytes; 0 at end of file.
    pub fn read(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.rp == self.wp && !self.fill()? {
            return Ok(0);
        }
        let n = out.len().min(self.wp - self.rp);
        out[..n].copy_from_slice(&self.buf[self.rp..self.rp + n]);
        self.rp += n;
        self.offset += n as u64;
        Ok(n)
    }

    /// The reader underneath.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.r
    }
}

/// Whether `s` is as long as its first byte says a rune is (Plan 9's
/// `fullrune`): one byte below 0x80, two below 0xe0, three below 0xf0,
/// else four.
fn full_rune(s: &[u8]) -> bool {
    match s.first() {
        None => false,
        Some(&c) if c < 0x80 => true,
        Some(&c) if c < 0xe0 => s.len() >= 2,
        Some(&c) if c < 0xf0 => s.len() >= 3,
        Some(_) => s.len() >= 4,
    }
}

/// A buffered writer. Dropping it flushes it, ignoring errors; call
/// [`flush`](Self::flush) to see them.
#[derive(Debug)]
pub struct Bout<W: Write> {
    w: W,
    buf: Vec<u8>,
}

impl<W: Write> Bout<W> {
    pub fn new(w: W) -> Bout<W> {
        Bout {
            w,
            buf: Vec::with_capacity(BSIZE),
        }
    }

    /// Writes `b`, through the buffer.
    pub fn write(&mut self, b: &[u8]) -> Result<(), Error> {
        if self.buf.len() + b.len() > BSIZE {
            self.flush()?;
            if b.len() >= BSIZE {
                return self.w.write_all(b);
            }
        }
        self.buf.extend_from_slice(b);
        Ok(())
    }

    pub fn putc(&mut self, b: u8) -> Result<(), Error> {
        self.write(&[b])
    }

    pub fn putrune(&mut self, c: char) -> Result<(), Error> {
        self.write(c.encode_utf8(&mut [0; utf::UTF_MAX]).as_bytes())
    }

    /// Writes what is buffered.
    pub fn flush(&mut self) -> Result<(), Error> {
        let r = self.w.write_all(&self.buf);
        self.buf.clear();
        r
    }

    /// Formats `args` into the buffer: `write!(bout, …)` works too, but
    /// loses the error's text.
    pub fn print(&mut self, args: fmt::Arguments<'_>) -> Result<(), Error> {
        let mut err = None;
        let mut sink = Sink { b: self, err: &mut err };
        let r = fmt::write(&mut sink, args);
        match (err, r) {
            (Some(e), _) => Err(e),
            (None, Err(_)) => Err(Error::from_text(b"formatting failed")),
            (None, Ok(())) => Ok(()),
        }
    }
}

struct Sink<'a, W: Write> {
    b: &'a mut Bout<W>,
    err: &'a mut Option<Error>,
}

impl<W: Write> fmt::Write for Sink<'_, W> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.b.write(s.as_bytes()).map_err(|e| {
            *self.err = Some(e);
            fmt::Error
        })
    }
}

impl<W: Write> fmt::Write for Bout<W> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write(s.as_bytes()).map_err(|_| fmt::Error)
    }
}

impl<W: Write> Drop for Bout<W> {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    /// Hands out at most `n` bytes a read, to cut lines across fills.
    struct Trickle<'a>(&'a [u8], usize);

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
            let n = buf.len().min(self.1).min(self.0.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    fn lines(input: &[u8], step: usize) -> Vec<Vec<u8>> {
        let mut b = Bin::new(Trickle(input, step));
        let mut out = Vec::new();
        while let Some(l) = b.line(b'\n').unwrap() {
            out.push(l.to_vec());
        }
        assert_eq!(b.offset(), input.len() as u64);
        out
    }

    #[test]
    fn lines_across_reads() {
        for step in [1, 3, 7, BSIZE] {
            assert_eq!(
                lines(b"one\ntwo\n\nlast", step),
                [&b"one\n"[..], b"two\n", b"\n", b"last"]
            );
        }
        assert_eq!(lines(b"", 5), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn long_lines_grow_the_buffer() {
        let mut input = vec![b'x'; 3 * BSIZE + 5];
        input.push(b'\n');
        input.extend_from_slice(b"after\n");
        let got = lines(&input, 1000);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].len(), 3 * BSIZE + 6);
        assert_eq!(got[1], b"after\n");
    }

    #[test]
    fn bytes_runes_and_unget() {
        let s = "aé世\u{1d11e}".as_bytes();
        let mut b = Bin::new(Trickle(s, 1));
        assert_eq!(b.getc().unwrap(), Some(b'a'));
        assert!(b.ungetc());
        assert_eq!(b.getrune().unwrap(), Some('a'));
        assert_eq!(b.getrune().unwrap(), Some('é'));
        assert_eq!(b.getrune().unwrap(), Some('世'));
        assert_eq!(b.getrune().unwrap(), Some('\u{1d11e}'));
        assert_eq!(b.getrune().unwrap(), None);
        // As Plan 9's Bgetrune: a bad sequence takes as many bytes as its
        // first says, and one cut off by the end is dropped.
        let mut bad = Bin::new(&b"\xe4\xb8A\x80B\xe4\xb8"[..]);
        assert_eq!(bad.getrune().unwrap(), Some(utf::RUNE_ERROR));
        assert_eq!(bad.getrune().unwrap(), Some(utf::RUNE_ERROR));
        assert_eq!(bad.getrune().unwrap(), None);
        assert_eq!(bad.offset(), 7);
    }

    #[test]
    fn unget_survives_a_fill() {
        let mut b = Bin::new(Trickle(b"xy", 1));
        assert_eq!(b.getc().unwrap(), Some(b'x'));
        assert_eq!(b.getc().unwrap(), Some(b'y'));
        assert!(b.ungetc());
        assert_eq!(b.getc().unwrap(), Some(b'y'));
        assert_eq!(b.getc().unwrap(), None);
    }

    #[test]
    fn bout_buffers_and_flushes() {
        let mut sink = Vec::new();
        {
            let mut o = Bout::new(&mut sink);
            o.write(b"hello, ").unwrap();
            o.print(format_args!("{} {}", "world", 7)).unwrap();
            o.putrune('é').unwrap();
            o.putc(b'\n').unwrap();
        }
        assert_eq!(sink, "hello, world 7é\n".as_bytes());
        let mut big = Vec::new();
        let mut o = Bout::new(&mut big);
        o.write(&[1; 100]).unwrap();
        o.write(&[2; BSIZE]).unwrap();
        o.flush().unwrap();
        drop(o);
        assert_eq!(big.len(), 100 + BSIZE);
        assert!(big[..100].iter().all(|&b| b == 1));
    }
}
