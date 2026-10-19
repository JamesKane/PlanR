//! Building the graph, as `sub.c`: nodes and states, fragments joined
//! and alternated, and alternations of many single bytes made into
//! tables (`addcase`).

use alloc::boxed::Box;
use alloc::vec;

use crate::parse::Parser;
use crate::{CASELIM, Fatal, Grep, NIL, Re, Re2, State, TALT, TBEGIN, TCASE, TCLASS, TEND, TOR, error};

impl Grep {
    /// A new state of `n` nodes.
    pub(crate) fn sal(&mut self, n: usize) -> usize {
        self.states.push(State {
            matched: false,
            re: vec![NIL; n],
            linkleft: NIL,
            linkright: NIL,
            next: Box::new([NIL; 256]),
        });
        self.states.len() - 1
    }

    /// A new node.
    pub(crate) fn ral(&mut self, ty: u8) -> usize {
        self.re.push(Re {
            ty,
            generation: 0,
            alt: NIL,
            cases: None,
            lo: 0,
            hi: 0,
            next: NIL,
        });
        self.maxfollow += 1;
        self.re.len() - 1
    }

    /// The bytes the alternation at `r` takes.
    fn countor(&self, mut r: usize) -> u32 {
        let mut n = 0;
        loop {
            let re = &self.re[r];
            match re.ty {
                TOR => {
                    n += self.countor(re.alt);
                    r = re.next;
                }
                TCLASS => return n + re.hi - re.lo + 1,
                _ => return n,
            }
        }
    }

    fn oralloc(&mut self, t: u8, r: usize, b: usize) -> usize {
        if b == NIL {
            return r;
        }
        let a = self.ral(t);
        self.re[a].alt = r;
        self.re[a].next = b;
        a
    }

    /// Adds the alternation at `r` to the table at `c`.
    fn case1(&mut self, c: usize, mut r: usize) {
        loop {
            match self.re[r].ty {
                TOR => {
                    self.case1(c, self.re[r].alt);
                    r = self.re[r].next;
                }
                // Add it to its bytes.
                TCLASS => {
                    let (lo, hi, next) = (self.re[r].lo, self.re[r].hi, self.re[r].next);
                    for n in lo..=hi {
                        let n = n as usize;
                        let old = self.re[c].cases.as_ref().map_or(NIL, |t| t[n]);
                        let new = self.oralloc(TOR, next, old);
                        if let Some(t) = self.re[c].cases.as_mut() {
                            t[n] = new;
                        }
                    }
                    return;
                }
                // Anything else goes to the table's next.
                _ => {
                    let new = self.oralloc(TALT, r, self.re[c].next);
                    self.re[c].next = new;
                    return;
                }
            }
        }
    }

    /// Makes long alternations at and after `r` into tables.
    pub(crate) fn addcase(&mut self, r: usize) -> Result<usize, Fatal> {
        if self.re[r].generation == self.generation {
            return Ok(r);
        }
        self.re[r].generation = self.generation;
        match self.re[r].ty {
            TOR => {
                let n = self.countor(r);
                if n >= CASELIM {
                    let a = self.ral(TCASE);
                    self.re[a].cases = Some(Box::new([NIL; 256]));
                    self.case1(a, r);
                    for i in 0..256 {
                        let ri = self.re[a].cases.as_ref().map_or(NIL, |t| t[i]);
                        if ri != NIL && self.countor(ri) < n {
                            let new = self.addcase(ri)?;
                            if let Some(t) = self.re[a].cases.as_mut() {
                                t[i] = new;
                            }
                        }
                    }
                    return Ok(a);
                }
                Ok(r)
            }
            TALT => {
                let next = self.addcase(self.re[r].next)?;
                self.re[r].next = next;
                let alt = self.addcase(self.re[r].alt)?;
                self.re[r].alt = alt;
                Ok(r)
            }
            TBEGIN | TEND | TCLASS => Ok(r),
            _ => Err(error("addcase")),
        }
    }

    /// Adds the pattern `p` (or, after `-f`, the pattern file's) as
    /// another alternative.
    pub fn str2top(&mut self, p: &[u8]) -> Result<(), Fatal> {
        let oldtop = self.topre;
        self.input.arg = p.to_vec();
        self.input.at = 0;
        if p.is_empty() {
            // It cannot be a file name here.
            return Err(self.yyerror("empty pattern"));
        }
        if self.flags[usize::from(b'f')] == 0 {
            self.pattern = Some(p.to_vec());
        }
        self.topre = Re2 { beg: NIL, end: NIL };
        Parser::new(self).prog()?;
        self.generation += 1;
        if self.topre.beg == NIL {
            return Err(self.yyerror("syntax"));
        }
        if oldtop.beg != NIL {
            self.topre = self.re2or(oldtop, self.topre);
        }
        Ok(())
    }

    /// Joins `b` to the end of the list of ends at `a`.
    pub(crate) fn appendnext(&mut self, mut a: usize, b: usize) {
        while self.re[a].next != NIL {
            a = self.re[a].next;
        }
        self.re[a].next = b;
    }

    /// Makes each end in the list at `a` lead to `b`.
    pub(crate) fn patchnext(&mut self, mut a: usize, b: usize) {
        while a != NIL {
            let n = self.re[a].next;
            self.re[a].next = b;
            a = n;
        }
    }

    pub(crate) fn re2cat(&mut self, a: Re2, b: Re2) -> Re2 {
        self.patchnext(a.end, b.beg);
        Re2 { beg: a.beg, end: b.end }
    }

    pub(crate) fn re2star(&mut self, a: Re2) -> Re2 {
        let beg = self.ral(TALT);
        self.re[beg].alt = a.beg;
        self.patchnext(a.end, beg);
        Re2 { beg, end: beg }
    }

    pub(crate) fn re2or(&mut self, a: Re2, b: Re2) -> Re2 {
        let beg = self.ral(TOR);
        self.re[beg].alt = b.beg;
        self.re[beg].next = a.beg;
        let c = Re2 { beg, end: b.end };
        self.appendnext(c.end, a.end);
        c
    }

    /// The bytes `c0` to `c1`.
    pub(crate) fn re2char(&mut self, c0: u32, c1: u32) -> Re2 {
        let beg = self.ral(TCLASS);
        self.re[beg].lo = c0 & 0xff;
        self.re[beg].hi = c1 & 0xff;
        Re2 { beg, end: beg }
    }
}
