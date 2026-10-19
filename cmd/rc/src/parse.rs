//! A recursive-descent parser for rc's grammar (Plan 9's `syn.y`), with
//! its precedences: `if`, `for`, `while` and `switch` take as much as
//! follows; then `&&` and `||`; then `!`, `@` and prefix redirections and
//! assignments; then `|`. Input is read and parsed a line at a time, as
//! rc runs it, and the text of the line's here documents is read after
//! it.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::lex::{Input, Keyword, Lexer, RedirKind, SyntaxError, Tok};
use crate::tree::{Arg, Cmd, Here, Redir, Word};

/// How tightly an operator binds.
const ANDOR: u8 = 1;
const UNARY: u8 = 2;
const PIPE: u8 = 3;

#[derive(Debug)]
pub struct Parser {
    pub lex: Lexer,
    peeked: Option<Tok>,
    /// The last command compiled was an `if`, so an `if not` may follow
    /// (Plan 9's `iflast`, kept from line to line).
    iflast: bool,
    /// The here documents of the line being read, to be read after it.
    here: Vec<Here>,
}

type Result<T> = core::result::Result<T, SyntaxError>;

impl Parser {
    #[must_use]
    pub fn new(input: Box<dyn Input>) -> Parser {
        Parser {
            lex: Lexer::new(input),
            peeked: None,
            iflast: false,
            here: Vec::new(),
        }
    }

    fn peek(&mut self) -> Result<&Tok> {
        if self.peeked.is_none() {
            self.peeked = Some(self.lex.lex()?);
        }
        Ok(self.peeked.as_ref().unwrap_or(&Tok::Eof))
    }

    fn next(&mut self) -> Result<Tok> {
        self.peek()?;
        Ok(self.peeked.take().unwrap_or(Tok::Eof))
    }

    fn error<T>(&self, msg: &'static str) -> Result<T> {
        Err(SyntaxError {
            msg,
            token: self.lex.token.clone(),
            line: self.lex.lineno,
            twice: false,
        })
    }

    fn expect(&mut self, want: &Tok) -> Result<()> {
        let t = self.next()?;
        if t == *want {
            return Ok(());
        }
        self.peeked = Some(t);
        self.error("syntax error")
    }

    /// Skips newlines where the grammar allows them; nothing may have been
    /// read ahead.
    fn skipnl(&mut self) {
        debug_assert!(self.peeked.is_none());
        self.lex.skipnl();
    }

    /// Reads one line of commands: `None` at the end of the input. After
    /// an error the rest of the line is skipped, so the next call starts
    /// afresh.
    pub fn line(&mut self) -> Result<Option<Cmd>> {
        self.line_with(&|l| l.to_vec())
    }

    /// [`Parser::line`], with `subst` making each line of a here document
    /// whose tag is not quoted what goes in its text.
    pub fn line_with(&mut self, subst: &dyn Fn(&[u8]) -> Vec<u8>) -> Result<Option<Cmd>> {
        let r = self.line_inner();
        if let Err(e) = &r {
            self.here.clear();
            let at_end = self.peeked.as_ref().is_some_and(|t| *t == Tok::Newline);
            self.peeked = None;
            if !at_end && !self.lex.eof {
                self.lex.skipline();
            }
            return Err(e.clone());
        }
        for h in core::mem::take(&mut self.here) {
            self.read_here(&h, subst);
        }
        self.lex.here_done();
        r
    }

    /// Whether the end of the input has been read.
    #[must_use]
    pub fn at_eof(&self) -> bool {
        self.lex.eof
    }

    /// Reads on after the end of the input (an interrupted read of a
    /// terminal), from the start of a line.
    pub fn clear_eof(&mut self) {
        self.peeked = None;
        self.lex.clear_eof();
    }

    /// Reads the text of `h`: whole lines up to its tag, or to the end
    /// of the input, where a last line with no newline is dropped.
    fn read_here(&mut self, h: &Here, subst: &dyn Fn(&[u8]) -> Vec<u8>) {
        let mut text = Vec::new();
        let mut line = Vec::new();
        // Plan 9's readhere counts a line for each, and for each newline
        // but the tag's.
        self.lex.here_line();
        while let Some(c) = self.lex.raw() {
            if c != b'\n' {
                line.push(c);
                continue;
            }
            if line == h.tag {
                break;
            }
            if h.quoted {
                text.extend_from_slice(&line);
            } else {
                text.extend(subst(&line));
            }
            self.lex.here_line();
            text.push(b'\n');
            line.clear();
        }
        *h.text.borrow_mut() = text;
    }

    fn line_inner(&mut self) -> Result<Option<Cmd>> {
        if *self.peek()? == Tok::Eof {
            return Ok(None);
        }
        let c = self.list(&Tok::Newline)?;
        let mut iflast = self.iflast;
        let r = check_ifnot(&c, &mut iflast);
        self.iflast = iflast;
        r.map_err(|msg| SyntaxError {
            msg,
            token: Vec::new(),
            line: self.lex.lineno,
            twice: false,
        })?;
        Ok(Some(c))
    }

    /// Commands ended by `;`, `&` or newlines, up to `end` (a newline or
    /// the end of the input for a line, `}` or `)` for a body), which is
    /// read too.
    fn list(&mut self, end: &Tok) -> Result<Cmd> {
        let mut cmds = Vec::new();
        loop {
            let c = self.cmd(0)?;
            let t = self.next()?;
            match t {
                Tok::Semi => cmds.push(c),
                Tok::Amp => cmds.push(Cmd::Async(Box::new(c))),
                Tok::Newline if *end != Tok::Newline => cmds.push(c),
                _ if t == *end || *end == Tok::Newline && t == Tok::Eof => {
                    cmds.push(c);
                    break;
                }
                _ => {
                    self.peeked = Some(t);
                    return self.error("syntax error");
                }
            }
        }
        Ok(cmds.into_iter().rev().fold(Cmd::Empty, |rest, c| Cmd::seq(c, rest)))
    }

    /// A command and the operators that bind tighter than `min`.
    fn cmd(&mut self, min: u8) -> Result<Cmd> {
        let mut left = self.unary()?;
        loop {
            match *self.peek()? {
                Tok::AndAnd if min < ANDOR => {
                    self.next()?;
                    let right = self.cmd(ANDOR)?;
                    left = Cmd::And(Box::new(left), Box::new(right));
                }
                Tok::OrOr if min < ANDOR => {
                    self.next()?;
                    let right = self.cmd(ANDOR)?;
                    left = Cmd::Or(Box::new(left), Box::new(right));
                }
                Tok::Pipe(l, r) if min < PIPE => {
                    self.next()?;
                    let right = self.cmd(PIPE)?;
                    left = Cmd::Pipe(Box::new(left), l, r, Box::new(right));
                }
                _ => return Ok(left),
            }
        }
    }

    fn unary(&mut self) -> Result<Cmd> {
        match self.peek()?.clone() {
            Tok::Keyword(Keyword::If) => {
                self.next()?;
                if *self.peek()? == Tok::Keyword(Keyword::Not) {
                    self.next()?;
                    self.skipnl();
                    return Ok(Cmd::IfNot(Box::new(self.cmd(0)?)));
                }
                let cond = self.paren()?;
                self.skipnl();
                Ok(Cmd::If(Box::new(cond), Box::new(self.cmd(0)?)))
            }
            Tok::Keyword(Keyword::For) => {
                self.next()?;
                self.expect(&Tok::LParen)?;
                let name = self.word()?;
                let ws = if *self.peek()? == Tok::Keyword(Keyword::In) {
                    self.next()?;
                    Some(self.words()?)
                } else {
                    None
                };
                self.expect(&Tok::RParen)?;
                self.skipnl();
                Ok(Cmd::For(name, ws, Box::new(self.cmd(0)?)))
            }
            Tok::Keyword(Keyword::While) => {
                self.next()?;
                let cond = self.paren()?;
                self.skipnl();
                Ok(Cmd::While(Box::new(cond), Box::new(self.cmd(0)?)))
            }
            Tok::Keyword(Keyword::Switch) => {
                self.next()?;
                let w = self.word()?;
                if *self.peek()? == Tok::Newline {
                    self.next()?;
                    self.skipnl();
                }
                Ok(Cmd::Switch(w, Box::new(self.brace()?)))
            }
            Tok::Keyword(Keyword::Bang) => {
                self.next()?;
                Ok(Cmd::Not(Box::new(self.cmd(UNARY)?)))
            }
            Tok::Keyword(Keyword::Subshell) => {
                self.next()?;
                Ok(Cmd::Subshell(Box::new(self.cmd(UNARY)?)))
            }
            Tok::Keyword(Keyword::Twiddle) => {
                self.next()?;
                let w = self.word()?;
                Ok(Cmd::Match(w, self.words()?))
            }
            Tok::Keyword(Keyword::Fn) => {
                self.next()?;
                let names = self.words()?;
                if *self.peek()? == Tok::LBrace {
                    Ok(Cmd::Fn(names, Some(Box::new(self.brace()?))))
                } else {
                    Ok(Cmd::Fn(names, None))
                }
            }
            Tok::LBrace => {
                let body = Cmd::Brace(Box::new(self.brace()?));
                // The epilog: redirections after the brace, which apply
                // in the order written, the first outermost.
                let mut rs = Vec::new();
                while self.is_redir()? {
                    match self.redir_or_pipefd()? {
                        Ok(r) => rs.push(r),
                        Err(_) => return self.error("syntax error"),
                    }
                }
                Ok(rs.into_iter().rev().fold(body, |c, r| Cmd::Redir(r, Box::new(c))))
            }
            Tok::Redir(..) | Tok::Dup(..) => match self.redir_or_pipefd()? {
                Ok(r) => Ok(Cmd::Redir(r, Box::new(self.cmd(UNARY)?))),
                Err(w) => {
                    let first = self.carets(w)?;
                    self.simple(first)
                }
            },
            t if starts_word(&t) && !matches!(t, Tok::Keyword(_)) => {
                let first = self.word()?;
                if *self.peek()? == Tok::Eq {
                    self.next()?;
                    let value = self.word()?;
                    return Ok(Cmd::Assign(first, value, Box::new(self.cmd(UNARY)?)));
                }
                self.simple(first)
            }
            _ => Ok(Cmd::Empty),
        }
    }

    /// The rest of a simple command, after its first word.
    fn simple(&mut self, first: Word) -> Result<Cmd> {
        let mut args = alloc::vec![Arg::Word(first)];
        loop {
            if self.is_redir()? {
                match self.redir_or_pipefd()? {
                    Ok(r) => args.push(Arg::Redir(r)),
                    Err(w) => {
                        let w = self.carets(w)?;
                        args.push(Arg::Word(w));
                    }
                }
            } else if starts_word(self.peek()?) {
                args.push(Arg::Word(self.word()?));
            } else {
                return Ok(Cmd::Simple(args));
            }
        }
    }

    fn is_redir(&mut self) -> Result<bool> {
        Ok(matches!(self.peek()?, Tok::Redir(..) | Tok::Dup(..)))
    }

    /// A redirection, or, for an arrow followed by a brace, the word
    /// `<{cmd}` or `>{cmd}`.
    fn redir_or_pipefd(&mut self) -> Result<core::result::Result<Redir, Word>> {
        match self.next()? {
            Tok::Dup(fd, Some(from)) => Ok(Ok(Redir::Dup(fd, from))),
            Tok::Dup(fd, None) => Ok(Ok(Redir::Close(fd))),
            Tok::Redir(k, fd) => {
                if *self.peek()? == Tok::LBrace {
                    let c = self.brace()?;
                    return Ok(Err(Word::PipeFd(k, fd, Box::new(c))));
                }
                let w = self.word()?;
                if k != RedirKind::Here {
                    return Ok(Ok(Redir::File(k, fd, w)));
                }
                let Word::Lit(tag, quoted) = w else {
                    return self.error("Bad here tag");
                };
                let h = Here {
                    tag,
                    quoted,
                    text: Rc::new(RefCell::new(Vec::new())),
                };
                self.here.push(h.clone());
                Ok(Ok(Redir::Here(fd, h)))
            }
            _ => self.error("syntax error"),
        }
    }

    /// `(body)`, as after `if` and `while`.
    fn paren(&mut self) -> Result<Cmd> {
        self.expect(&Tok::LParen)?;
        self.list(&Tok::RParen)
    }

    /// `{body}`: what is inside.
    fn brace(&mut self) -> Result<Cmd> {
        self.expect(&Tok::LBrace)?;
        self.list(&Tok::RBrace)
    }

    /// Words, as many as follow.
    fn words(&mut self) -> Result<Vec<Word>> {
        let mut ws = Vec::new();
        while starts_word(self.peek()?) || matches!(self.peek()?, Tok::Redir(..)) {
            ws.push(self.word()?);
        }
        Ok(ws)
    }

    /// A word: simple ones joined by carets.
    fn word(&mut self) -> Result<Word> {
        let w = self.comword()?;
        self.carets(w)
    }

    fn carets(&mut self, mut w: Word) -> Result<Word> {
        while *self.peek()? == Tok::Caret {
            self.next()?;
            let right = self.comword()?;
            w = Word::Concat(Box::new(w), Box::new(right));
        }
        Ok(w)
    }

    /// A word with no caret at its top, or a keyword taken as a word.
    fn comword(&mut self) -> Result<Word> {
        match self.next()? {
            Tok::Word(w, q) => Ok(Word::Lit(w, q)),
            Tok::Keyword(k) => {
                // What follows at once is joined to it, as to any word.
                self.lex.lastword = true;
                Ok(Word::Lit(k.text().to_vec(), false))
            }
            Tok::Dollar => {
                let name = self.comword()?;
                if *self.peek()? == Tok::Sub {
                    self.next()?;
                    let subs = self.words()?;
                    self.expect(&Tok::RParen)?;
                    return Ok(Word::Sub(Box::new(name), subs));
                }
                Ok(Word::Var(Box::new(name)))
            }
            Tok::Count => Ok(Word::Count(Box::new(self.comword()?))),
            Tok::Join => Ok(Word::Join(Box::new(self.comword()?))),
            Tok::Backquote => Ok(Word::Backquote(Box::new(self.brace()?))),
            Tok::LParen => {
                let ws = self.words()?;
                self.expect(&Tok::RParen)?;
                Ok(Word::List(ws))
            }
            Tok::Redir(k, fd) if *self.peek()? == Tok::LBrace => Ok(Word::PipeFd(k, fd, Box::new(self.brace()?))),
            t => {
                self.peeked = Some(t);
                self.error("syntax error")
            }
        }
    }
}

/// Whether `t` can start a word (a keyword too, after a command's first
/// word).
fn starts_word(t: &Tok) -> bool {
    matches!(
        t,
        Tok::Word(..) | Tok::Keyword(_) | Tok::Dollar | Tok::Count | Tok::Join | Tok::Backquote | Tok::LParen
    )
}

/// Checks that each `if not` follows an `if`, walking the commands in the
/// order Plan 9 compiles them; `iflast` says whether the last one was an
/// `if`.
fn check_ifnot(c: &Cmd, iflast: &mut bool) -> core::result::Result<(), &'static str> {
    match c {
        Cmd::Empty => return Ok(()),
        Cmd::Seq(a, b) => {
            check_ifnot(a, iflast)?;
            check_ifnot(b, iflast)?;
            *iflast = matches!(**a, Cmd::If(..));
            return Ok(());
        }
        Cmd::IfNot(body) => {
            if !*iflast {
                return Err("`if not' does not follow `if(...)'");
            }
            check_ifnot(body, iflast)?;
            *iflast = matches!(**body, Cmd::If(..));
            return Ok(());
        }
        Cmd::Brace(a) | Cmd::Async(a) | Cmd::Not(a) | Cmd::Subshell(a) | Cmd::Redir(_, a) | Cmd::Assign(_, _, a) => {
            check_ifnot(a, iflast)?;
        }
        Cmd::And(a, b) | Cmd::Or(a, b) | Cmd::Pipe(a, _, _, b) | Cmd::If(a, b) | Cmd::While(a, b) => {
            check_ifnot(a, iflast)?;
            check_ifnot(b, iflast)?;
        }
        Cmd::For(_, _, a) | Cmd::Switch(_, a) | Cmd::Fn(_, Some(a)) => check_ifnot(a, iflast)?,
        Cmd::Simple(_) | Cmd::Match(..) | Cmd::Fn(_, None) => {}
    }
    *iflast = matches!(c, Cmd::If(..));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::Text;
    use alloc::format;
    use alloc::string::String;

    /// Each line of `s`, parsed and printed back.
    fn lines(s: &str) -> Vec<String> {
        let mut p = Parser::new(Box::new(Text::new(s.as_bytes())));
        let mut v = Vec::new();
        loop {
            match p.line() {
                Ok(Some(c)) => v.push(format!("{c}")),
                Ok(None) => return v,
                Err(e) => v.push(format!("error: {} at {:?}", e.msg, String::from_utf8_lossy(&e.token))),
            }
        }
    }

    fn one(s: &str) -> String {
        let v = lines(s);
        assert_eq!(v.len(), 1, "{s:?} gave {v:?}");
        v.into_iter().next().unwrap()
    }

    #[test]
    fn simple_commands() {
        assert_eq!(one("echo a   b"), "echo a b");
        assert_eq!(one("echo 'a b' ''"), "echo 'a b' ''");
        assert_eq!(one("echo $x.c $x(1 2) $#y $\"z $$p"), "echo $x^.c $x(1 2) $#y $\"z $$p");
        assert_eq!(one("echo (a b)^c d^(e f)"), "echo (a b)^c d^(e f)");
        assert_eq!(one("echo for if in"), "echo for if in");
        assert_eq!(one("echo if$x if.c"), "echo if^$x if.c");
        assert_eq!(one("echo `{ls} <{a} >{b}"), "echo `{ls} <{a} >{b}");
        assert_eq!(one("x=1"), "x=1");
        assert_eq!(one("x=(a b) y=() cmd"), "x=(a b) y=() cmd");
        assert_eq!(one("\n"), "");
        assert_eq!(one("# just a comment\n"), "");
        assert_eq!(lines(""), Vec::<String>::new());
    }

    #[test]
    fn redirections() {
        assert_eq!(one("cat <a >b >>c <>d"), "<a >b >>c <>d cat");
        assert_eq!(one("cmd >[2]e >[2=1] >[3=]"), ">[2]e >[2=1]>[3=]cmd");
        assert_eq!(one(">f echo"), ">f echo");
        assert_eq!(one("{a;b} >f <g"), ">f <g {a;b}");
        assert_eq!(one(">[2=1] echo"), ">[2=1]echo");
    }

    #[test]
    fn operators_and_precedence() {
        assert_eq!(one("a && b || c"), "a && b || c");
        assert_eq!(one("a|b|[2]c|[3=4]d"), "a|b|[2]c|[3=4]d");
        assert_eq!(one("! a | b && c"), "! a|b && c");
        assert!(matches!(
            parse1("! a | b && c"),
            Cmd::And(a, _) if matches!(*a, Cmd::Not(ref p) if matches!(**p, Cmd::Pipe(..)))
        ));
        assert!(matches!(parse1("a && b | c"), Cmd::And(_, b) if matches!(*b, Cmd::Pipe(..))));
        assert!(matches!(parse1("x=1 a && b"), Cmd::And(a, _) if matches!(*a, Cmd::Assign(..))));
        assert!(matches!(parse1("if(t) a && b"), Cmd::If(_, b) if matches!(*b, Cmd::And(..))));
        assert_eq!(one("a & b; c &"), "a&;b;c&");
        assert_eq!(one("a &&\n\nb"), "a && b");
        assert_eq!(one("@ {cd /}"), "@ {cd /}");
    }

    fn parse1(s: &str) -> Cmd {
        let mut p = Parser::new(Box::new(Text::new(s.as_bytes())));
        p.line().unwrap().unwrap()
    }

    #[test]
    fn compound_commands() {
        assert_eq!(one("if(~ $x y) echo yes"), "if(~ $x y)echo yes");
        assert_eq!(
            lines("if(false)\n  echo a\nif not\n  echo b\n"),
            ["if(false)echo a", "if not echo b"]
        );
        assert_eq!(one("for(i in a b) echo $i"), "for(i in a b)echo $i");
        assert_eq!(one("for(i in) echo"), "for(i in ())echo");
        assert_eq!(one("for(i)\necho $i"), "for(i)echo $i");
        assert_eq!(one("while(test) {a\nb\n}"), "while (test){a;b}");
        assert_eq!(
            one("switch($x){\ncase a\n\techo a\ncase *\n\techo other\n}"),
            "switch ($x) {case a;echo a;case *;echo other}"
        );
        assert_eq!(one("fn f {echo $*}"), "fn f {echo $*}");
        assert_eq!(one("fn f g"), "fn f g ");
        assert_eq!(one("~ $x *.c a?b [a-z]"), "~ $x *.c a?b [a-z]");
        assert_eq!(one("{\n\ta\n\n\tb\n}"), "{a;b}");
    }

    #[test]
    fn here_documents() {
        let text = "cat <<EOF >f; cat <<'X'\none $x\nEOF\n$y\nX\necho after\ncat <<E\npartial";
        let mut p = Parser::new(Box::new(Text::new(text.as_bytes())));
        let upper = |l: &[u8]| l.to_ascii_uppercase();
        let c = p.line_with(&upper).unwrap().unwrap();
        assert_eq!(format!("{c}"), "<<EOF >f cat;<<'X' cat");
        let mut docs = Vec::new();
        collect(&c, &mut docs);
        assert_eq!(docs, ["ONE $X\n", "$y\n"]);
        assert_eq!(format!("{}", p.line().unwrap().unwrap()), "echo after");
        // At the end of the input, a line with no newline is dropped.
        let c = p.line().unwrap().unwrap();
        let mut docs = Vec::new();
        collect(&c, &mut docs);
        assert_eq!(docs, [""]);
        assert!(p.line().unwrap().is_none());
        assert_eq!(lines("cat <<$x\n"), ["error: Bad here tag at \"\""]);
    }

    /// The text of each here document in `c`.
    fn collect(c: &Cmd, out: &mut Vec<String>) {
        match c {
            Cmd::Seq(a, b) => {
                collect(a, out);
                collect(b, out);
            }
            Cmd::Simple(args) => {
                for a in args {
                    if let Arg::Redir(Redir::Here(_, h)) = a {
                        out.push(String::from_utf8(h.text.borrow().clone()).unwrap());
                    }
                }
            }
            _ => {}
        }
    }

    #[test]
    fn backquotes() {
        assert_eq!(one("x=`{ls}; y=`{a} z=`{b}"), "x=`{ls};y=`{a} z=`{b}");
        // Plan 9's grammar has no `split{...}.
        assert_eq!(lines("x=`:{a}\necho ok"), ["error: syntax error at \":\"", "echo ok"]);
        // Any arrow before a brace is a pipe file, printed as written.
        assert_eq!(one("cmp >>{a} <>[3]{b}"), "cmp >>{a} <>[3]{b}");
    }

    #[test]
    fn errors() {
        assert_eq!(lines("echo a=b\necho ok"), ["error: syntax error at \"=\"", "echo ok"]);
        assert_eq!(lines("echo (a\n"), ["error: syntax error at \"\""]);
        assert_eq!(
            lines("if not echo"),
            ["error: `if not' does not follow `if(...)' at \"\""]
        );
        assert_eq!(lines("x="), ["error: syntax error at \"\""]);
        assert_eq!(lines("echo a(b)"), ["error: syntax error at \"(\""]);
        assert_eq!(lines("}"), ["error: syntax error at \"}\""]);
        assert_eq!(lines("echo }\necho ok"), ["error: syntax error at \"}\"", "echo ok"]);
    }
}
