//! Options, as Plan 9's `ARGBEGIN`/`ARGEND` read them: after the program's
//! name, each argument that starts with `-` and has more is a run of
//! one-rune flags, until an argument that does not, or `--` (which is
//! taken away). A flag that wants a value takes the rest of its argument,
//! or else the next argument ([`Opts::arg`], Plan 9's `ARGF`).
//!
//! ```ignore
//! let mut o = r::Opts::new(args.iter());
//! while let Some(c) = o.next() {
//!     match c {
//!         'n' => n = true,
//!         'o' => out = o.arg().ok_or_else(usage)?,
//!         _ => return Err(usage()),
//!     }
//! }
//! let files = o.rest();
//! ```

use alloc::vec::Vec;

use crate::utf;

/// The options of an argument list; see the module.
#[derive(Debug)]
pub struct Opts<'a> {
    args: Vec<&'a [u8]>,
    /// The argument being read, and where in it.
    i: usize,
    at: usize,
    done: bool,
}

impl<'a> Opts<'a> {
    /// The options of `args`, the first of which is the program's name.
    pub fn new(args: impl IntoIterator<Item = &'a [u8]>) -> Opts<'a> {
        Opts {
            args: args.into_iter().collect(),
            i: 1,
            at: 0,
            done: false,
        }
    }

    /// The program's name, `argv0`.
    #[must_use]
    pub fn argv0(&self) -> &'a [u8] {
        self.args.first().copied().unwrap_or(b"")
    }

    /// The next flag; `None` once the options are over.
    #[allow(clippy::should_implement_trait)] // it borrows the value-taking `arg` too
    pub fn next(&mut self) -> Option<char> {
        if self.done {
            return None;
        }
        if self.at == 0 {
            let a = *self.args.get(self.i)?;
            if a == b"--" {
                self.i += 1;
                self.done = true;
                return None;
            }
            if a.len() < 2 || a[0] != b'-' {
                self.done = true;
                return None;
            }
            self.at = 1;
        }
        let a = self.args[self.i];
        let (c, n) = utf::decode(&a[self.at..]);
        self.at += n;
        if self.at >= a.len() {
            self.i += 1;
            self.at = 0;
        }
        Some(c)
    }

    /// The value of the flag just read: the rest of its argument, or else
    /// the next argument; `None` if there is neither (Plan 9's `ARGF`).
    pub fn arg(&mut self) -> Option<&'a [u8]> {
        let a = *self.args.get(self.i)?;
        let v = if self.at > 0 { &a[self.at..] } else { a };
        self.i += 1;
        self.at = 0;
        Some(v)
    }

    /// The arguments after the options.
    #[must_use]
    pub fn rest(mut self) -> Vec<&'a [u8]> {
        while self.next().is_some() {}
        self.args.split_off(self.i.min(self.args.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&'static str]) -> (Vec<(char, Option<&'static str>)>, Vec<&'static str>) {
        let mut o = Opts::new(args.iter().map(|a| a.as_bytes()));
        let mut flags = Vec::new();
        while let Some(c) = o.next() {
            let v = matches!(c, 'o' | 'é').then(|| o.arg()).flatten();
            flags.push((c, v.map(|v| core::str::from_utf8(v).unwrap())));
        }
        let rest = o.rest().into_iter().map(|a| core::str::from_utf8(a).unwrap()).collect();
        (flags, rest)
    }

    #[test]
    fn flags_and_values() {
        let (f, r) = parse(&["cmd", "-ab", "-o", "out", "-ofile", "x", "-c"]);
        assert_eq!(f, [('a', None), ('b', None), ('o', Some("out")), ('o', Some("file"))]);
        assert_eq!(r, ["x", "-c"]);
    }

    #[test]
    fn ends() {
        assert_eq!(parse(&["cmd", "-a", "--", "-b"]), (vec![('a', None)], vec!["-b"]));
        assert_eq!(parse(&["cmd", "-", "-a"]), (vec![], vec!["-", "-a"]));
        assert_eq!(parse(&["cmd"]), (vec![], vec![]));
        assert_eq!(parse(&["cmd", "-o"]), (vec![('o', None)], vec![]));
    }

    #[test]
    fn runes_as_flags() {
        let (f, r) = parse(&["cmd", "-xé", "v", "rest"]);
        assert_eq!(f, [('x', None), ('é', Some("v"))]);
        assert_eq!(r, ["rest"]);
    }

    #[test]
    fn argv0() {
        let o = Opts::new([&b"/bin/ls"[..]]);
        assert_eq!(o.argv0(), b"/bin/ls");
    }
}
