//! Exits with its arguments, joined by spaces, as the status, so the
//! caller can check what arrived.

#![no_std]
#![no_main]

r::main!(run);

fn run(args: r::Args) -> Result<(), &'static str> {
    let mut b = r::Buf::new();
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            b.put(b" ");
        }
        b.put(a);
    }
    r::exits(Some(b.as_bytes()))
}
