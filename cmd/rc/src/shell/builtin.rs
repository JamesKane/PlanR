//! The builtins: `.`, `builtin` (in [`Shell::command`]), `cd`, `eval`,
//! `exec`, `exit`, `finit`, `flag`, `rfork`, `shift`, `wait` and
//! `whatis`, as Plan 9's.

// Every builtin has the same type, whether or not it can fail.
#![allow(clippy::unnecessary_wraps)]

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use r::RforkFlags as F;

use crate::exec::{Run, Shell, Unwind};
use crate::fd::{FdInput, mode};
use crate::flag;
use crate::var::{List, definition};

type Builtin = fn(&mut Shell, &[Vec<u8>]) -> Run;

const BUILTINS: &[(&[u8], Builtin)] = &[
    (b".", dot),
    (b"cd", cd),
    (b"eval", eval),
    (b"exec", exec),
    (b"exit", exit),
    (b"finit", finit),
    (b"flag", flag),
    (b"rfork", rfork),
    (b"shift", shift),
    (b"wait", wait),
    (b"whatis", whatis),
];

pub(crate) fn find(name: &[u8]) -> Option<Builtin> {
    BUILTINS.iter().find(|(n, _)| *n == name).map(|&(_, b)| b)
}

/// The directories to look for `name` in: none if its name says where it
/// is, else `$path`'s. An empty one means `name` as it is.
pub(crate) fn search_path(sh: &Shell, name: &[u8]) -> List {
    let path = sh.vars.get(b"path");
    if r::env::is_explicit(name) || path.is_empty() {
        alloc::vec![Vec::new()]
    } else {
        path.clone()
    }
}

/// `dir/name`, or `name` for an empty `dir`.
fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() {
        return name.to_vec();
    }
    let mut f = dir.to_vec();
    f.push(b'/');
    f.extend_from_slice(name);
    f
}

fn chdir(dir: &[u8]) -> Result<(), r::Error> {
    r::chdir(&r::cstr(dir)?)
}

/// `cd [dir]`: to `$home`, or `dir`, which unless rooted is looked for in
/// each directory of `$cdpath`; one found there (but `.`) is printed.
fn cd(sh: &mut Shell, a: &[Vec<u8>]) -> Run {
    sh.set_status(b"can't cd");
    match &a[1..] {
        [] => match sh.vars.get(b"home").first().cloned() {
            Some(h) => match chdir(&h) {
                Ok(()) => sh.set_status(b""),
                Err(e) => r::eprintln!("Can't cd {}: {e}", r::print::Text(&h)),
            },
            None => r::eprintln!("Can't cd -- $home empty"),
        },
        [dir] => {
            let mut cdpath = sh.vars.get(b"cdpath").clone();
            if dir.first() == Some(&b'/') || cdpath.is_empty() {
                cdpath = alloc::vec![Vec::new()];
            }
            let mut last = None;
            for p in &cdpath {
                let full = join(p, dir);
                match chdir(&full) {
                    Ok(()) => {
                        if !p.is_empty() && p != b"." {
                            r::eprintln!("{}", r::print::Text(&full));
                        }
                        sh.set_status(b"");
                        return Ok(());
                    }
                    Err(e) => last = Some(e),
                }
            }
            if let Some(e) = last {
                r::eprintln!("Can't cd {}: {e}", r::print::Text(dir));
            }
        }
        _ => r::eprintln!("Usage: cd [directory]"),
    }
    Ok(())
}

/// `. [-i] file [args]`: runs the commands in `file`, found in `$path`,
/// with `$0` set to `file` and `$*` to `args`.
fn dot(sh: &mut Shell, a: &[Vec<u8>]) -> Run {
    let mut a = &a[1..];
    let mut interactive = false;
    if a.first().is_some_and(|w| w == b"-i") {
        interactive = true;
        a = &a[1..];
    }
    let Some((file, args)) = a.split_first() else {
        return sh.error("Usage: . [-i] file [arg ...]");
    };
    let mut fd = Err(r::Error::from_text(b"file does not exist"));
    for dir in search_path(sh, file) {
        fd = r::cstr(&join(&dir, file)).and_then(|f| r::open(&f, mode(r::Access::Read, r::OpenFlags::empty())));
        if fd.is_ok() {
            break;
        }
    }
    let fd = match fd {
        Ok(fd) => fd,
        Err(e) => {
            r::eprint!("{}: ", r::print::Text(file));
            sh.set_status(b"can't open");
            return sh.error_r(".: can't open", &e);
        }
    };
    // Past rcmain, which is the first, `-e` acts.
    if flag::NDOT.fetch_add(1, Ordering::Relaxed) > 0 {
        flag::EFLAG_OK.store(true, Ordering::Relaxed);
    }
    let fd = sh.fds.adopt(fd);
    sh.vars.push_local(b"0", alloc::vec![file.clone()]);
    sh.vars.push_local(b"*", args.to_vec());
    let res = sh.run_input(Box::new(FdInput::new(fd.clone())), Some(file), interactive);
    sh.vars.pop_local();
    sh.vars.pop_local();
    sh.fds.release(&fd);
    res
}

/// `eval args`: runs the arguments, joined by blanks, as commands.
fn eval(sh: &mut Shell, a: &[Vec<u8>]) -> Run {
    if a.len() < 2 {
        return sh.error("Usage: eval cmd ...");
    }
    flag::EFLAG_OK.store(true, Ordering::Relaxed);
    let mut text = a[1..].join(&b' ');
    text.push(b'\n');
    sh.eval_text(&text)
}

/// `exec cmd args`: replaces the shell with `cmd`.
fn exec(sh: &mut Shell, a: &[Vec<u8>]) -> Run {
    if a.len() < 2 {
        return sh.error("empty argument list");
    }
    sh.vars.export();
    sh.exec(&a[1..])
}

/// `exit [status]`
fn exit(sh: &mut Shell, a: &[Vec<u8>]) -> Run {
    if a.len() > 2 {
        r::eprintln!("Usage: exit [status]\nExiting anyway");
    }
    if let Some(s) = a.get(1) {
        sh.set_status(s);
    }
    Err(Unwind::Exit)
}

/// `finit`: defines the functions in `/env`.
fn finit(sh: &mut Shell, _: &[Vec<u8>]) -> Run {
    sh.vars.import_fns();
    Ok(())
}

/// `flag c` sets `$status` to whether flag `c` is set; `flag c +` sets
/// it, and `flag c -` clears it.
fn flag(sh: &mut Shell, a: &[Vec<u8>]) -> Run {
    match &a[1..] {
        [c] => {
            let on = c.first().is_some_and(|&c| flag::get(c));
            sh.set_status(if on { b"" } else { b"flag not set" });
            Ok(())
        }
        [c, how] if c.len() == 1 && (how == b"+" || how == b"-") => {
            flag::set(c[0], how == b"+");
            Ok(())
        }
        _ => sh.error("Usage: flag [letter] [+-]"),
    }
}

/// `rfork [nNeEsfFm]`: gives the shell new groups, as `rfork(2)`; with no
/// argument, `ens`.
fn rfork(sh: &mut Shell, a: &[Vec<u8>]) -> Run {
    let flags = match &a[1..] {
        [] => F::ENVG | F::NAMEG | F::NOTEG,
        [f] => {
            let mut flags = F::empty();
            for c in f {
                flags = flags
                    | match c {
                        b'n' => F::NAMEG,
                        b'N' => F::CNAMEG,
                        b'm' => F::NOMNT,
                        b'e' => F::ENVG,
                        b'E' => F::CENVG,
                        b's' => F::NOTEG,
                        b'f' => F::FDG,
                        b'F' => F::CFDG,
                        _ => return rfork_usage(sh, &a[0]),
                    };
            }
            flags
        }
        _ => return rfork_usage(sh, &a[0]),
    };
    if r::rfork(flags).is_ok() {
        if flags.contains(F::CENVG) {
            sh.vars.mark_all();
        }
        sh.set_status(b"");
    } else {
        r::eprintln!("rc: {} failed", r::print::Text(&a[0]));
        sh.set_status(b"rfork failed");
    }
    Ok(())
}

fn rfork_usage(sh: &mut Shell, name: &[u8]) -> Run {
    r::eprintln!("Usage: {} [fnesFNEm]", r::print::Text(name));
    sh.set_status(b"rfork usage");
    Ok(())
}

/// `shift [n]`: drops the first `n` (1) words of `$*`.
fn shift(sh: &mut Shell, a: &[Vec<u8>]) -> Run {
    let n = match &a[1..] {
        [] => 1,
        [n] => core::str::from_utf8(n).ok().and_then(|s| s.parse().ok()).unwrap_or(0),
        _ => {
            r::eprintln!("Usage: shift [n]");
            sh.set_status(b"shift usage");
            return Ok(());
        }
    };
    let mut star = sh.vars.get(b"*").clone();
    star.drain(..n.min(star.len()));
    sh.vars.set(b"*", star);
    sh.set_status(b"");
    Ok(())
}

/// `wait [pid]`: for a child of `&`, or all of them.
fn wait(sh: &mut Shell, a: &[Vec<u8>]) -> Run {
    match &a[1..] {
        [] => {
            while let Some(&pid) = sh.apids.first() {
                sh.wait_for(pid, false)?;
                if sh.apids.contains(&pid) {
                    // Interrupted.
                    break;
                }
            }
            Ok(())
        }
        [p] => {
            let pid = core::str::from_utf8(p).ok().and_then(|s| s.parse().ok());
            match pid {
                Some(pid) if sh.apids.contains(&pid) => sh.wait_for(pid, false),
                _ => Ok(()),
            }
        }
        _ => sh.error("Usage: wait [pid]"),
    }
}

/// `whatis name…`: each as a variable, a function, a builtin, or a
/// program in `$path`.
fn whatis(sh: &mut Shell, a: &[Vec<u8>]) -> Run {
    if a.len() < 2 {
        return sh.error("Usage: whatis name ...");
    }
    sh.set_status(b"");
    let mut out = Vec::new();
    for name in &a[1..] {
        let val = sh.vars.get(name);
        let found = !val.is_empty();
        if let [w] = &val[..] {
            out.extend(alloc::format!("{}={}\n", r::print::Text(name), r::RcQuoted(w)).bytes());
        } else if found {
            out.extend(alloc::format!("{}=", r::print::Text(name)).bytes());
            for (i, w) in val.iter().enumerate() {
                let sep = if i == 0 { '(' } else { ' ' };
                out.extend(alloc::format!("{sep}{}", r::RcQuoted(w)).bytes());
            }
            out.extend_from_slice(b")\n");
        }
        if let Some(f) = sh.vars.func(name) {
            out.extend(definition(name, &f));
        } else if find(name).is_some() {
            out.extend(alloc::format!("builtin {}\n", r::print::Text(name)).bytes());
        } else if let Some(file) = search_path(sh, name)
            .into_iter()
            .map(|d| join(&d, name))
            .find(|f| executable(f))
        {
            out.extend_from_slice(&file);
            out.push(b'\n');
        } else if !found {
            r::eprintln!("{}: not found", r::print::Text(name));
            sh.set_status(b"not found");
        }
    }
    let _ = r::io::Write::write_all(&mut r::File(1), &out);
    Ok(())
}

/// Whether `file` is a program: a file anyone may execute.
fn executable(file: &[u8]) -> bool {
    r::cstr(file)
        .and_then(|c| r::dirstat(&c))
        .is_ok_and(|d| d.mode & 0o111 != 0 && !d.is_dir())
}
