//! `basename [-d] string [suffix]`: `string` after its last slash, and
//! without `suffix` if it ends so; with `-d`, what is before the slash
//! (`.` if there is none).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

r::main!(run);

fn run(args: r::Args) -> Result<(), &'static str> {
    let mut words: Vec<&[u8]> = args.iter().skip(1).collect();
    let dflag = words.first() == Some(&&b"-d"[..]);
    if dflag {
        words.remove(0);
    }
    if words.is_empty() || words.len() > 2 {
        r::eprintln!("usage: basename [-d] string [suffix]");
        return Err("usage");
    }
    let s = words[0];
    let slash = s.iter().rposition(|&b| b == b'/');
    let out = if dflag {
        slash.map_or(&b"."[..], |i| &s[..i])
    } else {
        let base = slash.map_or(s, |i| &s[i + 1..]);
        match words.get(1) {
            Some(suffix) if base.ends_with(suffix) => &base[..base.len() - suffix.len()],
            _ => base,
        }
    };
    let mut line = out.to_vec();
    line.push(b'\n');
    r::write(1, &line).map_err(|_| "write error")?;
    Ok(())
}
