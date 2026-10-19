//! rc's syntax tree, and its text as Plan 9's `pcmd.c` prints it: what
//! `whatis` shows of a function and what goes in `/env/fn#name`.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::fmt::{self, Display, Formatter, Write as _};

use crate::lex::RedirKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Word {
    /// Text as written, and whether it was quoted: unquoted, its `*`, `?`
    /// and `[` are patterns.
    Lit(Vec<u8>, bool),
    /// `$w`
    Var(Box<Word>),
    /// `$w(subscripts)`
    Sub(Box<Word>, Vec<Word>),
    /// `$#w`
    Count(Box<Word>),
    /// `$"w`
    Join(Box<Word>),
    /// `a^b`
    Concat(Box<Word>, Box<Word>),
    /// `(words)`
    List(Vec<Word>),
    /// `` `{cmd} ``, or with the characters to split its output at,
    /// `` `split{cmd} ``.
    Backquote(Option<Box<Word>>, Box<Cmd>),
    /// `<{cmd}` or `>{cmd}`: a pipe to the command, named as a file.
    PipeFd(RedirKind, Box<Cmd>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Redir {
    /// `<`, `>` and the rest: descriptor, file.
    File(RedirKind, u32, Word),
    /// `>[fd=from]`
    Dup(u32, u32),
    /// `>[fd=]`
    Close(u32),
    /// `<<tag`: descriptor, document.
    Here(u32, Here),
}

/// A here document: the lines after its command, up to one that is its
/// tag. Its text is read after the command's line, so it is shared with
/// the parser that fills it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Here {
    pub tag: Vec<u8>,
    /// A quoted tag: no `$` substitution in the text.
    pub quoted: bool,
    pub text: Rc<RefCell<Vec<u8>>>,
}

/// A simple command's words and redirections, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    Word(Word),
    Redir(Redir),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Empty,
    Simple(Vec<Arg>),
    /// `{body}`
    Brace(Box<Cmd>),
    /// `a;b` or a newline between them.
    Seq(Box<Cmd>, Box<Cmd>),
    /// `a&`
    Async(Box<Cmd>),
    And(Box<Cmd>, Box<Cmd>),
    Or(Box<Cmd>, Box<Cmd>),
    /// `a|[left=right]b`
    Pipe(Box<Cmd>, u32, u32, Box<Cmd>),
    /// `! cmd`
    Not(Box<Cmd>),
    /// `@ cmd`
    Subshell(Box<Cmd>),
    /// A redirection of a whole command: `>f cmd`, or `{…} >f`.
    Redir(Redir, Box<Cmd>),
    /// `name=value cmd`, or with no command, `name=value`.
    Assign(Word, Word, Box<Cmd>),
    /// `if(cond) cmd`
    If(Box<Cmd>, Box<Cmd>),
    /// `if not cmd`
    IfNot(Box<Cmd>),
    /// `for(name in words) cmd`; without `in`, `None`.
    For(Word, Option<Vec<Word>>, Box<Cmd>),
    /// `while(cond) cmd`
    While(Box<Cmd>, Box<Cmd>),
    /// `switch word {body}`
    Switch(Word, Box<Cmd>),
    /// `~ subject patterns`
    Match(Word, Vec<Word>),
    /// `fn names {body}`, or with no body, `fn names` (which deletes them).
    Fn(Vec<Word>, Option<Box<Cmd>>),
}

impl Cmd {
    /// `a;b`, either of which may be empty (Plan 9's `tree2(';', …)`).
    #[must_use]
    pub fn seq(a: Cmd, b: Cmd) -> Cmd {
        match (a, b) {
            (Cmd::Empty, b) => b,
            (a, Cmd::Empty) => a,
            (a, b) => Cmd::Seq(Box::new(a), Box::new(b)),
        }
    }

    /// Whether this is a simple command whose first word is `case`.
    #[must_use]
    pub fn is_case(&self) -> bool {
        matches!(self, Cmd::Simple(args) if matches!(args.first(),
            Some(Arg::Word(Word::Lit(w, false))) if w == b"case"))
    }
}

/// Writes a quoted word as rc reads it back: in quotes, with each quote
/// doubled.
fn quoted(f: &mut Formatter<'_>, w: &[u8]) -> fmt::Result {
    f.write_char('\'')?;
    for (i, part) in w.split(|&b| b == b'\'').enumerate() {
        if i > 0 {
            f.write_str("''")?;
        }
        bytes(f, part)?;
    }
    f.write_char('\'')
}

/// Writes bytes that should be UTF-8; a bad byte shows as U+FFFD.
fn bytes(f: &mut Formatter<'_>, b: &[u8]) -> fmt::Result {
    f.write_str(&String::from_utf8_lossy(b))
}

fn words(f: &mut Formatter<'_>, ws: &[Word]) -> fmt::Result {
    for (i, w) in ws.iter().enumerate() {
        if i > 0 {
            f.write_char(' ')?;
        }
        write!(f, "{w}")?;
    }
    Ok(())
}

impl Display for Word {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Word::Lit(w, true) => quoted(f, w),
            Word::Lit(w, false) => bytes(f, w),
            Word::Var(w) => write!(f, "${w}"),
            Word::Sub(w, s) => {
                write!(f, "${w}(")?;
                words(f, s)?;
                f.write_char(')')
            }
            Word::Count(w) => write!(f, "$#{w}"),
            Word::Join(w) => write!(f, "$\"{w}"),
            Word::Concat(a, b) => write!(f, "{a}^{b}"),
            Word::List(ws) => {
                f.write_char('(')?;
                words(f, ws)?;
                f.write_char(')')
            }
            Word::Backquote(None, c) => write!(f, "`{{{c}}}"),
            Word::Backquote(Some(split), c) => write!(f, "`{split}{{{c}}}"),
            Word::PipeFd(k, c) => {
                f.write_str(if *k == RedirKind::Read { "<" } else { ">" })?;
                write!(f, "{{{c}}}")
            }
        }
    }
}

impl Display for Redir {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Redir::File(k, fd, w) => {
                let (arrow, std) = match k {
                    RedirKind::Read => ("<", 0),
                    RedirKind::ReadWrite => ("<>", 0),
                    RedirKind::Here => ("<<", 0),
                    RedirKind::Write => (">", 1),
                    RedirKind::Append => (">>", 1),
                };
                f.write_str(arrow)?;
                if *fd != std {
                    write!(f, "[{fd}]")?;
                }
                write!(f, "{w}")
            }
            Redir::Dup(fd, from) => write!(f, ">[{fd}={from}]"),
            Redir::Close(fd) => write!(f, ">[{fd}=]"),
            Redir::Here(fd, h) => {
                f.write_str("<<")?;
                if *fd != 0 {
                    write!(f, "[{fd}]")?;
                }
                if h.quoted { quoted(f, &h.tag) } else { bytes(f, &h.tag) }
            }
        }
    }
}

impl Display for Arg {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Arg::Word(w) => write!(f, "{w}"),
            Arg::Redir(r) => write!(f, "{r}"),
        }
    }
}

impl Display for Cmd {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Cmd::Empty => Ok(()),
            // Plan 9's parser takes the redirections out and wraps them
            // round the command, so they come first, in order, as with
            // `Cmd::Redir`.
            Cmd::Simple(args) => {
                let redirs = args.iter().filter_map(|a| match a {
                    Arg::Redir(r) => Some(r),
                    Arg::Word(_) => None,
                });
                let mut left = args.len();
                for r in redirs {
                    left -= 1;
                    write!(f, "{r}")?;
                    if left > 0 && matches!(r, Redir::File(..) | Redir::Here(..)) {
                        f.write_char(' ')?;
                    }
                }
                let ws = args.iter().filter_map(|a| match a {
                    Arg::Word(w) => Some(w),
                    Arg::Redir(_) => None,
                });
                for (i, w) in ws.enumerate() {
                    if i > 0 {
                        f.write_char(' ')?;
                    }
                    write!(f, "{w}")?;
                }
                Ok(())
            }
            Cmd::Brace(c) => write!(f, "{{{c}}}"),
            Cmd::Seq(a, b) => write!(f, "{a};{b}"),
            Cmd::Async(c) => write!(f, "{c}&"),
            Cmd::And(a, b) => write!(f, "{a} && {b}"),
            Cmd::Or(a, b) => write!(f, "{a} || {b}"),
            Cmd::Pipe(left, lfd, rfd, right) => {
                write!(f, "{left}|")?;
                if *rfd != 0 {
                    write!(f, "[{lfd}={rfd}]")?;
                } else if *lfd != 1 {
                    write!(f, "[{lfd}]")?;
                }
                write!(f, "{right}")
            }
            Cmd::Not(c) => write!(f, "! {c}"),
            Cmd::Subshell(c) => write!(f, "@ {c}"),
            // A dup's command follows it with no blank, as Plan 9 has it.
            Cmd::Redir(r @ (Redir::Dup(..) | Redir::Close(_)), c) => write!(f, "{r}{c}"),
            Cmd::Redir(r, c) => {
                write!(f, "{r}")?;
                if **c != Cmd::Empty {
                    write!(f, " {c}")?;
                }
                Ok(())
            }
            Cmd::Assign(n, v, c) => {
                write!(f, "{n}={v}")?;
                if **c != Cmd::Empty {
                    write!(f, " {c}")?;
                }
                Ok(())
            }
            Cmd::If(cond, c) => write!(f, "if({cond}){c}"),
            Cmd::IfNot(c) => write!(f, "if not {c}"),
            Cmd::For(n, ws, c) => {
                write!(f, "for({n}")?;
                if let Some(ws) = ws {
                    f.write_str(" in ")?;
                    if ws.is_empty() {
                        f.write_str("()")?;
                    }
                    words(f, ws)?;
                }
                write!(f, "){c}")
            }
            Cmd::While(cond, c) => write!(f, "while ({cond}){c}"),
            Cmd::Switch(w, body) => write!(f, "switch {w} {{{body}}}"),
            Cmd::Match(w, ps) => {
                write!(f, "~ {w} ")?;
                words(f, ps)
            }
            Cmd::Fn(names, body) => {
                // Plan 9's "fn %t %t", the second empty for a deletion.
                f.write_str("fn ")?;
                words(f, names)?;
                f.write_char(' ')?;
                match body {
                    Some(b) => write!(f, "{{{b}}}"),
                    None => Ok(()),
                }
            }
        }
    }
}
