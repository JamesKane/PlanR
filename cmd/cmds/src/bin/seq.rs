//! `seq [-fformat] [-w] [first [incr]] last`: the numbers from `first`
//! (1) to `last` by `incr` (1), one to a line, as `%g` prints them, or
//! as `format` (a `print` format for one floating-point value) does;
//! `-w` makes them all as wide, with leading zeros.
//!
//! An argument that starts with `-` and is not `-w` or `-f` ends the
//! options and is a number.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::format;
use alloc::vec::Vec;

use r::Bout;
use r::num::{atof, g, printf_float};

r::main!(run);

fn usage() -> Result<(), &'static str> {
    r::eprintln!("usage: seq [-fformat] [-w] [first [incr]] last");
    Err("usage")
}

/// The format for `-w`: as wide as the widest number, with as many
/// places as the most precise; `%g` if any needs an exponent.
fn constant_format(min: f64, max: f64, incr: f64) -> Vec<u8> {
    let (mut width, mut places) = (0, 0);
    let mut val = min;
    while val <= max {
        let s = g(val);
        if s.contains('e') {
            return b"%g\n".to_vec();
        }
        let (w, p) = s.split_once('.').map_or((s.len(), 0), |(w, p)| (w.len(), p.len()));
        width = width.max(w);
        places = places.max(p);
        val += incr;
    }
    if places > 0 {
        width += places + 1;
    }
    format!("%{width}.{places}f\n").into_bytes()
}

fn run(args: r::Args) -> Result<(), &'static str> {
    let all: Vec<&[u8]> = args.iter().collect();
    let mut constant = false;
    let mut format = None;
    let mut i = 1;
    'options: while let Some(&a) = all.get(i) {
        if a.len() < 2 || a[0] != b'-' {
            break;
        }
        if a == b"--" {
            i += 1;
            break;
        }
        for (j, &c) in a.iter().enumerate().skip(1) {
            match c {
                b'w' => constant = true,
                b'f' => {
                    let f = if j + 1 < a.len() {
                        &a[j + 1..]
                    } else {
                        i += 1;
                        match all.get(i) {
                            Some(f) => f,
                            None => return usage(),
                        }
                    };
                    let mut f = f.to_vec();
                    if f.last() != Some(&b'\n') {
                        f.push(b'\n');
                    }
                    format = Some(f);
                    i += 1;
                    continue 'options;
                }
                _ => break 'options,
            }
        }
        i += 1;
    }
    let nums = &all[i..];
    if nums.is_empty() || nums.len() > 3 {
        return usage();
    }
    let max = atof(nums[nums.len() - 1]);
    let min = if nums.len() > 1 { atof(nums[0]) } else { 1.0 };
    let incr = if nums.len() > 2 { atof(nums[1]) } else { 1.0 };
    if incr == 0.0 {
        r::eprintln!("seq: zero increment");
        return Err("zero increment");
    }
    let format = format.unwrap_or_else(|| {
        if constant {
            constant_format(min, max, incr)
        } else {
            b"%g\n".to_vec()
        }
    });
    let mut out = Bout::new(r::STDOUT);
    let mut val = min;
    while if incr > 0.0 { val <= max } else { val >= max } {
        let mut line = printf_float(&format, val);
        if constant {
            for b in line.iter_mut().take_while(|b| **b == b' ') {
                *b = b'0';
            }
        }
        out.write(&line).map_err(|_| "write error")?;
        val += incr;
    }
    out.flush().map_err(|_| "write error")
}
