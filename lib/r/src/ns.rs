//! Name spaces from a file, as Plan 9's `newns(2)` (`libauth/newns.c`):
//! [`newns`] builds a new one from `/lib/namespace` or another file, and
//! [`addns`] adds a file's commands to the one there is.
//!
//! The file holds one command a line: `bind`, `mount`, `unmount`,
//! `import`, `cd`, `clear` and `.` (to read another file), with `bind`'s
//! flags. A line starting `#` (after blanks) is a comment. Words are
//! split at blanks and quoted as rc quotes them, and `$name` outside
//! quotes is the first word of that variable, read from `#e`. Errors are
//! ignored; with [`set_debug`], the commands and their failures are shown
//! on standard error. There is no factotum yet, so no mount is
//! authenticated, and no network, so `import` does nothing.

use alloc::ffi::CString;
use alloc::vec::Vec;
use core::ffi::CStr;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::bio::{BSIZE, Bin};
use crate::io::File;
use crate::{Access, BindFlags, Error, OpenFlags, OpenMode, RforkFlags, Text};

/// The most words a line may have, less one (Plan 9's `NARG`).
const NARG: usize = 15;
/// The longest variable name, plus one (Plan 9's `ANAMELEN`).
const ANAMELEN: usize = 28;
/// The longest argument, once expanded (Plan 9's `MAXARG`).
const MAXARG: usize = 10 * ANAMELEN;

static DEBUG: AtomicBool = AtomicBool::new(false);

/// Shows each command, and each failed `bind` and `mount`, on standard
/// error (Plan 9's `newnsdebug`).
pub fn set_debug(on: bool) {
    DEBUG.store(on, Ordering::Relaxed);
}

fn debug() -> bool {
    DEBUG.load(Ordering::Relaxed)
}

const READ: OpenMode = OpenMode {
    access: Access::Read,
    flags: OpenFlags::empty(),
};

const RDWR: OpenMode = OpenMode {
    access: Access::ReadWrite,
    flags: OpenFlags::empty(),
};

/// Builds a new name space for `user` from `file` (`/lib/namespace` if
/// `None`): with a new copy of the environment holding `user` and `home`
/// (`/usr/user`), from nothing but the root device. If the file does not
/// `cd` to a rooted name, the current directory is found again in the new
/// name space, else `/`.
pub fn newns(user: &[u8], file: Option<&CStr>) -> Result<(), Error> {
    buildns(true, user, file)
}

/// Adds the commands of `file` to the name space there is.
pub fn addns(user: &[u8], file: &CStr) -> Result<(), Error> {
    buildns(false, user, Some(file))
}

fn buildns(new: bool, user: &[u8], file: Option<&CStr>) -> Result<(), Error> {
    let file = file.unwrap_or(c"/lib/namespace");
    let fd = crate::open(file, READ).map_err(|e| {
        let mut t = Vec::from(&b"can't open "[..]);
        t.extend_from_slice(file.to_bytes());
        t.extend_from_slice(b": ");
        t.extend_from_slice(e.as_bytes());
        Error::from_text(&t)
    })?;
    if new {
        let _ = crate::rfork(RforkFlags::ENVG | RforkFlags::CNAMEG);
        setenv(b"user", user);
        let mut home = Vec::from(&b"/usr/"[..]);
        home.extend_from_slice(user);
        setenv(b"home", &home);
    }
    let mut b = Bin::new(File(fd));
    let cdroot = nsfile(if new { "newns" } else { "addns" }, &mut b);
    let _ = crate::close(fd);
    // Make sure we got into the new name space.
    if new && !cdroot {
        let mut path = [0u8; 1024];
        let back = crate::open(c".", READ).ok().and_then(|d| {
            let n = crate::fd2path(d, &mut path);
            let _ = crate::close(d);
            n.ok()
        });
        let ok = back.is_some_and(|n| CString::new(&path[..n]).is_ok_and(|p| crate::chdir(&p).is_ok()));
        if !ok {
            let _ = crate::chdir(c"/");
        }
    }
    Ok(())
}

/// Runs the commands read from `b`; true if one was a `cd` to a rooted
/// name. Reading stops, as Plan 9's `Brdline` does, at a line too long
/// for the buffer or a last line with no newline.
fn nsfile(f: &str, b: &mut Bin<File>) -> bool {
    let mut cdroot = false;
    while let Ok(Some(line)) = b.line(b'\n') {
        if line.len() > BSIZE || !line.ends_with(b"\n") {
            break;
        }
        let line = &line[..line.len() - 1];
        let start = line.iter().position(|&c| c != b' ' && c != b'\t').unwrap_or(line.len());
        let cmd = &line[start..];
        if cmd.first() == Some(&b'#') {
            continue;
        }
        let args = splitargs(cmd);
        if !args.is_empty() {
            cdroot |= nsop(f, &args);
        }
    }
    cdroot
}

/// The words of `p`, expanded and unquoted; none if there are too many,
/// or one is too long.
fn splitargs(p: &[u8]) -> Vec<Vec<u8>> {
    let words = crate::gettokens(p, " \t\r");
    if words.len() >= NARG {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(words.len());
    for w in words {
        let Some(mut a) = expandarg(w) else { return Vec::new() };
        unquote(&mut a);
        out.push(a);
    }
    out
}

/// Takes the quotes out of `s`: a quote opens or closes a quoted part,
/// and `''` inside one is a quote (newns's own `unquote`).
fn unquote(s: &mut Vec<u8>) {
    let mut out = Vec::with_capacity(s.len());
    let mut quoting = false;
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c != b'\'' {
            out.push(c);
        } else if !quoting {
            quoting = true;
        } else if s.get(i + 1) == Some(&b'\'') {
            out.push(b'\'');
            i += 1;
        } else {
            quoting = false;
        }
        i += 1;
    }
    *s = out;
}

/// Where the next `$` outside quotes is in `arg`.
fn nextdollar(arg: &[u8]) -> Option<usize> {
    let mut quoting = false;
    for (i, &c) in arg.iter().enumerate() {
        if c == b'\'' {
            quoting = !quoting;
        }
        if c == b'$' && !quoting {
            return Some(i);
        }
    }
    None
}

/// `arg` with each `$name` outside quotes replaced by the variable's
/// first word (at most `ANAMELEN - 1` bytes of it). The name runs to the
/// next of `/.!'$`; an empty name or one too long is dropped, `$` and
/// all. `None` if the result would be too long.
fn expandarg(mut arg: &[u8]) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    while let Some(d) = nextdollar(arg) {
        if buf.len() + d + ANAMELEN >= MAXARG - 1 {
            return None;
        }
        buf.extend_from_slice(&arg[..d]);
        let p = &arg[d + 1..];
        let len = p.iter().position(|c| b"/.!'$".contains(c)).unwrap_or(p.len());
        arg = &p[len..];
        if len == 0 || len >= ANAMELEN {
            continue;
        }
        let mut name = Vec::from(&b"#e/"[..]);
        name.extend_from_slice(&p[..len]);
        let Ok(name) = CString::new(name) else { continue };
        if let Ok(fd) = crate::open(&name, READ) {
            let mut v = [0u8; ANAMELEN - 1];
            if let Ok(n) = crate::read(fd, &mut v) {
                // A list's words are ended by NULs: take the first.
                let n = v[..n].iter().position(|&c| c == 0).unwrap_or(n);
                buf.extend_from_slice(&v[..n]);
            }
            let _ = crate::close(fd);
        }
    }
    if buf.len() + arg.len() >= MAXARG - 1 {
        return None;
    }
    buf.extend_from_slice(arg);
    Some(buf)
}

/// Runs one command, `argv`; true if it was a `cd` to a rooted name.
fn nsop(f: &str, argv: &[Vec<u8>]) -> bool {
    if debug() {
        for a in argv {
            crate::eprint!("{} ", Text(a));
        }
        crate::eprintln!();
    }
    // ARGBEGIN, with no default: other flags are ignored.
    let mut flags = BindFlags::empty();
    let mut i = 1;
    while i < argv.len() && argv[i].len() > 1 && argv[i][0] == b'-' {
        if argv[i] == b"--" {
            i += 1;
            break;
        }
        for &c in &argv[i][1..] {
            match c {
                b'a' => flags = flags | BindFlags::AFTER,
                b'b' => flags = flags | BindFlags::BEFORE,
                b'c' => flags = flags | BindFlags::CREATE,
                b'C' => flags = flags | BindFlags::CACHE,
                _ => {}
            }
        }
        i += 1;
    }
    let cmd = argv[0].as_slice();
    let words: Vec<CString> = argv[i..]
        .iter()
        .filter_map(|a| CString::new(a.as_slice()).ok())
        .collect();
    if words.len() != argv.len() - i {
        return false;
    }
    let a: Vec<&CStr> = words.iter().map(CString::as_c_str).collect();
    match (cmd, a.as_slice()) {
        (b".", [file]) => {
            let Ok(fd) = crate::open(file, READ) else { return false };
            let mut b = Bin::new(File(fd));
            let cdroot = nsfile(f, &mut b);
            let _ = crate::close(fd);
            return cdroot;
        }
        (b"clear", []) => {
            let _ = crate::rfork(RforkFlags::CNAMEG);
        }
        (b"bind", [new, old]) => {
            if let Err(e) = crate::bind(new, old, flags)
                && debug()
            {
                crate::eprintln!("{f}: bind: {} {}: {e}", Text(new.to_bytes()), Text(old.to_bytes()));
            }
        }
        (b"unmount", [old]) => {
            let _ = crate::unmount(None, old);
        }
        (b"unmount", [new, old]) => {
            let _ = crate::unmount(Some(new), old);
        }
        (b"mount", [srv, old, spec @ ..]) if spec.len() <= 1 => {
            let fd = crate::open(srv, RDWR);
            let spec = spec.first().copied().unwrap_or(c"");
            let r = match fd {
                Ok(fd) => famount(fd, old, flags, spec),
                Err(e) => Err(e),
            };
            if let Err(e) = r
                && debug()
            {
                crate::eprint!("{f}: mount: {} {}", Text(srv.to_bytes()), Text(old.to_bytes()));
                if !spec.is_empty() {
                    crate::eprint!(" {}", Text(spec.to_bytes()));
                }
                crate::eprintln!(": {e}");
            }
            if let Ok(fd) = fd {
                let _ = crate::close(fd);
            }
        }
        (b"cd", [dir]) if crate::chdir(dir).is_ok() => return dir.to_bytes().first() == Some(&b'/'),
        // `import` dials the system named: there is no network yet, so
        // the dial fails, as does the mount of what it would give.
        _ => {}
    }
    false
}

/// Mounts the server on `fd`, authenticating if it asks (there is no
/// factotum yet to answer it, so the mount goes ahead unauthenticated).
fn famount(fd: crate::Fd, old: &CStr, flags: BindFlags, aname: &CStr) -> Result<(), Error> {
    let afd = crate::fauth(fd, aname).ok();
    let r = crate::mount(fd, afd, old, flags, aname);
    if let Some(a) = afd {
        let _ = crate::close(a);
    }
    r
}

/// Writes `val` to `#e/name`, made anew.
fn setenv(name: &[u8], val: &[u8]) {
    let mut n = Vec::from(&b"#e/"[..]);
    n.extend_from_slice(name);
    let Ok(n) = CString::new(n) else { return };
    let mode = OpenMode {
        access: Access::Write,
        flags: OpenFlags::empty(),
    };
    if let Ok(fd) = crate::create(&n, mode, 0o664) {
        let _ = crate::write(fd, val);
        let _ = crate::close(fd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquotes_as_newns_does() {
        for (from, to) in [
            (&b"plain"[..], &b"plain"[..]),
            (b"'a b'", b"a b"),
            (b"'it''s'", b"it's"),
            (b"a'b'c", b"abc"),
            (b"''", b""),
        ] {
            let mut v = from.to_vec();
            unquote(&mut v);
            assert_eq!(v, to);
        }
    }

    #[test]
    fn finds_dollars_outside_quotes() {
        assert_eq!(nextdollar(b"/a/$b"), Some(3));
        assert_eq!(nextdollar(b"'$a'$b"), Some(4));
        assert_eq!(nextdollar(b"none"), None);
    }

    #[test]
    fn drops_empty_and_long_names() {
        // No `#e` on the host: every variable reads as empty.
        assert_eq!(expandarg(b"/a/$/b").unwrap(), b"/a//b");
        assert_eq!(expandarg(b"x$.y").unwrap(), b"x.y");
        let long = [b'n'; ANAMELEN];
        let mut arg = Vec::from(&b"/$"[..]);
        arg.extend_from_slice(&long);
        arg.extend_from_slice(b"/z");
        assert_eq!(expandarg(&arg).unwrap(), b"//z");
        assert_eq!(expandarg(b"'$x'").unwrap(), b"'$x'");
        assert!(expandarg(&[b'a'; MAXARG]).is_none());
    }

    #[test]
    fn splits_at_most_narg_words() {
        assert_eq!(splitargs(b"bind -a '#c' /dev").len(), 4);
        let many = b"a b c d e f g h i j k l m n o";
        assert_eq!(splitargs(many), Vec::<Vec<u8>>::new());
        assert_eq!(splitargs(&many[..many.len() - 2]).len(), 14);
    }
}
