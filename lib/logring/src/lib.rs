//! The persistent log ring: console output kept in a block of RAM that
//! survives a warm reset, so the next 9boot can save it to the boot volume.
//!
//! 9boot places the block at the same physical address on every boot (see
//! `boot/9boot/src/logring.rs`), recovers what the previous boot wrote,
//! then [`reset`](Ring::reset)s it and hands it to the kernel, which
//! appends everything it prints. Memory that did not survive (a cold boot)
//! fails the header check and is ignored.
//!
//! 9boot finds the previous ring by checking every [`sites`] address in
//! free RAM for a valid header, so the ring is recovered even if the
//! firmware's own allocations shift between boots and the new one has to
//! go elsewhere.
//!
//! The layout is a 64-byte [`Header`] followed by the data bytes. `head`
//! counts every byte ever written, so the data index is `head % capacity`
//! and the ring has wrapped once `head > capacity`.

#![cfg_attr(not(test), no_std)]

use core::ptr::NonNull;

/// `Header::magic`: "PLANRLOG".
pub const MAGIC: u64 = u64::from_le_bytes(*b"PLANRLOG");
/// Bumped on any layout change.
pub const VERSION: u32 = 1;
/// Bytes before the data.
pub const HEADER_BYTES: usize = size_of::<Header>();

#[repr(C)]
#[derive(Debug)]
struct Header {
    magic: u64,
    version: u32,
    _pad: u32,
    /// Data bytes after the header.
    capacity: u64,
    /// Boots since the ring was last found invalid, counting this one.
    boot: u64,
    /// Bytes ever written.
    head: u64,
    /// Checks the fields above other than `head`.
    check: u64,
    _reserved: [u64; 2],
}

const _: () = assert!(HEADER_BYTES == 64);

/// The block 9boot reserves for the ring, header included.
pub const RING_BYTES: u64 = 1 << 20;
/// Rings start on boundaries of this size, so finding one means checking
/// one address per 2 MiB of free RAM.
pub const ALIGN: u64 = 2 << 20;

/// The addresses in free RAM `[start, end)` where a ring may be: every
/// [`ALIGN`] boundary with room for [`RING_BYTES`] after it, except 0.
pub fn sites(start: u64, end: u64) -> impl Iterator<Item = u64> {
    core::iter::successors(start.checked_next_multiple_of(ALIGN), |a| a.checked_add(ALIGN))
        .take_while(move |&a| a.checked_add(RING_BYTES).is_some_and(|e| e <= end))
        .filter(|&a| a != 0)
}

/// Where to put a new ring: the highest site in any of the `free` ranges.
/// Firmware tends to allocate low, so the top of RAM is the likeliest to
/// survive a reset untouched.
#[must_use]
pub fn place(free: &[(u64, u64)]) -> Option<u64> {
    free.iter()
        .filter_map(|&(start, end)| {
            let last = end.checked_sub(RING_BYTES)? / ALIGN * ALIGN;
            (last >= start && last != 0).then_some(last)
        })
        .max()
}

fn check(capacity: u64, boot: u64) -> u64 {
    MAGIC.rotate_left(13) ^ capacity.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ boot ^ u64::from(VERSION)
}

/// A log ring over a block of memory.
#[derive(Debug)]
pub struct Ring {
    header: NonNull<Header>,
    data: NonNull<u8>,
    capacity: usize,
}

// SAFETY: a `Ring` has exclusive access to its block (the `new` contract).
unsafe impl Send for Ring {}

/// What a valid ring found in memory says about the boot that wrote it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found {
    pub boot: u64,
    /// Bytes it ever wrote; the ring holds the last `capacity` of them.
    pub written: u64,
}

impl Ring {
    /// A ring over `[base, base + len)`, which is left as it is: call
    /// [`found`](Self::found) to see whether it already holds a ring and
    /// [`reset`](Self::reset) to start a new one. `None` if the block is
    /// too small or misaligned.
    ///
    /// # Safety
    ///
    /// The block must be valid for reads and writes, and used by nothing
    /// else, for as long as the `Ring` exists.
    pub unsafe fn new(base: *mut u8, len: usize) -> Option<Ring> {
        if len <= HEADER_BYTES || !(base as usize).is_multiple_of(align_of::<Header>()) {
            return None;
        }
        Some(Ring {
            header: NonNull::new(base.cast())?,
            // SAFETY: inside the block, which is longer than the header.
            data: NonNull::new(unsafe { base.add(HEADER_BYTES) })?,
            capacity: len - HEADER_BYTES,
        })
    }

    fn header(&self) -> Header {
        // SAFETY: the block starts with room for a header (`new`); volatile
        // because what is there may be left over from an earlier boot.
        unsafe { self.header.read_volatile() }
    }

    /// The ring already in the block, if its header is intact and matches
    /// this block's size.
    #[must_use]
    pub fn found(&self) -> Option<Found> {
        let h = self.header();
        let ok = h.magic == MAGIC
            && h.version == VERSION
            && h.capacity == self.capacity as u64
            && h.check == check(h.capacity, h.boot);
        ok.then_some(Found {
            boot: h.boot,
            written: h.head,
        })
    }

    /// Starts an empty ring for boot number `boot`.
    pub fn reset(&mut self, boot: u64) {
        let capacity = self.capacity as u64;
        let h = Header {
            magic: MAGIC,
            version: VERSION,
            _pad: 0,
            capacity,
            boot,
            head: 0,
            check: check(capacity, boot),
            _reserved: [0; 2],
        };
        // SAFETY: as in `header`.
        unsafe { self.header.write_volatile(h) };
    }

    /// Bytes ever written, from the header.
    fn head(&self) -> u64 {
        // SAFETY: as in `header`.
        unsafe { (&raw const (*self.header.as_ptr()).head).read_volatile() }
    }

    /// Appends `bytes`, overwriting the oldest once full. Does nothing to a
    /// ring that has not been [`reset`](Self::reset).
    pub fn write(&mut self, bytes: &[u8]) {
        if self.found().is_none() {
            return;
        }
        let mut head = self.head();
        for &b in bytes {
            #[allow(clippy::cast_possible_truncation)] // less than `capacity`
            let i = (head % self.capacity as u64) as usize;
            // SAFETY: `i < capacity`, inside the block.
            unsafe { self.data.add(i).write_volatile(b) };
            head += 1;
        }
        // The data first, then the count: a reset in between loses the new
        // bytes rather than exposing stale ones.
        // SAFETY: as in `header`.
        unsafe { (&raw mut (*self.header.as_ptr()).head).write_volatile(head) };
    }

    /// What the ring holds, oldest first, as two pieces (the second is
    /// empty until the ring wraps).
    #[must_use]
    pub fn contents(&self) -> (&[u8], &[u8]) {
        let head = self.head();
        // SAFETY: the data area is `capacity` bytes inside the block.
        let data = unsafe { core::slice::from_raw_parts(self.data.as_ptr(), self.capacity) };
        if head <= self.capacity as u64 {
            #[allow(clippy::cast_possible_truncation)] // at most `capacity`
            (&data[..head as usize], &[])
        } else {
            #[allow(clippy::cast_possible_truncation)] // less than `capacity`
            let i = (head % self.capacity as u64) as usize;
            (&data[i..], &data[..i])
        }
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(words: usize) -> Vec<u64> {
        vec![0xdead_beef_dead_beef; words]
    }

    fn ring(mem: &mut [u64]) -> Ring {
        // SAFETY: the test owns `mem` for the ring's lifetime.
        unsafe { Ring::new(mem.as_mut_ptr().cast(), mem.len() * 8) }.unwrap()
    }

    fn text(r: &Ring) -> String {
        let (a, b) = r.contents();
        String::from_utf8([a, b].concat()).unwrap()
    }

    #[test]
    fn garbage_is_not_a_ring() {
        let mut mem = block(64);
        let mut r = ring(&mut mem);
        assert_eq!(r.found(), None);
        r.write(b"ignored");
        assert_eq!(r.found(), None);
    }

    #[test]
    fn too_small_or_misaligned() {
        let mut mem = block(8);
        // SAFETY: as in `ring`.
        assert!(unsafe { Ring::new(mem.as_mut_ptr().cast(), 64) }.is_none());
        // SAFETY: as in `ring`.
        assert!(unsafe { Ring::new(mem.as_mut_ptr().cast::<u8>().add(1), 63) }.is_none());
    }

    #[test]
    fn write_then_recover() {
        let mut mem = block(64);
        {
            let mut r = ring(&mut mem);
            r.reset(7);
            r.write(b"hello\n");
            r.write(b"world\n");
        }
        let r = ring(&mut mem);
        assert_eq!(r.found(), Some(Found { boot: 7, written: 12 }));
        assert_eq!(text(&r), "hello\nworld\n");
    }

    #[test]
    fn wraps_keeping_the_newest() {
        let mut mem = block(8 + 2); // 16 data bytes
        let mut r = ring(&mut mem);
        r.reset(1);
        r.write(b"0123456789");
        r.write(b"abcdefghij");
        assert_eq!(r.capacity(), 16);
        assert_eq!(text(&r), "456789abcdefghij");
        assert_eq!(r.found().unwrap().written, 20);
    }

    #[test]
    fn size_mismatch_or_damage_is_rejected() {
        let mut mem = block(64);
        ring(&mut mem).reset(3);
        {
            // SAFETY: as in `ring`, a smaller view of the same block.
            let smaller = unsafe { Ring::new(mem.as_mut_ptr().cast(), 256) }.unwrap();
            assert_eq!(smaller.found(), None);
        }
        mem[3] ^= 1; // the boot number
        assert_eq!(ring(&mut mem).found(), None);
    }

    #[test]
    fn sites_are_aligned_and_fit() {
        const M: u64 = 1 << 20;
        let v: Vec<u64> = sites(M, 9 * M).collect();
        assert_eq!(v, [2 * M, 4 * M, 6 * M, 8 * M]);
        assert_eq!(sites(M, 9 * M - 1).last(), Some(6 * M));
        assert_eq!(sites(0, 3 * M).collect::<Vec<_>>(), [2 * M]);
        assert_eq!(sites(3 * M, 5 * M).count(), 1);
        assert_eq!(sites(3 * M, 4 * M + 1).count(), 0);
        assert_eq!(sites(u64::MAX - M, u64::MAX).count(), 0);
    }

    #[test]
    fn place_takes_the_highest_site() {
        const M: u64 = 1 << 20;
        let free = [(M, 9 * M), (100 * M, 100 * M + M / 2), (40 * M, 50 * M)];
        assert_eq!(place(&free), Some(48 * M));
        assert!(sites(40 * M, 50 * M).any(|a| a == 48 * M));
        assert_eq!(place(&[(0, M)]), None);
        assert_eq!(place(&[]), None);
    }

    #[test]
    fn reset_empties() {
        let mut mem = block(64);
        let mut r = ring(&mut mem);
        r.reset(1);
        r.write(b"old");
        r.reset(2);
        assert_eq!(text(&r), "");
        assert_eq!(r.found(), Some(Found { boot: 2, written: 0 }));
    }
}
