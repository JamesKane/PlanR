//! Copying to and from user memory.
//!
//! Every system call argument that points into user memory goes through
//! here. A range must lie wholly in the user half: SMAP and PAN keep the
//! kernel out of user pages, but nothing but this check keeps a user
//! pointer out of the kernel's. The copy itself runs in the
//! architecture's user-access window, and a fault ends it through a
//! fixup entry instead of a panic.

use alloc::vec::Vec;

use crate::arch;
use crate::port::mm::PAGE;

/// The end of the user half. The last page below the canonical boundary
/// is left out, so that a user `rip` there can never reach `sysret` with
/// a non-canonical successor.
pub(crate) const USER_TOP: u64 = 0x0000_7fff_ffff_f000;

/// A user address range that is outside the user half or not mapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BadAddress;

/// Checks that `[addr, addr + len)` lies in the user half.
pub(crate) fn check(addr: u64, len: usize) -> Result<(), BadAddress> {
    match addr.checked_add(len as u64) {
        Some(end) if end <= USER_TOP => Ok(()),
        _ => Err(BadAddress),
    }
}

/// Fills `dst` from user memory at `src`.
pub(crate) fn copy_in(dst: &mut [u8], src: u64) -> Result<(), BadAddress> {
    check(src, dst.len())?;
    // SAFETY: `dst` is ours to write, and `src` is a user range.
    match unsafe { arch::copy_in(dst.as_mut_ptr(), src, dst.len()) } {
        0 => Ok(()),
        _ => Err(BadAddress),
    }
}

/// Writes `src` to user memory at `dst`.
pub(crate) fn copy_out(dst: u64, src: &[u8]) -> Result<(), BadAddress> {
    check(dst, src.len())?;
    // SAFETY: `src` is ours to read, and `dst` is a user range.
    match unsafe { arch::copy_out(dst, src.as_ptr(), src.len()) } {
        0 => Ok(()),
        _ => Err(BadAddress),
    }
}

/// Reads a NUL-terminated string from user memory at `src`: the bytes
/// before the NUL, or the first `max` bytes if there is none sooner. Reads
/// a page at a time, so it never touches a page past the string's end.
pub(crate) fn copy_in_str(src: u64, max: usize) -> Result<Vec<u8>, BadAddress> {
    let mut s = Vec::new();
    let mut at = src;
    while s.len() < max {
        #[allow(clippy::cast_possible_truncation)] // at most a page
        let in_page = (PAGE - at % PAGE) as usize;
        let n = in_page.min(max - s.len());
        let start = s.len();
        s.resize(start + n, 0);
        copy_in(&mut s[start..], at)?;
        if let Some(nul) = s[start..].iter().position(|&b| b == 0) {
            s.truncate(start + nul);
            return Ok(s);
        }
        at += n as u64;
    }
    Ok(s)
}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use super::{BadAddress, USER_TOP, copy_in, copy_in_str, copy_out};
    use crate::port::ktest::TestResult;
    use crate::port::mm::{self, PAGE};

    /// Copies reach a mapped user page, and refuse kernel addresses,
    /// ranges that wrap or cross the top, and unmapped pages.
    pub(crate) fn copies_checked() -> TestResult {
        let va = mm::ktests::user_page()?;
        let mut w = [0u8; 8];
        copy_in(&mut w, va).map_err(|_| "cannot read a user page")?;
        if u64::from_ne_bytes(w) != mm::ktests::USER_WORD {
            return Err("read the wrong bytes".into());
        }
        // Write a string at the end of the page and read it back.
        let at = va + PAGE - 6;
        copy_out(at, b"hello\0").map_err(|_| "cannot write a user page")?;
        if copy_in_str(at, 64) != Ok(b"hello".to_vec()) {
            return Err("string not read back".into());
        }
        // No NUL before the page ends: the next page is unmapped.
        copy_out(at, b"hellos").map_err(|_| "cannot write a user page")?;
        if copy_in_str(at, 64) != Err(BadAddress) {
            return Err("a string ran on into an unmapped page".into());
        }
        if copy_in_str(at, 3) != Ok(b"hel".to_vec()) {
            return Err("string not cut at its limit".into());
        }
        copy_out(va, &mm::ktests::USER_WORD.to_ne_bytes()).map_err(|_| "cannot restore the page")?;
        let kernel = (&raw const w) as u64;
        let refused = [
            copy_in(&mut w, kernel),
            copy_out(kernel, &[0; 8]),
            copy_in(&mut w, USER_TOP - 4),
            copy_in(&mut w, u64::MAX - 3),
            copy_in(&mut w, va + PAGE),
        ];
        if refused.iter().any(Result::is_ok) {
            return Err("a bad range was copied".into());
        }
        Ok(())
    }
}
