//! `cleanname [-d pwd] name ...`: each name cleaned lexically, as
//! `cleanname(2)` does; with `-d`, a name not rooted at `/` is taken in
//! directory `pwd` first.

#![no_std]
#![no_main]

extern crate alloc;

use r::Bout;

r::main!(run);

fn usage() -> Result<(), &'static str> {
    r::eprintln!("usage: cleanname [-d pwd] name...");
    Err("usage")
}

fn run(args: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(args.iter());
    let mut dir = None;
    while let Some(c) = o.next() {
        match c {
            'd' => match o.arg() {
                Some(d) => dir = Some(d),
                None => return usage(),
            },
            _ => return usage(),
        }
    }
    let names = o.rest();
    if names.is_empty() {
        return usage();
    }
    let mut out = Bout::new(r::STDOUT);
    for name in names {
        let full = match dir {
            Some(d) if name.first() != Some(&b'/') => [d, b"/", name].concat(),
            _ => name.to_vec(),
        };
        let mut line = r::cleanname(&full);
        line.push(b'\n');
        out.write(&line).map_err(|_| "write error")?;
    }
    out.flush().map_err(|_| "write error")
}
