//! libr's base, in a real process: the heap (collections, a large
//! block, memory reused, two processes sharing the heap with `RFMEM`),
//! `print!` and `bio` through a pipe, and options from real arguments.
//! `/sys/test/run` runs it as `tlibr -v -o out file1 file2`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::{format, vec};

use r::{Bin, Bout, File, RforkFlags as F};

r::main!(run);

type Check = Result<(), &'static str>;

fn ensure(ok: bool, why: &'static str) -> Check {
    if ok { Ok(()) } else { Err(why) }
}

fn run(args: r::Args) -> Result<(), &'static str> {
    options(args)?;
    collections()?;
    large()?;
    reuse()?;
    shared()?;
    pipes()
}

fn options(args: r::Args) -> Check {
    let mut o = r::Opts::new(args.iter());
    ensure(o.argv0() == b"tlibr", "argv0")?;
    let (mut v, mut out) = (false, None);
    while let Some(c) = o.next() {
        match c {
            'v' => v = true,
            'o' => out = o.arg(),
            _ => return Err("an unexpected flag"),
        }
    }
    ensure(v && out == Some(&b"out"[..]), "flags misread")?;
    ensure(o.rest() == [&b"file1"[..], b"file2"], "arguments after the flags")
}

fn collections() -> Check {
    let mut v: Vec<u64> = (0..10_000).collect();
    v.retain(|x| x % 3 == 0);
    ensure(v.len() == 3334 && v[3333] == 9999, "a vector")?;
    let mut m = BTreeMap::new();
    for i in 0..2000u32 {
        m.insert(format!("key{i:05}"), i);
    }
    ensure(m.get("key01234") == Some(&1234) && m.len() == 2000, "a map")?;
    let s: String = m.keys().take(3).map(String::as_str).collect::<Vec<_>>().join(",");
    ensure(s == "key00000,key00001,key00002", "strings")
}

/// A 3 MiB block, written and read back at both ends.
fn large() -> Check {
    let mut big = vec![0u8; 3 << 20];
    big[0] = 1;
    *big.last_mut().ok_or("empty")? = 2;
    ensure(
        big[0] == 1 && big[(3 << 20) - 1] == 2 && big[1 << 20] == 0,
        "a large block",
    )
}

/// Freed memory is used again: a thousand rounds of 100 KiB blocks and
/// small strings leave the break where a few rounds put it.
fn reuse() -> Check {
    let round = || {
        let b = vec![7u8; 100 * 1024];
        let s: Vec<String> = (0..50).map(|i| format!("{i}")).collect();
        b.len() + s.len()
    };
    for _ in 0..5 {
        round();
    }
    let before = r::heap::top();
    for _ in 0..1000 {
        round();
    }
    ensure(r::heap::top() == before, "freed memory was not reused")
}

/// Two processes sharing the heap allocate and free at once.
fn shared() -> Check {
    let churn = |seed: u32| -> Check {
        let mut keep: Vec<Vec<u32>> = Vec::new();
        let mut x = seed;
        for i in 0..3000u32 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            keep.push(vec![i; (x >> 16) as usize % 300 + 1]);
            if x.is_multiple_of(2) {
                let j = (x >> 8) as usize % keep.len();
                let v = keep.swap_remove(j);
                if v.iter().any(|&y| y != v[0]) {
                    return Err("shared heap memory overwritten");
                }
            }
        }
        ensure(
            keep.iter().all(|v| v.iter().all(|&y| y == v[0])),
            "shared heap memory overwritten",
        )
    };
    match r::rfork(F::PROC | F::MEM) {
        Err(_) => return Err("rfork failed"),
        Ok(0) => r::exits(churn(1).err().map(str::as_bytes)),
        Ok(_) => {}
    }
    let mine = churn(2);
    let mut buf = [0u8; 256];
    let n = r::await_child(&mut buf).map_err(|_| "await failed")?;
    mine?;
    ensure(buf[..n].ends_with(b" ''"), "the sharing child failed")
}

/// `print!` to a pipe, read back by `Bin`; `Bout` the other way.
fn pipes() -> Check {
    let [a, b] = r::pipe().map_err(|_| "pipe failed")?;
    let saved = r::dup(1, None).map_err(|_| "dup failed")?;
    r::dup(a, Some(1)).map_err(|_| "dup failed")?;
    r::println!("line {} of {}", 1, 2);
    r::print!("{}\n", r::Quoted(b"it's quoted"));
    r::dup(saved, Some(1)).map_err(|_| "dup failed")?;
    {
        let mut o = Bout::new(File(a));
        for i in 0..500 {
            o.print(format_args!("bout {i}\n")).map_err(|_| "Bout failed")?;
        }
    }
    r::close(a).map_err(|_| "close failed")?;
    r::close(saved).map_err(|_| "close failed")?;
    let mut i = Bin::new(File(b));
    ensure(
        i.line(b'\n') == Ok(Some(&b"line 1 of 2\n"[..])),
        "println through a pipe",
    )?;
    ensure(i.line(b'\n') == Ok(Some(&b"'it''s quoted'\n"[..])), "a quoted print")?;
    for n in 0..500 {
        let want = format!("bout {n}\n");
        ensure(i.line(b'\n') == Ok(Some(want.as_bytes())), "a Bout line")?;
    }
    ensure(i.line(b'\n') == Ok(None), "no end of file")
}
