//! `grep [-bchiLlnsv] [-e pattern] [-f patternfile] [file ...]`: prints
//! the lines of each file, or of standard input, that match the pattern,
//! as Plan 9's `grep` (`main.c`; the pattern machine is the library).
//! Several `-e` patterns, or lines of a pattern or pattern file, are
//! alternatives. `-c` counts the lines instead, `-l` (`-L`) names the
//! files with (without) any, `-s` only exits so; `-v` takes the lines
//! that do not match, `-i` folds case, `-n` numbers the lines, `-h`
//! leaves out the file names a search of several has, and `-b` does not
//! buffer the output (or does, for standard input). The exit status is
//! "no matches" if no line was found.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::vec::Vec;

use grep::{BFLAG, CFLAG, FLSHCNT, Fatal, Grep, HFLAG, IFLAG, LLFLAG, LLLFLAG, NFLAG, SFLAG, VFLAG};
use r::{Access, Bin, Bout, File, OpenFlags, OpenMode, Rerr, Text};

r::main!(run);

const VALIDFLAGS: &str = "bchiLlnsv";

const READ: OpenMode = OpenMode {
    access: Access::Read,
    flags: OpenFlags::empty(),
};

fn usage() -> ! {
    r::eprintln!("usage: grep [-{VALIDFLAGS}] [-e pattern] [-f patternfile] [file ...]");
    r::exits(Some(b"usage"))
}

fn die(f: &Fatal) -> ! {
    r::eprintln!("{}", f.msg);
    r::exits(Some(f.status.as_bytes()))
}

fn run(cmdline: r::Args) -> Result<(), &'static str> {
    let mut g = Grep::new();
    let mut o = r::Opts::new(cmdline.iter());
    while let Some(c) = o.next() {
        match c {
            'e' => {
                g.flags[usize::from(b'e')] += 1;
                g.lineno = 0;
                let p = o.arg().unwrap_or_else(|| usage());
                g.str2top(p).unwrap_or_else(|f| die(&f));
            }
            'f' => {
                let name = o.arg().unwrap_or_else(|| usage());
                let Ok(contents) = r::cstr(name).and_then(|n| read_all(&n)) else {
                    r::eprintln!("grep: can't open {}: {Rerr}", Text(name));
                    r::exits(Some(b"open"));
                };
                g.pattern_file(name, contents);
                g.str2top(name).unwrap_or_else(|f| die(&f));
            }
            c if VALIDFLAGS.contains(c) => g.flags[c as usize] += 1,
            _ => usage(),
        }
    }
    let mut files = o.rest();
    if g.flags[usize::from(b'f')] == 0 && g.flags[usize::from(b'e')] == 0 {
        if files.is_empty() {
            usage();
        }
        let p = files.remove(0);
        g.str2top(p).unwrap_or_else(|f| die(&f));
    }
    g.initstate().unwrap_or_else(|f| die(&f));

    let mut out = Bout::new(r::STDOUT);
    let status = match files.as_slice() {
        [] => search(&mut g, &mut out, None, 0),
        [f] => search(&mut g, &mut out, Some(f), 0),
        _ => {
            let mut status = false;
            for f in &files {
                status |= search(&mut g, &mut out, Some(f), HFLAG);
            }
            status
        }
    };
    if status { Ok(()) } else { Err("no matches") }
}

/// The contents of the pattern file `name`, as `Bgetc` would read them.
fn read_all(name: &core::ffi::CStr) -> Result<Vec<u8>, r::Error> {
    let fd = r::open(name, READ)?;
    let mut b = Bin::new(File(fd));
    let mut v = Vec::new();
    let mut buf = [0; 8192];
    while let Ok(n @ 1..) = b.read(&mut buf) {
        v.extend_from_slice(&buf[..n]);
    }
    let _ = r::close(fd);
    Ok(v)
}

/// The size of each half of C's buffer: the line so far, then a read.
const HALF: usize = 16 * 1024;

/// Searches `file` (standard input if `None`); whether any line was
/// counted.
#[allow(clippy::too_many_lines)] // one loop, as C's
fn search(g: &mut Grep, out: &mut Bout<File>, file: Option<&[u8]>, mut flag: u32) -> bool {
    let (name, fid): (&[u8], _) = match file {
        None => {
            flag |= BFLAG;
            (b"stdin", Ok(0))
        }
        Some(f) => (f, r::cstr(f).and_then(|c| r::open(&c, READ))),
    };
    let Ok(fid) = fid else {
        r::eprintln!("grep: can't open {}: {Rerr}", Text(name));
        return false;
    };
    let on = |c: u8| g.flags[usize::from(c)] > 0;
    if on(b'b') {
        // Don't buffer output.
        flag ^= BFLAG;
    }
    for (c, f) in [
        (b'c', CFLAG),
        (b'i', IFLAG),
        (b'l', LLFLAG),
        (b'L', LLLFLAG),
        (b'n', NFLAG),
        (b's', SFLAG),
        (b'v', VFLAG),
    ] {
        if on(c) {
            flag |= f;
        }
    }
    if on(b'h') {
        flag &= !HFLAG;
    }

    // C's u.pre and u.buf, one after the other.
    let mut buf = alloc::vec![0u8; 2 * HALF];
    let mut state = g.state0;
    let mut lineno: i64 = 0;
    let mut count: i64 = 0;
    let mut eof = false;
    let mut empty = true;
    let mut nl = false;
    let mut lp = HALF;
    let mut bol = lp;
    loop {
        // Keep the line so far before the next read.
        let n = (lp - bol).min(HALF);
        buf.copy_within(bol..bol + n, HALF - n);
        bol = HALF - n;
        let Ok(mut n) = r::read(fid, &mut buf[HALF..]) else {
            r::eprintln!("grep: read error on {}: {Rerr}", Text(name));
            return count != 0;
        };
        // A file without a final newline gets one, for its last line.
        if n > 0 {
            empty = false;
            nl = buf[HALF + n - 1] == b'\n';
        } else if !eof && !nl && !empty {
            buf[HALF] = b'\n';
            n = 1;
            eof = true;
        }
        if n == 0 {
            let _ = r::close(fid);
            if flag & CFLAG != 0 {
                if flag & HFLAG != 0 {
                    let _ = out.print(format_args!("{}:", Text(name)));
                }
                let _ = out.print(format_args!("{count}\n"));
            }
            if (flag & LLFLAG != 0 && count != 0) || (flag & LLLFLAG != 0 && count == 0) {
                let _ = out.print(format_args!("{}\n", Text(name)));
            }
            let _ = out.flush();
            return count != 0;
        }
        lp = HALF;
        let elp = lp + n;
        while lp != elp {
            let mut c = buf[lp];
            if flag & IFLAG != 0 {
                c = c.to_ascii_lowercase();
            }
            state = g.step(state, c).unwrap_or_else(|f| die(&f));
            lp += 1;
            if c != b'\n' {
                continue;
            }
            lineno += 1;
            if g.states[state].matched == (flag & VFLAG == 0) {
                count += 1;
                if flag & (CFLAG | SFLAG | LLFLAG | LLLFLAG) == 0 {
                    if flag & HFLAG != 0 {
                        let _ = out.print(format_args!("{}:", Text(name)));
                    }
                    if flag & NFLAG != 0 {
                        let _ = out.print(format_args!("{lineno}: "));
                    }
                    // Not the newline added at the end, unless the
                    // lines are labelled with the file's name.
                    let cut = usize::from(eof && flag & HFLAG == 0);
                    let _ = out.write(&buf[bol..lp - cut]);
                    if flag & BFLAG != 0 {
                        let _ = out.flush();
                    }
                }
            }
            if lineno & FLSHCNT == 0 {
                let _ = out.flush();
            }
            bol = lp;
        }
    }
}
