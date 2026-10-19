//! rc's lexer, as Plan 9's `lex.c`: words, keywords, the operators, the
//! redirection tokens with their descriptor numbers (`>[2=1]`), comments,
//! `\`-newline, and the free carets: a word followed at once by another
//! is joined to it with `^`, and a word followed at once by `(` opens a
//! subscript.

use alloc::boxed::Box;
use alloc::vec::Vec;

/// Where the lexer reads from, a byte at a time, as it needs them; an
/// interactive input prompts before reading a new line.
pub trait Input {
    fn next(&mut self) -> Option<u8>;
}

/// A whole text read as input.
#[derive(Debug)]
pub struct Text {
    text: Vec<u8>,
    at: usize,
}

impl Text {
    #[must_use]
    pub fn new(text: &[u8]) -> Text {
        Text {
            text: text.to_vec(),
            at: 0,
        }
    }
}

impl Input for Text {
    fn next(&mut self) -> Option<u8> {
        let c = self.text.get(self.at).copied();
        self.at += 1;
        c
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
    For,
    In,
    While,
    If,
    Not,
    Twiddle,
    Bang,
    Subshell,
    Switch,
    Fn,
}

impl Keyword {
    fn look(w: &[u8]) -> Option<Keyword> {
        Some(match w {
            b"for" => Keyword::For,
            b"in" => Keyword::In,
            b"while" => Keyword::While,
            b"if" => Keyword::If,
            b"not" => Keyword::Not,
            b"~" => Keyword::Twiddle,
            b"!" => Keyword::Bang,
            b"@" => Keyword::Subshell,
            b"switch" => Keyword::Switch,
            b"fn" => Keyword::Fn,
            _ => return None,
        })
    }

    #[must_use]
    pub fn text(self) -> &'static [u8] {
        match self {
            Keyword::For => b"for",
            Keyword::In => b"in",
            Keyword::While => b"while",
            Keyword::If => b"if",
            Keyword::Not => b"not",
            Keyword::Twiddle => b"~",
            Keyword::Bang => b"!",
            Keyword::Subshell => b"@",
            Keyword::Switch => b"switch",
            Keyword::Fn => b"fn",
        }
    }
}

/// What a redirection does with its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirKind {
    /// `<`
    Read,
    /// `>`
    Write,
    /// `>>`
    Append,
    /// `<>`
    ReadWrite,
    /// `<<`
    Here,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    /// A word, and whether it was quoted.
    Word(Vec<u8>, bool),
    Keyword(Keyword),
    /// `$`
    Dollar,
    /// `$#`
    Count,
    /// `$"`
    Join,
    /// `^`, written or free.
    Caret,
    /// `(` right after a word: a subscript.
    Sub,
    LParen,
    RParen,
    LBrace,
    RBrace,
    Semi,
    Amp,
    AndAnd,
    OrOr,
    Eq,
    Backquote,
    Newline,
    Eof,
    /// `|`, `|[l]`, `|[l=r]`: the left side's descriptor, the right's.
    Pipe(u32, u32),
    /// `<`, `>[2]` and so on, on descriptor `fd`.
    Redir(RedirKind, u32),
    /// `>[a=b]` (`fd` becomes a copy of `from`) or `>[a=]` (closed).
    Dup(u32, Option<u32>),
}

/// The characters that end a word.
#[must_use]
pub fn wordchr(c: u8) -> bool {
    !b"\n \t#;&|^$=`'{}()<>".contains(&c)
}

/// The characters of a variable's name.
#[must_use]
pub fn idchr(c: u8) -> bool {
    c > b' ' && !b"!\"#$%&'()+,-./:;<=>?@[\\]^`{|}~".contains(&c)
}

/// A syntax error, with the token it was found at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    pub msg: &'static str,
    /// The token's text, empty at a newline or the end.
    pub token: Vec<u8>,
    pub line: u32,
}

#[allow(clippy::struct_excessive_bools)] // Plan 9's lexer state, flag by flag
pub struct Lexer {
    input: Box<dyn Input>,
    /// A byte read ahead (`None` for the end) and the newlines read to
    /// get it.
    peekc: Option<(Option<u8>, u32)>,
    /// A byte `getnext` put back after a `\` that did not end a line.
    back: Option<u8>,
    in_quote: bool,
    in_comment: bool,
    /// The last token was a word: what follows at once is a free caret or
    /// a subscript.
    pub lastword: bool,
    /// The last token was `$`, `$#` or `$"`: a name follows.
    lastdol: bool,
    /// The text of the last token, for errors.
    pub token: Vec<u8>,
    /// The line being read, from 1.
    pub line: u32,
    /// The line the last token started on.
    pub tok_line: u32,
    /// Whether the end of the input has been read.
    pub eof: bool,
}

impl core::fmt::Debug for Lexer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Lexer")
            .field("line", &self.line)
            .finish_non_exhaustive()
    }
}

impl Lexer {
    #[must_use]
    pub fn new(input: Box<dyn Input>) -> Lexer {
        Lexer {
            input,
            peekc: None,
            back: None,
            in_quote: false,
            in_comment: false,
            lastword: false,
            lastdol: false,
            token: Vec::new(),
            line: 1,
            tok_line: 1,
            eof: false,
        }
    }

    /// The next byte, and the newlines read to get it: `\`-newline reads
    /// as a blank, outside quotes and comments.
    fn getnext(&mut self) -> (Option<u8>, u32) {
        if let Some(c) = self.back.take() {
            return (Some(c), 0);
        }
        if self.eof {
            return (None, 0);
        }
        let c = self.input.next();
        if c.is_none() {
            self.eof = true;
        }
        if self.in_quote || c != Some(b'\\') {
            return (c, 0);
        }
        match self.input.next() {
            Some(b'\n') if !self.in_comment => (Some(b' '), 1),
            Some(d) => {
                self.back = Some(d);
                (c, 0)
            }
            None => {
                self.eof = true;
                (c, 0)
            }
        }
    }

    fn nextc(&mut self) -> Option<u8> {
        if self.peekc.is_none() {
            self.peekc = Some(self.getnext());
        }
        self.peekc.and_then(|(c, _)| c)
    }

    fn advance(&mut self) -> Option<u8> {
        let c = self.nextc();
        if let Some((_, n)) = self.peekc.take() {
            self.line += n + u32::from(c == Some(b'\n'));
        }
        c
    }

    fn nextis(&mut self, c: u8) -> bool {
        if self.nextc() == Some(c) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn skipwhite(&mut self) {
        loop {
            let mut c = self.nextc();
            if c == Some(b'#') {
                self.in_comment = true;
                loop {
                    c = self.nextc();
                    if c.is_none() || c == Some(b'\n') {
                        break;
                    }
                    self.advance();
                }
                self.in_comment = false;
            }
            if c == Some(b' ') || c == Some(b'\t') {
                self.advance();
            } else {
                return;
            }
        }
    }

    /// Skips blanks, comments and newlines, as after `&&`, `||` and `|`,
    /// and where the grammar allows a newline before a command.
    pub fn skipnl(&mut self) {
        loop {
            self.skipwhite();
            if self.nextc() != Some(b'\n') {
                return;
            }
            self.advance();
        }
    }

    /// The next byte as it is, for a here document's text: no
    /// continuations, comments or tokens.
    pub fn raw(&mut self) -> Option<u8> {
        if self.peekc.is_some() {
            return self.advance();
        }
        let c = match self.back.take() {
            Some(c) => Some(c),
            None if self.eof => None,
            None => self.input.next(),
        };
        match c {
            None => self.eof = true,
            Some(b'\n') => self.line += 1,
            Some(_) => {}
        }
        c
    }

    /// Reads on after the end of the input, which an interrupted read of
    /// a terminal looks like, as if starting a new line.
    pub fn clear_eof(&mut self) {
        self.eof = false;
        self.peekc = None;
        self.back = None;
        self.lastword = false;
        self.lastdol = false;
        self.in_quote = false;
    }

    /// Skips the rest of the line, after a syntax error.
    pub fn skipline(&mut self) {
        self.lastword = false;
        self.lastdol = false;
        self.in_quote = false;
        while let Some(c) = self.advance() {
            if c == b'\n' {
                return;
            }
        }
    }

    fn error(&self, msg: &'static str) -> SyntaxError {
        SyntaxError {
            msg,
            token: self.token.clone(),
            line: self.tok_line,
        }
    }

    fn set(&mut self, text: &[u8], t: Tok) -> Tok {
        self.token.clear();
        self.token.extend_from_slice(text);
        t
    }

    pub fn lex(&mut self) -> Result<Tok, SyntaxError> {
        let d = self.nextc();
        self.tok_line = self.line;
        if self.lastword {
            self.lastword = false;
            if d == Some(b'(') {
                self.advance();
                return Ok(self.set(b"(", Tok::Sub));
            }
            if d.is_some_and(|d| wordchr(d) || b"'`$\"".contains(&d)) {
                return Ok(self.set(b"^", Tok::Caret));
            }
        }
        self.in_quote = false;
        self.skipwhite();
        self.tok_line = self.line;
        let Some(c) = self.advance() else {
            self.lastdol = false;
            return Ok(self.set(b"", Tok::Eof));
        };
        match c {
            b'$' => {
                self.lastdol = true;
                if self.nextis(b'#') {
                    return Ok(self.set(b"$#", Tok::Count));
                }
                if self.nextis(b'"') {
                    return Ok(self.set(b"$\"", Tok::Join));
                }
                return Ok(self.set(b"$", Tok::Dollar));
            }
            b'&' => {
                self.lastdol = false;
                if self.nextis(b'&') {
                    self.skipnl();
                    return Ok(self.set(b"&&", Tok::AndAnd));
                }
                return Ok(self.set(b"&", Tok::Amp));
            }
            b'|' if self.nextc() == Some(b'|') => {
                self.advance();
                self.lastdol = false;
                self.skipnl();
                return Ok(self.set(b"||", Tok::OrOr));
            }
            b'|' | b'<' | b'>' => return self.arrow(c),
            b'\'' => return Ok(self.quoted()),
            _ => {}
        }
        if !wordchr(c) {
            self.lastdol = false;
            let t = match c {
                b'(' => Tok::LParen,
                b')' => Tok::RParen,
                b'{' => Tok::LBrace,
                b'}' => Tok::RBrace,
                b';' => Tok::Semi,
                b'^' => Tok::Caret,
                b'=' => Tok::Eq,
                b'`' => Tok::Backquote,
                _ => Tok::Newline,
            };
            let text: &[u8] = if c == b'\n' { b"" } else { &[c] };
            return Ok(self.set(text, t));
        }
        let mut w = alloc::vec![c];
        loop {
            match self.nextc() {
                Some(c) if if self.lastdol { idchr(c) } else { wordchr(c) } => {
                    w.push(c);
                    self.advance();
                }
                _ => break,
            }
        }
        self.lastdol = false;
        self.token.clone_from(&w);
        if let Some(k) = Keyword::look(&w) {
            return Ok(Tok::Keyword(k));
        }
        self.lastword = true;
        Ok(Tok::Word(w, false))
    }

    fn quoted(&mut self) -> Tok {
        self.lastdol = false;
        self.lastword = true;
        self.in_quote = true;
        let mut w = Vec::new();
        while let Some(c) = self.advance() {
            if c == b'\'' {
                if self.nextc() != Some(b'\'') {
                    break;
                }
                self.advance();
            }
            w.push(c);
        }
        self.in_quote = false;
        let mut text = alloc::vec![b'\''];
        text.extend_from_slice(&w);
        self.token = text;
        Tok::Word(w, true)
    }

    /// `|`, `<`, `>` and what may follow them: a second arrow, and
    /// descriptors in brackets.
    fn arrow(&mut self, c: u8) -> Result<Tok, SyntaxError> {
        self.lastdol = false;
        self.token.clear();
        self.token.push(c);
        let (mut kind, mut fd) = match c {
            b'|' => (None, 1),
            b'>' if self.nextis(b'>') => (Some(RedirKind::Append), 1),
            b'>' => (Some(RedirKind::Write), 1),
            b'<' if self.nextis(b'<') => (Some(RedirKind::Here), 0),
            b'<' if self.nextis(b'>') => (Some(RedirKind::ReadWrite), 0),
            _ => (Some(RedirKind::Read), 0),
        };
        if self.token.len() == 1 && kind.is_some_and(|k| matches!(k, RedirKind::Append | RedirKind::Here)) {
            self.token.push(c);
        } else if kind == Some(RedirKind::ReadWrite) {
            self.token.push(b'>');
        }
        let mut other = None;
        let mut dup = false;
        if self.nextis(b'[') {
            self.token.push(b'[');
            let what = if kind.is_none() {
                "pipe syntax"
            } else {
                "redirection syntax"
            };
            let Some(n) = self.number() else {
                return Err(self.bad(what));
            };
            fd = n;
            if self.nextis(b'=') {
                self.token.push(b'=');
                dup = true;
                if self.nextc().is_some_and(|c| c.is_ascii_digit()) {
                    other = self.number();
                } else if kind.is_none() {
                    return Err(self.error(what));
                }
            }
            if !self.nextis(b']') {
                return Err(self.bad(what));
            }
            self.token.push(b']');
            if dup && kind.is_some_and(|k| matches!(k, RedirKind::Here | RedirKind::Append)) {
                return Err(self.error(what));
            }
        }
        Ok(match kind.take() {
            None => {
                self.skipnl();
                Tok::Pipe(fd, if dup { other.unwrap_or(0) } else { 0 })
            }
            Some(_) if dup => Tok::Dup(fd, other),
            Some(k) => Tok::Redir(k, fd),
        })
    }

    /// An error at the character that should not be there, which is
    /// shown with the token.
    fn bad(&mut self, what: &'static str) -> SyntaxError {
        if let Some(c) = self.advance() {
            self.token.push(c);
        }
        self.error(what)
    }

    fn number(&mut self) -> Option<u32> {
        let mut n: Option<u32> = None;
        while let Some(c) = self.nextc().filter(u8::is_ascii_digit) {
            self.advance();
            self.token.push(c);
            n = Some(n.unwrap_or(0).saturating_mul(10).saturating_add(u32::from(c - b'0')));
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn toks(s: &str) -> Vec<Tok> {
        let mut l = Lexer::new(Box::new(Text::new(s.as_bytes())));
        let mut v = Vec::new();
        loop {
            let t = l.lex().unwrap();
            if t == Tok::Eof {
                return v;
            }
            v.push(t);
        }
    }

    fn w(s: &str) -> Tok {
        Tok::Word(s.as_bytes().to_vec(), false)
    }

    fn q(s: &str) -> Tok {
        Tok::Word(s.as_bytes().to_vec(), true)
    }

    #[test]
    fn words_and_carets() {
        assert_eq!(toks("echo a b"), [w("echo"), w("a"), w("b")]);
        assert_eq!(
            toks("cc $x.c 'it''s'x"),
            [
                w("cc"),
                Tok::Dollar,
                w("x"),
                Tok::Caret,
                w(".c"),
                q("it's"),
                Tok::Caret,
                w("x")
            ]
        );
        assert_eq!(toks("$x-y"), [Tok::Dollar, w("x"), Tok::Caret, w("-y")]);
        assert_eq!(toks("$x*"), [Tok::Dollar, w("x*")]);
        assert_eq!(
            toks("$x(1) $#y $\"z"),
            [
                Tok::Dollar,
                w("x"),
                Tok::Sub,
                w("1"),
                Tok::RParen,
                Tok::Count,
                w("y"),
                Tok::Join,
                w("z")
            ]
        );
        // No caret after a keyword or `)`.
        assert_eq!(
            toks("for(i) (a)b"),
            [
                Tok::Keyword(Keyword::For),
                Tok::LParen,
                w("i"),
                Tok::RParen,
                Tok::LParen,
                w("a"),
                Tok::RParen,
                w("b")
            ]
        );
    }

    #[test]
    fn operators() {
        assert_eq!(
            toks("a&&b||c|d&;!e"),
            [
                w("a"),
                Tok::AndAnd,
                w("b"),
                Tok::OrOr,
                w("c"),
                Tok::Pipe(1, 0),
                w("d"),
                Tok::Amp,
                Tok::Semi,
                w("!e")
            ]
        );
        assert_eq!(
            toks("! ~ @ x=y"),
            [
                Tok::Keyword(Keyword::Bang),
                Tok::Keyword(Keyword::Twiddle),
                Tok::Keyword(Keyword::Subshell),
                w("x"),
                Tok::Eq,
                w("y")
            ]
        );
        assert_eq!(toks("a|\n\nb"), [w("a"), Tok::Pipe(1, 0), w("b")]);
    }

    #[test]
    fn redirections() {
        assert_eq!(
            toks("<a >b >>c <>d <<e"),
            [
                Tok::Redir(RedirKind::Read, 0),
                w("a"),
                Tok::Redir(RedirKind::Write, 1),
                w("b"),
                Tok::Redir(RedirKind::Append, 1),
                w("c"),
                Tok::Redir(RedirKind::ReadWrite, 0),
                w("d"),
                Tok::Redir(RedirKind::Here, 0),
                w("e")
            ]
        );
        assert_eq!(
            toks(">[2]f >[2=1] >[3=] |[2] |[3=4]"),
            [
                Tok::Redir(RedirKind::Write, 2),
                w("f"),
                Tok::Dup(2, Some(1)),
                Tok::Dup(3, None),
                Tok::Pipe(2, 0),
                Tok::Pipe(3, 4)
            ]
        );
        let mut l = Lexer::new(Box::new(Text::new(b">[x]")));
        let e = l.lex().unwrap_err();
        assert_eq!((e.msg, &e.token[..]), ("redirection syntax", &b">[x"[..]));
        let mut l = Lexer::new(Box::new(Text::new(b"|[2=]")));
        assert_eq!(l.lex().unwrap_err().msg, "pipe syntax");
    }

    #[test]
    fn comments_and_continuations() {
        assert_eq!(toks("a # c\\\nb\\\nc"), [w("a"), Tok::Newline, w("b"), w("c")]);
        assert_eq!(toks("a#b"), [w("a")]);
        assert_eq!(toks("'a\\\nb'"), [q("a\\\nb")]);
        assert_eq!(toks("a\\b"), [w("a\\b")]);
        let mut l = Lexer::new(Box::new(Text::new(b"a\nb\\\nc\nd")));
        let lines: Vec<u32> = core::iter::from_fn(|| {
            let t = l.lex().unwrap();
            (t != Tok::Eof).then_some(l.line)
        })
        .collect();
        // a, newline, b, c (after a continuation), newline, d.
        assert_eq!(lines, vec![1, 2, 2, 3, 4, 4]);
    }
}
