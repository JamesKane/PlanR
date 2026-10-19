//! `ns [-r] [pid]`: the name space of process `pid` (else this one's), as
//! commands that would build it, as Plan 9's `ns`: one line for each
//! entry `/proc/pid/ns` gives, read one at a time, with names quoted if
//! they hold blanks, quotes, `\`, `#` or `$`. A mount from a network
//! connection is shown by the address at its other end, unless `-r`.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::format;
use alloc::vec::Vec;

use r::{Access, Bout, OpenFlags, OpenMode, Rerr, Text};

r::main!(run);

fn usage() -> ! {
    r::eprintln!("usage: ns [-r] [pid]");
    r::exits(Some(b"usage"))
}

const READ: OpenMode = OpenMode {
    access: Access::Read,
    flags: OpenFlags::empty(),
};

/// An entry: what it does, its flags, what is bound or mounted, where,
/// and the tree asked for.
struct Mount {
    cmd: Vec<u8>,
    flag: Vec<u8>,
    new: Vec<u8>,
    old: Vec<u8>,
    spec: Vec<u8>,
}

fn run(cmdline: r::Args) -> Result<(), &'static str> {
    let mut o = r::Opts::new(cmdline.iter());
    let mut rflag = false;
    while let Some(c) = o.next() {
        match c {
            'r' => rflag = true,
            _ => usage(),
        }
    }
    let argv = o.rest();
    let pid = match argv.as_slice() {
        [] => r::getpid().map_or(0, u64::cast_signed),
        [p] => match r::num::atol(p) {
            0 => usage(),
            n => n,
        },
        _ => usage(),
    };
    let mut out = Bout::new(r::STDOUT);
    let name = format!("/proc/{pid}/ns");
    let Ok(fd) = r::cstr(name.as_bytes()).and_then(|c| r::open(&c, READ)) else {
        r::eprintln!("ns: open {name}: {Rerr}");
        return Err("open ns");
    };
    let mut buf = [0u8; 1024];
    let mut line = 1;
    loop {
        let n = match r::read(fd, &mut buf) {
            Ok(n) if n == buf.len() => {
                r::eprintln!("ns: ns string too long");
                return Err("read ns");
            }
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => {
                r::eprintln!("ns: read {Rerr}");
                return Err("read ns");
            }
        };
        let mut av = r::tokenize(&buf[..n]);
        av.truncate(5);
        let take = |i: usize, av: &mut Vec<Vec<u8>>| core::mem::take(&mut av[i]);
        let mut m = match av.len() {
            2 if av[0] == b"cd" => {
                let _ = out.print(format_args!("{} {}\n", Text(&av[0]), Text(&av[1])));
                line += 1;
                continue;
            }
            5 => Mount {
                cmd: take(0, &mut av),
                flag: take(1, &mut av),
                new: take(2, &mut av),
                old: take(3, &mut av),
                spec: take(4, &mut av),
            },
            4 if av[1].first() == Some(&b'-') => Mount {
                cmd: take(0, &mut av),
                flag: take(1, &mut av),
                new: take(2, &mut av),
                old: take(3, &mut av),
                spec: Vec::new(),
            },
            4 => Mount {
                cmd: take(0, &mut av),
                flag: Vec::new(),
                new: take(1, &mut av),
                old: take(2, &mut av),
                spec: take(3, &mut av),
            },
            3 => Mount {
                cmd: take(0, &mut av),
                flag: Vec::new(),
                new: take(1, &mut av),
                old: take(2, &mut av),
                spec: Vec::new(),
            },
            k => {
                let _ = out.flush();
                r::eprintln!("ns: unrecognized format of ns file: {k} elements on line {line}");
                return Err("format");
            }
        };
        if !rflag && m.cmd == b"mount" {
            xlatemnt(&mut m);
        }
        let _ = out.print(format_args!(
            "{} {} {} {} {}\n",
            Text(&m.cmd),
            Text(&m.flag),
            Text(&quote(&m.new)),
            Text(&quote(&m.old)),
            Text(&quote(&m.spec))
        ));
        line += 1;
    }
    let _ = out.flush();
    Ok(())
}

/// A mount of `/net/proto/n/data` names the address at the other end,
/// from `/net/proto/n/remote`, as `proto!address`.
fn xlatemnt(m: &mut Mount) {
    let Some(rest) = m.new.strip_prefix(b"/net/") else {
        return;
    };
    let mut parts = rest.splitn(3, |&c| c == b'/');
    let (Some(net), Some(port), Some(file)) = (parts.next(), parts.next(), parts.next()) else {
        return;
    };
    if file != b"data" {
        return;
    }
    let name = [&b"/net/"[..], net, b"/", port, b"/remote"].concat();
    let Ok(fd) = r::cstr(&name).and_then(|c| r::open(&c, READ)) else {
        return;
    };
    let mut buf = [0u8; 256];
    let n = r::read(fd, &mut buf);
    let _ = r::close(fd);
    let Ok(mut n) = n else { return };
    if n <= 1 {
        return;
    }
    if buf[n - 1] == b'\n' {
        n -= 1;
    }
    m.new = [net, b"!", &buf[..n]].concat();
}

/// `s` in quotes, with quotes in it doubled, if it holds a blank, a
/// quote, `\`, `#` or `$`; at most about 1 KiB of it, as ns's `quote`.
fn quote(s: &[u8]) -> Vec<u8> {
    if !s.iter().any(|c| b" '\\\t#$".contains(c)) {
        return s.to_vec();
    }
    let mut p = Vec::from(&b"'"[..]);
    let mut s = s.iter();
    while p.len() < 1024 - 5 {
        let Some(&c) = s.next() else { break };
        if c == b'\'' {
            p.push(b'\'');
        }
        p.push(c);
    }
    p.push(b'\'');
    p
}
