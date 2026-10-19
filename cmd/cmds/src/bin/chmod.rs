//! `chmod mode file ...`: sets each file's permissions to `mode`, in
//! octal (`0755`) or as changes, `[ugoa]*[-+=][rwxalt]*`: for the user,
//! group, others or all (all if none is named), take away, add or set
//! read, write and execute permission, append-only, exclusive use and
//! temporary.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use abi::{DMAPPEND, DMEXCL, DMTMP};
use r::{DirBuf, Text};

r::main!(run);

const RWX: u32 = 7;

const fn all(x: u32) -> u32 {
    x << 6 | x << 3 | x
}

/// The bits a symbolic mode changes, and what it sets them to.
fn parse_mode(spec: &[u8]) -> Option<(u32, u32)> {
    let mut mask = DMAPPEND | DMEXCL | DMTMP;
    let mut i = 0;
    loop {
        match spec.get(i)? {
            b'u' => mask |= RWX << 6,
            b'g' => mask |= RWX << 3,
            b'o' => mask |= RWX,
            b'a' => mask |= all(RWX),
            _ => break,
        }
        i += 1;
    }
    if i == 0 {
        mask |= all(RWX);
    }
    let op = spec[i];
    if !matches!(op, b'+' | b'-' | b'=') {
        return None;
    }
    let mut mode = 0;
    for &c in &spec[i + 1..] {
        mode |= match c {
            b'r' => all(4),
            b'w' => all(2),
            b'x' => all(1),
            b'a' => DMAPPEND,
            b'l' => DMEXCL,
            b't' => DMTMP,
            _ => return None,
        };
    }
    if op != b'=' {
        mask &= mode;
    }
    if op == b'-' {
        mode = !mode;
    }
    Some((mask, mode))
}

fn run(args: r::Args) -> Result<(), &'static str> {
    if args.len() < 3 {
        r::eprintln!("usage: chmod 0777 file ... or chmod [who]op[rwxalt] file ...");
        return Err("usage");
    }
    let spec = args.get(1).unwrap_or_default();
    let (octal, n) = r::num::strtoul(spec, 8);
    #[allow(clippy::cast_possible_truncation)] // mode bits
    let parsed = if n == spec.len() {
        Some((all(RWX), octal as u32))
    } else {
        parse_mode(spec)
    };
    let Some((mask, mode)) = parsed else {
        r::eprintln!("chmod: bad mode: {}", Text(spec));
        return Err("mode");
    };
    for f in args.iter().skip(2) {
        let c = r::cstr(f);
        let d = match c.as_ref().map_err(|e| *e).and_then(|c| r::dirstat(c)) {
            Ok(d) => d,
            Err(e) => {
                r::eprintln!("chmod: can't stat {}: {e}", Text(f));
                continue;
            }
        };
        let mut n = DirBuf::null();
        n.mode = (d.mode & !mask) | (mode & mask);
        if let Err(e) = c.and_then(|c| r::dirwstat(&c, &n)) {
            r::eprintln!("chmod: can't wstat {}: {e}", Text(f));
        }
    }
    Ok(())
}
