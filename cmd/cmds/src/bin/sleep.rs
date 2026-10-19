//! `sleep [seconds]`: waits that many seconds.

#![no_std]
#![no_main]

r::main!(run);

#[allow(clippy::unnecessary_wraps)] // the signature `main!` wants
fn run(args: r::Args) -> Result<(), &'static str> {
    if let Some(a) = args.get(1) {
        for _ in 0..r::num::atol(a).max(0) {
            let _ = r::sleep(1000);
        }
    }
    Ok(())
}
