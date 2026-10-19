//! `date [-un] [seconds]`: the time now, or `seconds` after the epoch:
//! in the local zone, in GMT (`-u`), or as the number of seconds (`-n`).

#![no_std]
#![no_main]

use r::time;

r::main!(run);

fn run(args: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(args.iter());
    let (mut nflag, mut uflag) = (false, false);
    while let Some(c) = o.next() {
        match c {
            'n' => nflag = true,
            'u' => uflag = true,
            _ => {
                r::eprintln!("usage: date [-un] [seconds]");
                return Err("usage");
            }
        }
    }
    let rest = o.rest();
    #[allow(clippy::cast_possible_wrap)] // a C ulong, read back as signed
    let now = match rest[..] {
        [s] => r::num::strtoul(s, 0).0 as i64,
        _ => time::time(),
    };
    if nflag {
        r::println!("{now}");
    } else if uflag {
        r::print!("{}", time::asctime(&time::gmtime(now)));
    } else {
        r::print!("{}", time::ctime(now));
    }
    Ok(())
}
