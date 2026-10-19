//! The shell's flags, as Plan 9's `getflags` reads them, which `flag`
//! tests and sets: `-c command`, `-m rcmain`, and the letters of
//! [`LETTERS`]. Global, as the input echo (`-v`, `-V`) reads them too.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use alloc::vec::Vec;

/// The flags without an argument, as the usage message lists them.
const LETTERS: &[u8] = b"srdiIlxepvV";

static FLAGS: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];

/// The `.` commands run so far; rcmain is the first.
pub(crate) static NDOT: AtomicU32 = AtomicU32::new(0);

/// Whether `-e` acts yet: not until rcmain runs a script, through `eval`
/// or a second `.`.
pub(crate) static EFLAG_OK: AtomicBool = AtomicBool::new(false);

pub(crate) fn get(c: u8) -> bool {
    FLAGS[usize::from(c)].load(Ordering::Relaxed)
}

pub(crate) fn set(c: u8, on: bool) {
    FLAGS[usize::from(c)].store(on, Ordering::Relaxed);
}

/// The flags' arguments, and the words after them.
#[derive(Debug, Default)]
pub(crate) struct Args {
    pub(crate) command: Option<Vec<u8>>,
    pub(crate) rcmain: Option<Vec<u8>>,
    pub(crate) rest: Vec<Vec<u8>>,
}

/// Reads the flags of `argv` (after its name), up to the first word that
/// is not one, as `getflags(argc, argv, "srdiIlxepvVc:1m:1[command]", 1)`
/// does; on a mistake prints why and the usage, and exits.
pub(crate) fn parse(argv: &[&[u8]]) -> Args {
    let name = argv.first().copied().unwrap_or(b"rc");
    let mut a = Args::default();
    let mut i = 1;
    while let Some(&w) = argv.get(i) {
        if w.first() != Some(&b'-') || w.len() == 1 {
            break;
        }
        i += 1;
        for (k, &c) in w.iter().enumerate().skip(1) {
            if !LETTERS.contains(&c) && c != b'c' && c != b'm' {
                bad(name, &alloc::format!("Illegal flag -{}", char::from(c)));
            }
            if get(c) {
                bad(name, &alloc::format!("Flag -{}: set twice", char::from(c)));
            }
            set(c, true);
            if c == b'c' || c == b'm' {
                let val = if k + 1 < w.len() {
                    w[k + 1..].to_vec()
                } else if let Some(&next) = argv.get(i) {
                    i += 1;
                    next.to_vec()
                } else {
                    bad(name, &alloc::format!("Flag -{}: too few arguments", char::from(c)));
                };
                if c == b'c' {
                    a.command = Some(val);
                } else {
                    a.rcmain = Some(val);
                }
                break;
            }
        }
    }
    a.rest = argv[i.min(argv.len())..].iter().map(|w| w.to_vec()).collect();
    a
}

fn bad(name: &[u8], why: &str) -> ! {
    r::eprintln!(
        "{why}\nUsage: {} [-{}] [-c arg] [-m command] [file [arg ...]]",
        r::print::Text(name),
        r::print::Text(LETTERS)
    );
    r::exits(Some(b"bad flags"))
}
