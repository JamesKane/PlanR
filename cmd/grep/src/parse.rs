//! The pattern's grammar and lexer, as `grep.y`: Plan 9 has yacc
//! generate the parser; this is the same grammar by recursive descent.
//!
//! ```text
//! prog:  expr newlines
//! expr:  expr0 | expr newlines expr0
//! expr0: expr1 | '*' expr1          (the '*' makes the rest literal)
//! expr1: expr2 | expr1 '|' expr2
//! expr2: expr3 | expr2 expr3
//! expr3: expr4 | expr3 '*' | expr3 '+' | expr3 '?'
//! expr4: char | '^' | '$' | '.' | class | '(' expr1 ')'
//! ```
//!
//! yacc's parser reduces `prog: /* empty */` (whose action is the error
//! "empty pattern") by default at the start, so a pattern that does not
//! start as an `expr0` can is "empty"; any other error is "syntax error".

use alloc::format;
use alloc::vec::Vec;

use crate::{Fatal, Grep, Re2, TALT, TBEGIN, TCLASS, TEND, fatal};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Tok {
    Class(Vec<u8>),
    Char(u8),
    LParen,
    RParen,
    Alt,
    Star,
    Plus,
    Ques,
    Begin,
    End,
    Dot,
    Bad,
    Newline,
    /// The end, after the NUL's newline.
    Eof,
}

/// What C's `u.string`, where a class is gathered, holds, less 5.
const CLASSMAX: usize = 16 * 1024 - 5;

/// The parser: C's yacc state is the one token it has looked at.
pub(crate) struct Parser<'a> {
    g: &'a mut Grep,
    tok: Option<Tok>,
}

impl Grep {
    /// C's `yyerror`.
    pub(crate) fn yyerror(&self, e: &str) -> Fatal {
        let mut msg = Vec::from(&b"grep: "[..]);
        if let Some(f) = &self.filename {
            msg.extend_from_slice(f);
            msg.extend_from_slice(format!(":{}: ", self.lineno).as_bytes());
        } else if let Some(p) = &self.pattern {
            msg.extend_from_slice(p);
            msg.extend_from_slice(b": ");
        }
        msg.extend_from_slice(e.as_bytes());
        fatal(alloc::string::String::from_utf8_lossy(&msg).into_owned(), "syntax")
    }

    /// The next byte of the pattern, folded for `-i`; 0 at its end.
    pub(crate) fn getrec(&mut self) -> u8 {
        let i = &mut self.input;
        let c = if self.flags[usize::from(b'f')] > 0 {
            let c = i.file.get(i.file_at).copied().unwrap_or(0);
            i.file_at += 1;
            c
        } else {
            let c = i.arg.get(i.at).copied().unwrap_or(0);
            i.at += 1;
            c
        };
        let c = if self.flags[usize::from(b'i')] > 0 {
            c.to_ascii_lowercase()
        } else {
            c
        };
        if c == b'\n' {
            self.lineno += 1;
        }
        c
    }

    fn yylex(&mut self) -> Result<Tok, Fatal> {
        if let Some(t) = self.peekc.take() {
            return Ok(t);
        }
        let mut c = self.getrec();
        if self.literal {
            if c != 0 && c != b'\n' {
                return Ok(Tok::Char(c));
            }
            self.literal = false;
        }
        Ok(match c {
            b'\\' => {
                c = self.getrec();
                if c == b'\n' { Tok::Newline } else { Tok::Char(c) }
            }
            b'[' => return self.getclass(),
            b'(' => Tok::LParen,
            b')' => Tok::RParen,
            b'|' => Tok::Alt,
            b'*' => Tok::Star,
            b'+' => Tok::Plus,
            b'?' => Tok::Ques,
            b'^' => Tok::Begin,
            b'$' => Tok::End,
            b'.' => Tok::Dot,
            0 => {
                self.peekc = Some(Tok::Eof);
                Tok::Newline
            }
            b'\n' => Tok::Newline,
            _ => Tok::Char(c),
        })
    }

    /// A class, after its `[`: its text, with `\`s kept, and a negated
    /// one starting `^\n-\n`.
    fn getclass(&mut self) -> Result<Tok, Fatal> {
        let mut q = Vec::new();
        let mut c = self.getrec();
        if c == b'^' {
            q.extend_from_slice(b"^\n-\n");
            c = self.getrec();
        }
        loop {
            if q.len() >= CLASSMAX {
                return Err(crate::error("class too long"));
            }
            if c == b']' || c == 0 {
                break;
            }
            if c == b'\\' {
                q.push(c);
                c = self.getrec();
                if c == 0 {
                    break;
                }
            }
            q.push(c);
            c = self.getrec();
        }
        if c == 0 {
            return Ok(Tok::Bad);
        }
        Ok(Tok::Class(q))
    }
}

impl Parser<'_> {
    pub(crate) fn new(g: &mut Grep) -> Parser<'_> {
        Parser { g, tok: None }
    }

    fn peek(&mut self) -> Result<&Tok, Fatal> {
        if self.tok.is_none() {
            self.tok = Some(self.g.yylex()?);
        }
        Ok(self.tok.as_ref().unwrap_or(&Tok::Eof))
    }

    fn take(&mut self) -> Result<Tok, Fatal> {
        self.peek()?;
        Ok(self.tok.take().unwrap_or(Tok::Eof))
    }

    fn syntax(&self) -> Fatal {
        self.g.yyerror("syntax error")
    }

    fn starts_expr3(t: &Tok) -> bool {
        matches!(
            t,
            Tok::Char(_) | Tok::Begin | Tok::End | Tok::Dot | Tok::Class(_) | Tok::LParen
        )
    }

    /// `prog`: sets the pattern's graph, with what any line may have
    /// around it.
    pub(crate) fn prog(&mut self) -> Result<(), Fatal> {
        let t = self.peek()?;
        if !(Self::starts_expr3(t) || *t == Tok::Star) {
            return Err(self.g.yyerror("empty pattern"));
        }
        let mut e = self.expr0()?;
        loop {
            self.newlines()?;
            let t = self.peek()?;
            if *t == Tok::Eof {
                break;
            }
            if !(Self::starts_expr3(t) || *t == Tok::Star) {
                return Err(self.syntax());
            }
            let e0 = self.expr0()?;
            e = self.g.re2or(e, e0);
        }
        let g = &mut *self.g;
        let end = g.ral(TEND);
        let mut x = Re2 { beg: end, end };
        let lo = g.re2char(0x00, u32::from(b'\n') - 1);
        let hi = g.re2char(u32::from(b'\n') + 1, 0xff);
        let notnl = g.re2or(lo, hi);
        let tail = g.re2star(notnl);
        x = g.re2cat(tail, x);
        x = g.re2cat(e, x);
        let any = g.re2char(0x00, 0xff);
        let head = g.re2star(any);
        x = g.re2cat(head, x);
        g.topre = x;
        Ok(())
    }

    fn newlines(&mut self) -> Result<(), Fatal> {
        if *self.peek()? != Tok::Newline {
            return Err(self.syntax());
        }
        while *self.peek()? == Tok::Newline {
            self.take()?;
        }
        Ok(())
    }

    fn expr0(&mut self) -> Result<Re2, Fatal> {
        if *self.peek()? == Tok::Star {
            // The rest of the line is literal, from the next token.
            self.take()?;
            self.g.literal = true;
        }
        self.expr1()
    }

    fn expr1(&mut self) -> Result<Re2, Fatal> {
        let mut x = self.expr2()?;
        while *self.peek()? == Tok::Alt {
            self.take()?;
            let y = self.expr2()?;
            x = self.g.re2or(x, y);
        }
        Ok(x)
    }

    fn expr2(&mut self) -> Result<Re2, Fatal> {
        let mut x = self.expr3()?;
        while Self::starts_expr3(self.peek()?) {
            let y = self.expr3()?;
            x = self.g.re2cat(x, y);
        }
        Ok(x)
    }

    fn expr3(&mut self) -> Result<Re2, Fatal> {
        let mut x = self.expr4()?;
        loop {
            match self.peek()? {
                Tok::Star => {
                    self.take()?;
                    x = self.g.re2star(x);
                }
                Tok::Plus => {
                    self.take()?;
                    let g = &mut *self.g;
                    let beg = g.ral(TALT);
                    g.patchnext(x.end, beg);
                    g.re[beg].alt = x.beg;
                    x = Re2 { beg: x.beg, end: beg };
                }
                Tok::Ques => {
                    self.take()?;
                    let g = &mut *self.g;
                    let beg = g.ral(TALT);
                    g.re[beg].alt = x.beg;
                    g.appendnext(x.end, beg);
                    x = Re2 { beg, end: x.end };
                }
                _ => return Ok(x),
            }
        }
    }

    fn expr4(&mut self) -> Result<Re2, Fatal> {
        match self.take()? {
            Tok::Char(c) => {
                let g = &mut *self.g;
                let beg = g.ral(TCLASS);
                g.re[beg].lo = u32::from(c);
                g.re[beg].hi = u32::from(c);
                Ok(Re2 { beg, end: beg })
            }
            Tok::Begin => {
                let beg = self.g.ral(TBEGIN);
                Ok(Re2 { beg, end: beg })
            }
            Tok::End => {
                let beg = self.g.ral(TEND);
                Ok(Re2 { beg, end: beg })
            }
            Tok::Dot => self.g.re2class(b"^\n"),
            Tok::Class(s) => self.g.re2class(&s),
            Tok::LParen => {
                let x = self.expr1()?;
                if self.take()? != Tok::RParen {
                    return Err(self.syntax());
                }
                Ok(x)
            }
            _ => Err(self.syntax()),
        }
    }
}
