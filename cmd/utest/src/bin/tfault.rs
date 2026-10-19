//! Faults in the way its argument says; the kernel must end it with a
//! `sys: trap` status. Exits normally only if the fault did not happen.

#![no_std]
#![no_main]

use core::hint::black_box;

r::main!(run);

fn run(args: r::Args) -> Result<(), &'static str> {
    match args.get(1) {
        // SAFETY: none; it is meant to fault.
        Some(b"null") => unsafe {
            black_box(core::ptr::read_volatile(black_box(8usize) as *const u64));
        },
        // SAFETY: none; it is meant to fault.
        Some(b"text") => unsafe {
            core::ptr::write_volatile(
                black_box(run as fn(r::Args) -> Result<(), &'static str> as usize) as *mut u8,
                0,
            );
        },
        Some(b"brk") => {
            let end = r::end_of_bss();
            r::brk(end + 65536).map_err(|_| "brk would not grow")?;
            let p = black_box(end.next_multiple_of(4096) + 8192) as *mut u64;
            // SAFETY: inside the grown bss.
            unsafe { p.write_volatile(1) };
            r::brk(end).map_err(|_| "brk would not shrink")?;
            // SAFETY: none; the page is gone, and this is meant to fault.
            unsafe { p.write_volatile(2) };
        }
        Some(b"stack") => {
            black_box(recurse(0));
        }
        _ => return Err("usage: tfault null|text|brk|stack"),
    }
    Ok(())
}

/// Runs off the bottom of the stack segment.
#[allow(unconditional_recursion)]
fn recurse(n: u64) -> u64 {
    let frame = black_box([n; 512]);
    recurse(n + 1) + frame[1]
}
