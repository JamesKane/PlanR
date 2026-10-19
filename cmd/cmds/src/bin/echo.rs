//! `echo [-n] [arg ...]`: the arguments, separated by blanks, and a
//! newline unless `-n`, in one write.

#![no_std]
#![no_main]

extern crate alloc;

r::main!(run);

fn run(args: r::Args) -> Result<(), &'static str> {
    let mut words: alloc::vec::Vec<&[u8]> = args.iter().skip(1).collect();
    let newline = words.first() != Some(&&b"-n"[..]);
    if !newline {
        words.remove(0);
    }
    let mut out = words.join(&b' ');
    if newline {
        out.push(b'\n');
    }
    if let Err(e) = r::write(1, &out) {
        r::eprintln!("echo: write error: {e}");
        return Err("write error");
    }
    Ok(())
}
