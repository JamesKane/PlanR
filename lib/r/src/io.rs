//! Reading and writing, behind two small traits so that buffered I/O and
//! formatted printing work on descriptors and, in tests, on memory.

use alloc::vec::Vec;

use crate::{Error, Fd};

/// Something to read bytes from.
pub trait Read {
    /// Reads at most `buf.len()` bytes; 0 at end of file.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error>;
}

/// Something to write bytes to.
pub trait Write {
    /// Writes some of `buf`; returns how much.
    fn write(&mut self, buf: &[u8]) -> Result<usize, Error>;

    /// Writes all of `buf`.
    fn write_all(&mut self, mut buf: &[u8]) -> Result<(), Error> {
        while !buf.is_empty() {
            match self.write(buf)? {
                0 => return Err(Error::from_text(b"short write")),
                n => buf = &buf[n..],
            }
        }
        Ok(())
    }
}

/// An open file, by its descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct File(pub Fd);

/// Standard input, output and error.
pub const STDIN: File = File(0);
pub const STDOUT: File = File(1);
pub const STDERR: File = File(2);

impl Read for File {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        crate::read(self.0, buf)
    }
}

impl Write for File {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
        crate::write(self.0, buf)
    }
}

impl Read for &[u8] {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        let n = buf.len().min(self.len());
        buf[..n].copy_from_slice(&self[..n]);
        *self = &self[n..];
        Ok(n)
    }
}

impl Write for Vec<u8> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
        self.extend_from_slice(buf);
        Ok(buf.len())
    }
}

impl<T: Read + ?Sized> Read for &mut T {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        (**self).read(buf)
    }
}

impl<T: Write + ?Sized> Write for &mut T {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
        (**self).write(buf)
    }
}
