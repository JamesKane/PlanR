//! Reading and writing files on the volume the loader was started from.

use core::marker::PhantomData;
use core::ptr::{self, NonNull};

use crate::{Guid, Handle, Protocol, Result, Status, raw};

/// `EFI_LOADED_IMAGE_PROTOCOL`: where this image came from.
#[repr(transparent)]
#[derive(Debug)]
pub struct LoadedImage(raw::LoadedImage);

// SAFETY: GUID and layout are from the UEFI specification.
unsafe impl Protocol for LoadedImage {
    const GUID: Guid = Guid::new(0x5b1b_31a1, 0x9562, 0x11d2, 0x8e3f, 0x00a0_c969_723b);
}

impl LoadedImage {
    /// The device (partition) the image was loaded from.
    #[must_use]
    pub fn device(&self) -> Handle {
        self.0.device_handle
    }
}

/// `EFI_SIMPLE_FILE_SYSTEM_PROTOCOL`.
#[repr(transparent)]
#[derive(Debug)]
pub struct SimpleFileSystem(raw::SimpleFileSystem);

// SAFETY: GUID and layout are from the UEFI specification.
unsafe impl Protocol for SimpleFileSystem {
    const GUID: Guid = Guid::new(0x964e_5b22, 0x6459, 0x11d2, 0x8e39, 0x00a0_c969_723b);
}

impl SimpleFileSystem {
    /// Opens the root directory of the volume.
    pub fn open_volume(&self) -> Result<File<'_>> {
        let mut root = ptr::null_mut();
        let this = (&raw const self.0).cast_mut();
        // SAFETY: firmware interface; `this` is the protocol instance.
        unsafe { (self.0.open_volume)(this, &raw mut root) }.to_result()?;
        File::from_raw(root)
    }
}

const FILE_INFO: Guid = Guid::new(0x0957_6e92, 0x6d3f, 0x11d2, 0x8e39, 0x00a0_c969_723b);
const MODE_READ: u64 = 1;
const MODE_WRITE: u64 = 2;
/// Longest path accepted, in UCS-2 code units.
const MAX_PATH: usize = 255;

/// An open file or directory, closed on drop.
#[derive(Debug)]
pub struct File<'a> {
    raw: NonNull<raw::FileProtocol>,
    _life: PhantomData<&'a SimpleFileSystem>,
}

impl File<'_> {
    fn from_raw<'b>(p: *mut raw::FileProtocol) -> Result<File<'b>> {
        NonNull::new(p)
            .map(|raw| File {
                raw,
                _life: PhantomData,
            })
            .ok_or(Status::DEVICE_ERROR)
    }

    fn ptr(&self) -> *mut raw::FileProtocol {
        self.raw.as_ptr()
    }

    /// Opens `path` (`/` or `\` separated) relative to this directory,
    /// read-only.
    pub fn open(&self, path: &str) -> Result<File<'_>> {
        self.open_mode(path, MODE_READ)
    }

    /// Opens the existing file `path` for writing and empties it. Never
    /// creates a file: `NOT_FOUND` if there is none.
    pub fn rewrite(&self, path: &str) -> Result<File<'_>> {
        let mut f = self.open_mode(path, MODE_READ | MODE_WRITE)?;
        f.truncate()?;
        Ok(f)
    }

    fn open_mode(&self, path: &str, mode: u64) -> Result<File<'_>> {
        let mut name = [0u16; MAX_PATH + 1];
        for (i, u) in path.encode_utf16().enumerate() {
            if i == MAX_PATH {
                return Err(Status::INVALID_PARAMETER);
            }
            name[i] = if u == u16::from(b'/') { u16::from(b'\\') } else { u };
        }
        let mut out = ptr::null_mut();
        // SAFETY: `name` is NUL-terminated; `self` is an open directory.
        unsafe { ((*self.ptr()).open)(self.ptr(), &raw mut out, name.as_ptr(), mode, 0) }.to_result()?;
        File::from_raw(out)
    }

    /// The file's `EFI_FILE_INFO` (its sizes, times and attributes, then
    /// the name; `FileSize` is the second `u64`) and the info's length in
    /// bytes.
    fn info(&self) -> Result<([u64; 128], usize)> {
        // 8-byte aligned, room for a long name.
        let mut buf = [0u64; 128];
        let mut len = size_of_val(&buf);
        let guid = FILE_INFO;
        // SAFETY: valid buffer and length.
        unsafe { ((*self.ptr()).get_info)(self.ptr(), &raw const guid, &raw mut len, buf.as_mut_ptr().cast()) }
            .to_result()?;
        Ok((buf, len))
    }

    /// The file's size in bytes.
    pub fn size(&self) -> Result<u64> {
        Ok(self.info()?.0[1])
    }

    /// Cuts the file to zero bytes.
    fn truncate(&mut self) -> Result<()> {
        let (mut buf, len) = self.info()?;
        if buf[1] == 0 {
            return Ok(());
        }
        buf[1] = 0;
        let guid = FILE_INFO;
        // SAFETY: the info just read, `len` bytes, with FileSize changed.
        unsafe { ((*self.ptr()).set_info)(self.ptr(), &raw const guid, len, buf.as_ptr().cast()) }.to_result()
    }

    /// Writes all of `buf` at the current position.
    pub fn write_all(&mut self, mut buf: &[u8]) -> Result<()> {
        while !buf.is_empty() {
            let mut len = buf.len();
            // SAFETY: valid buffer and length.
            unsafe { ((*self.ptr()).write)(self.ptr(), &raw mut len, buf.as_ptr().cast()) }.to_result()?;
            if len == 0 {
                return Err(Status::DEVICE_ERROR);
            }
            buf = &buf[len..];
        }
        Ok(())
    }

    /// Writes cached data to the device.
    pub fn flush(&mut self) -> Result<()> {
        // SAFETY: an open file.
        unsafe { ((*self.ptr()).flush)(self.ptr()) }.to_result()
    }

    /// Reads up to `buf.len()` bytes; returns the count (0 at end of file).
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let mut len = buf.len();
        // SAFETY: valid buffer and length.
        unsafe { ((*self.ptr()).read)(self.ptr(), &raw mut len, buf.as_mut_ptr().cast()) }.to_result()?;
        Ok(len)
    }

    /// Fills `buf` completely or fails.
    pub fn read_exact(&mut self, mut buf: &mut [u8]) -> Result<()> {
        while !buf.is_empty() {
            let n = self.read(buf)?;
            if n == 0 {
                return Err(Status::DEVICE_ERROR);
            }
            buf = &mut buf[n..];
        }
        Ok(())
    }
}

impl Drop for File<'_> {
    fn drop(&mut self) {
        // SAFETY: the handle is open and dropped exactly once.
        let _ = unsafe { ((*self.ptr()).close)(self.ptr()) };
    }
}
