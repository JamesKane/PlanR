//! Memory: data loaded from the file, bss and stack pages that appear on
//! first touch, `brk`, and a system call on a page not yet touched.

#![no_std]
#![no_main]

use core::hint::black_box;
use core::ptr::{addr_of, addr_of_mut};

use r::{Sys, brk, end_of_bss};

const PAGE: usize = 4096;

static RODATA: [u64; 4] = [1, 2, 3, 0x0ddc_0ffe_e15b_ad00];
static mut DATA: u64 = 0x5eed_da7a_0000_0001;
static mut BSS: [u8; 3 * PAGE + 100] = [0; 3 * PAGE + 100];
/// Two pages; the second is touched first by the kernel.
static mut UNTOUCHED: [u8; 2 * PAGE] = [0; 2 * PAGE];

r::main!(run);

fn run(_: r::Args) -> Result<(), &'static str> {
    if black_box(RODATA)[3] != 0x0ddc_0ffe_e15b_ad00 {
        return Err("read-only data not loaded");
    }
    // SAFETY: one thread; every access is volatile.
    unsafe {
        if addr_of!(DATA).read_volatile() != 0x5eed_da7a_0000_0001 {
            return Err("data not loaded");
        }
        addr_of_mut!(DATA).write_volatile(7);
        if addr_of!(DATA).read_volatile() != 7 {
            return Err("data not writable");
        }
        let bss = addr_of_mut!(BSS).cast::<u8>();
        for i in (0..3 * PAGE + 100).step_by(512) {
            if bss.add(i).read_volatile() != 0 {
                return Err("bss not zero");
            }
            bss.add(i).write_volatile(0xa5);
        }
    }
    if deep(64) != 64 * 65 / 2 {
        return Err("stack lost a value");
    }
    grow()?;
    untouched_page()
}

/// Recursion with 4 KiB a frame, so the stack runs over many new pages.
fn deep(n: u64) -> u64 {
    let frame = black_box([n; PAGE / 8]);
    if n == 0 { frame[7] } else { frame[0] + deep(n - 1) }
}

/// `brk` grows the bss into new zeroed pages, refuses to move below its
/// start, and shrinks back.
fn grow() -> Result<(), &'static str> {
    let end = end_of_bss();
    let top = end.next_multiple_of(PAGE as u64);
    brk(end + 1024 * 1024).map_err(|_| "brk would not grow")?;
    for va in (top..top + 1024 * 1024).step_by(PAGE) {
        let p = va as *mut u64;
        // SAFETY: inside the bss just grown.
        unsafe {
            if p.read_volatile() != 0 {
                return Err("grown bss not zero");
            }
            p.write_volatile(va);
        }
    }
    if brk(0x1000).is_ok() {
        return Err("brk moved below the bss");
    }
    if brk(u64::MAX - 10).is_ok() {
        return Err("brk accepted a kernel address");
    }
    brk(end).map_err(|_| "brk would not shrink")?;
    brk(end + 8192).map_err(|_| "brk would not grow again")?;
    // SAFETY: inside the bss just grown again.
    if unsafe { (top as *const u64).read_volatile() } != 0 {
        return Err("a page freed by brk came back dirty");
    }
    Ok(())
}

/// `errstr` on a buffer whose page the program never touched: the kernel's
/// copy faults it in.
fn untouched_page() -> Result<(), &'static str> {
    // SAFETY: an unknown call does nothing.
    if unsafe { r::raw_syscall(9999, [0; 6]) }.is_ok() {
        return Err("an unknown call succeeded");
    }
    // SAFETY: `buf` is ours, and its page has never been touched.
    let buf = unsafe { addr_of_mut!(UNTOUCHED).cast::<u8>().add(PAGE) };
    // SAFETY: as above; the kernel writes at most 128 bytes.
    if unsafe { r::syscall(Sys::Errstr, [buf as u64, 128, 0, 0, 0, 0]) }.is_err() {
        return Err("errstr on an untouched page failed");
    }
    // SAFETY: the kernel wrote a NUL-terminated string there.
    let text = unsafe { core::ffi::CStr::from_ptr(buf.cast()) };
    if text.to_bytes() != b"bad system call" {
        return Err("errstr returned the wrong text");
    }
    Ok(())
}
