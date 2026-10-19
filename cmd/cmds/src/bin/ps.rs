//! `ps [-apr]`: the processes in `/proc`, by pid, as Plan 9's `ps`: a line
//! each of user, pid, user and system time, memory, state and name. `-a`
//! shows the arguments in place of the name, `-p` the base and current
//! priorities, `-r` the real time too. Other flags are ignored.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use r::{Access, Bout, File, OpenFlags, OpenMode, Text};

r::main!(run);

const READ: OpenMode = OpenMode {
    access: Access::Read,
    flags: OpenFlags::empty(),
};

#[derive(Clone, Copy)]
struct Flags {
    a: bool,
    p: bool,
    r: bool,
}

fn error(s: &str) -> ! {
    r::eprint!("ps: {s}: ");
    r::perror(b"error");
    r::exits(Some(s.as_bytes()))
}

fn run(cmdline: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(cmdline.iter());
    let mut f = Flags {
        a: false,
        p: false,
        r: false,
    };
    while let Some(c) = o.next() {
        match c {
            'a' => f.a = true,
            'p' => f.p = true,
            'r' => f.r = true,
            _ => {}
        }
    }
    let mut out = Bout::new(r::STDOUT);
    if r::chdir(c"/proc").is_err() {
        error("/proc");
    }
    let Ok(fd) = r::open(c".", READ) else { error("/proc") };
    let mut dir = r::dirreadall(fd).unwrap_or_default();
    let _ = r::close(fd);
    if dir.is_empty() {
        r::eprintln!("ps: empty directory /proc");
        return Err("empty");
    }
    dir.sort_by_key(|d| r::num::atol(d.name.as_bytes()));
    for d in &dir {
        ps(&mut out, f, d.name.as_bytes());
    }
    let _ = out.flush();
    Ok(())
}

/// The line for process `s`, if its status can be read.
fn ps(out: &mut Bout<File>, f: Flags, s: &[u8]) {
    let mut status = [0u8; 4096];
    let Some(n) = read_file(&[s, b"/status"].concat(), &mut status[..4095]) else {
        return;
    };
    if n == 0 {
        return;
    }
    let mut argv = r::tokenize(&status[..n]);
    argv.truncate(15);
    if argv.len() < 12 {
        return;
    }
    // 0 name, 1 user, 2 state, 3-8 times, 9 memory, 10 base priority, 11
    // priority.
    let num = |i: usize| r::num::strtoul(&argv[i], 0).0;
    let (utime, stime, rtime, size) = (num(3) / 1000, num(4) / 1000, num(5) / 1000, num(9));
    let pbuf = if f.p {
        format!(" {:2} {:2}", num(10), num(11))
    } else {
        String::new()
    };
    let rbuf = if f.r {
        let t = if rtime >= 86400 {
            format!(
                " {}:{:02}:{:02}:{:02}",
                rtime / 86400,
                (rtime / 3600) % 24,
                (rtime / 60) % 60,
                rtime % 60
            )
        } else if rtime >= 3600 {
            format!(" {}:{:02}:{:02}", rtime / 3600, (rtime / 60) % 60, rtime % 60)
        } else {
            format!(" {}:{:02}", rtime / 60, rtime % 60)
        };
        format!("{t:>12}")
    } else {
        String::new()
    };
    let state: String = String::from_utf8_lossy(&argv[2]).chars().take(8).collect();
    let _ = out.print(format_args!(
        "{:<10} {:>8}{} {:4}:{:02} {:3}:{:02} {} {:7}K {:<8} ",
        Text(&argv[1]),
        Text(s),
        rbuf,
        utime / 60,
        utime % 60,
        stime / 60,
        stime % 60,
        pbuf,
        size,
        state
    ));
    if !f.a {
        let _ = out.print(format_args!("{}\n", Text(&argv[0])));
        return;
    }
    let mut text = [0u8; 256];
    match read_file(&[s, b"/args"].concat(), &mut text[..255]) {
        None => {
            let _ = out.print(format_args!("{} ?\n", Text(&argv[0])));
        }
        Some(0) => {
            let _ = out.print(format_args!("{}\n", Text(&argv[0])));
        }
        Some(n) => {
            let a: Vec<u8> = text[..n].iter().map(|&c| if c == b'\n' { b' ' } else { c }).collect();
            let _ = out.print(format_args!("{}\n", Text(&a)));
        }
    }
}

/// One read of file `name`, as ps makes it.
fn read_file(name: &[u8], buf: &mut [u8]) -> Option<usize> {
    let fd = r::cstr(name).and_then(|c| r::open(&c, READ)).ok()?;
    let n = r::read(fd, buf);
    let _ = r::close(fd);
    n.ok()
}
