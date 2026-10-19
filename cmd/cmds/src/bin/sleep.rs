//! `sleep [time]`: waits `time` seconds (Plan 9's `sleep.c`): a whole
//! number in C's bases, then perhaps a point and up to three decimal
//! places, done without floating point.

#![no_std]
#![no_main]

r::main!(run);

#[allow(clippy::unnecessary_wraps)] // the signature `main!` wants
fn run(args: r::Args) -> Result<(), &'static str> {
    let Some(a) = args.get(1) else { return Ok(()) };
    let (secs, k) = r::num::strtol(a, 0);
    for _ in 0..secs.max(0) {
        let _ = r::sleep(1000);
    }
    if a.get(k) == Some(&b'.') {
        let p = &a[k + 1..];
        let (mut n, len) = r::num::strtol(p, 10);
        if n > 0 {
            match len {
                1 => n *= 100,
                2 => n *= 10,
                3 => {}
                _ => n = r::num::strtol(&p[..3.min(p.len())], 10).0,
            }
            #[allow(clippy::cast_sign_loss)] // positive
            let _ = r::sleep(n as u64);
        }
    }
    Ok(())
}
