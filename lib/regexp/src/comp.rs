//! The compiler, as `regcomp.c`: an operator-precedence parser over two
//! stacks, operands (fragments of program, first and last instruction)
//! and operators, that links instructions as it reduces; then `NOP`s are
//! taken out of the chains. Plan 9's limits stand: 20 entries a stack,
//! 16 classes a program, 31 ranges a class, 31 subexpressions.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use r::utf;

use crate::{
    ANY, ANYNL, BOL, CAT, CCLASS, END, EOL, LBRA, NCCLASS, NIL, NOP, NSUBEXP, OPERATOR, OR, PLUS, QUEST, RBRA, RUNE,
    Reclass, Reinst, Reprog, STAR, START,
};

const NSTACK: usize = 20;
/// Most classes a program has (`nelem(reprog->class)`).
const NCLASS: usize = 16;
/// Most runes a class's ranges take (`nelem(classp->spans)`).
const NCCRUNE: usize = 64;

/// A fragment of program.
#[derive(Clone, Copy)]
struct Node {
    first: usize,
    last: usize,
}

type Result<T> = core::result::Result<T, String>;

struct Parser<'a> {
    inst: Vec<Reinst>,
    class: Vec<Reclass>,
    andstack: Vec<Node>,
    atorstack: Vec<u16>,
    /// Parallel to `atorstack`.
    subidstack: Vec<usize>,
    /// The id of the current subexpression.
    cursubid: usize,
    /// The last token was an operand.
    lastwasand: bool,
    nbra: usize,
    expr: &'a [u8],
    at: usize,
    lexdone: bool,
    /// The last rune and class lexed.
    yyrune: char,
    yyclass: usize,
}

impl Parser<'_> {
    fn newinst(&mut self, t: u16) -> usize {
        self.inst.push(Reinst {
            ty: t,
            r: '\0',
            subid: 0,
            cp: 0,
            right: NIL,
            next: NIL,
        });
        self.inst.len() - 1
    }

    fn operand(&mut self, t: u16) -> Result<()> {
        if self.lastwasand {
            // Catenation is implicit.
            self.operator(CAT)?;
        }
        let i = self.newinst(t);
        if t == CCLASS || t == NCCLASS {
            self.inst[i].cp = self.yyclass;
        }
        if t == RUNE {
            self.inst[i].r = self.yyrune;
        }
        self.pushand(i, i)?;
        self.lastwasand = true;
        Ok(())
    }

    fn operator(&mut self, t: u16) -> Result<()> {
        if t == RBRA {
            if self.nbra == 0 {
                return Err("unmatched right paren".into());
            }
            self.nbra -= 1;
        }
        if t == LBRA {
            self.cursubid += 1;
            if self.cursubid >= NSUBEXP {
                return Err("too many subexpressions".into());
            }
            self.nbra += 1;
            if self.lastwasand {
                self.operator(CAT)?;
            }
        } else {
            self.evaluntil(t)?;
        }
        if t != RBRA {
            self.pushator(t)?;
        }
        // These look like operands.
        self.lastwasand = matches!(t, STAR | QUEST | PLUS | RBRA);
        Ok(())
    }

    fn pushand(&mut self, first: usize, last: usize) -> Result<()> {
        if self.andstack.len() >= NSTACK {
            return Err(cant("operand stack overflow"));
        }
        self.andstack.push(Node { first, last });
        Ok(())
    }

    fn pushator(&mut self, t: u16) -> Result<()> {
        if self.atorstack.len() >= NSTACK {
            return Err(cant("operator stack overflow"));
        }
        self.atorstack.push(t);
        self.subidstack.push(self.cursubid);
        Ok(())
    }

    /// The top operand, for operator `op` (named in the error).
    fn popand(&mut self, op: Option<char>) -> Result<Node> {
        self.andstack.pop().ok_or_else(|| {
            let mut s = String::from("missing operand for ");
            s.extend(op);
            s
        })
    }

    /// The top operator and its subexpression.
    fn popator(&mut self) -> Result<(u16, usize)> {
        match (self.atorstack.pop(), self.subidstack.pop()) {
            (Some(t), Some(id)) => Ok((t, id)),
            _ => Err(cant("operator stack underflow")),
        }
    }

    fn evaluntil(&mut self, pri: u16) -> Result<()> {
        while pri == RBRA || self.atorstack.last().is_some_and(|&t| t >= pri) {
            let (op, subid) = self.popator()?;
            match op {
                // Must have been RBRA.
                LBRA => {
                    let op1 = self.popand(Some('('))?;
                    let inst2 = self.newinst(RBRA);
                    self.inst[inst2].subid = subid;
                    self.inst[op1.last].next = inst2;
                    let inst1 = self.newinst(LBRA);
                    self.inst[inst1].subid = subid;
                    self.inst[inst1].next = op1.first;
                    self.pushand(inst1, inst2)?;
                    return Ok(());
                }
                OR => {
                    let op2 = self.popand(Some('|'))?;
                    let op1 = self.popand(Some('|'))?;
                    let inst2 = self.newinst(NOP);
                    self.inst[op2.last].next = inst2;
                    self.inst[op1.last].next = inst2;
                    let inst1 = self.newinst(OR);
                    self.inst[inst1].right = op1.first;
                    self.inst[inst1].next = op2.first;
                    self.pushand(inst1, inst2)?;
                }
                CAT => {
                    let op2 = self.popand(None)?;
                    let op1 = self.popand(None)?;
                    self.inst[op1.last].next = op2.first;
                    self.pushand(op1.first, op2.last)?;
                }
                STAR => {
                    let op2 = self.popand(Some('*'))?;
                    let inst1 = self.newinst(OR);
                    self.inst[op2.last].next = inst1;
                    self.inst[inst1].right = op2.first;
                    self.pushand(inst1, inst1)?;
                }
                PLUS => {
                    let op2 = self.popand(Some('+'))?;
                    let inst1 = self.newinst(OR);
                    self.inst[op2.last].next = inst1;
                    self.inst[inst1].right = op2.first;
                    self.pushand(op2.first, inst1)?;
                }
                QUEST => {
                    let op2 = self.popand(Some('?'))?;
                    let inst1 = self.newinst(OR);
                    let inst2 = self.newinst(NOP);
                    self.inst[inst1].next = inst2;
                    self.inst[inst1].right = op2.first;
                    self.inst[op2.last].next = inst2;
                    self.pushand(inst1, inst2)?;
                }
                _ => return Err("unknown operator in evaluntil".into()),
            }
        }
        Ok(())
    }

    fn newclass(&mut self) -> Result<usize> {
        if self.class.len() >= NCLASS {
            return Err("too many character classes; increase Reprog.class size".into());
        }
        self.class.push(Reclass::default());
        Ok(self.class.len() - 1)
    }

    /// The next rune of the source, and whether it was quoted; NUL once
    /// it is done.
    fn nextc(&mut self) -> (char, bool) {
        if self.lexdone {
            return ('\0', true);
        }
        let mut r = self.chartorune();
        if r == '\\' {
            r = self.chartorune();
            return (r, true);
        }
        if r == '\0' {
            self.lexdone = true;
        }
        (r, false)
    }

    /// The source's next rune: NUL at its end, as at the end of a C
    /// string.
    fn chartorune(&mut self) -> char {
        let rest = self.expr.get(self.at..).unwrap_or_default();
        if rest.first().is_none_or(|&b| b == 0) {
            self.at += 1;
            return '\0';
        }
        let (r, n) = utf::decode(rest);
        self.at += n;
        r
    }

    fn lex(&mut self, literal: bool, dot_type: u16) -> Result<u16> {
        let (r, quoted) = self.nextc();
        self.yyrune = r;
        if literal || quoted {
            return Ok(if r == '\0' { END } else { RUNE });
        }
        Ok(match r {
            '\0' => END,
            '*' => STAR,
            '?' => QUEST,
            '+' => PLUS,
            '|' => OR,
            '.' => dot_type,
            '(' => LBRA,
            ')' => RBRA,
            '^' => BOL,
            '$' => EOL,
            '[' => self.bldcclass()?,
            _ => RUNE,
        })
    }

    fn bldcclass(&mut self) -> Result<u16> {
        let malformed = || String::from("malformed '[]'");
        // The '[' is already seen.
        let mut ty = CCLASS;
        self.yyclass = self.newclass()?;

        // Look ahead for negation; a negated class does not match
        // newline.
        let mut r: Vec<u32> = Vec::with_capacity(NCCRUNE);
        let (mut rune, mut quoted) = self.nextc();
        if !quoted && rune == '^' {
            ty = NCCLASS;
            (rune, quoted) = self.nextc();
            r.extend([u32::from('\n'), u32::from('\n')]);
        }

        // The class as a set of spans.
        while r.len() < NCCRUNE - 1 {
            if rune == '\0' {
                return Err(malformed());
            }
            if !quoted && rune == ']' {
                break;
            }
            if !quoted && rune == '-' {
                if r.is_empty() {
                    return Err(malformed());
                }
                (rune, quoted) = self.nextc();
                if (!quoted && rune == ']') || rune == '\0' {
                    return Err(malformed());
                }
                let n = r.len();
                r[n - 1] = u32::from(rune);
            } else {
                r.extend([u32::from(rune), u32::from(rune)]);
            }
            (rune, quoted) = self.nextc();
        }
        if r.len() >= NCCRUNE - 1 {
            return Err("char class too large; increase Reclass.spans size".into());
        }

        // Sorted on the start of each span, then merged where they
        // overlap or touch.
        let (pairs, _) = r.as_chunks::<2>();
        let mut pairs = pairs.to_vec();
        pairs.sort_unstable_by_key(|p| p[0]);
        let mut spans: Vec<[u32; 2]> = Vec::new();
        for p in pairs {
            match spans.last_mut() {
                Some(np) if p[0] <= np[1] + 1 => {
                    if p[1] >= np[1] {
                        np[1] = p[1];
                    }
                }
                _ => spans.push(p),
            }
        }
        self.class[self.yyclass].spans = spans;
        Ok(ty)
    }

    /// Takes the `NOP`s out of the chains.
    fn optimize(&mut self) {
        let mut i = 0;
        while i < self.inst.len() && self.inst[i].ty != END {
            let mut target = self.inst[i].next;
            while target != NIL && self.inst[target].ty == NOP {
                target = self.inst[target].next;
            }
            self.inst[i].next = target;
            i += 1;
        }
    }
}

fn cant(s: &str) -> String {
    format!("can't happen: {s}")
}

fn regcomp1(s: &[u8], literal: bool, dot_type: u16) -> Result<Reprog> {
    let mut p = Parser {
        inst: Vec::new(),
        class: Vec::new(),
        andstack: Vec::new(),
        atorstack: Vec::new(),
        subidstack: Vec::new(),
        cursubid: 0,
        lastwasand: false,
        nbra: 0,
        expr: s,
        at: 0,
        lexdone: false,
        yyrune: '\0',
        yyclass: 0,
    };

    // A low-priority operator primes the parser.
    p.pushator(START - 1)?;
    loop {
        let token = p.lex(literal, dot_type)?;
        if token == END {
            break;
        }
        if token & 0o300 == OPERATOR {
            p.operator(token)?;
        } else {
            p.operand(token)?;
        }
    }
    // Close with a low-priority operator, then force END.
    p.evaluntil(START)?;
    p.operand(END)?;
    p.evaluntil(START)?;
    if p.nbra > 0 {
        return Err("unmatched left paren".into());
    }
    // The first and only operand.
    let start = p.andstack.last().map_or(NIL, |n| n.first);
    p.optimize();
    Ok(Reprog {
        startinst: start,
        class: p.class,
        inst: p.inst,
    })
}

/// Compiles `s`; the error is what was wrong, as Plan 9's `regcomp` gives
/// it to `regerror`.
pub fn regcomp(s: &[u8]) -> Result<Reprog> {
    regcomp1(s, false, ANY)
}

/// Compiles `s` with every rune taken literally.
pub fn regcomplit(s: &[u8]) -> Result<Reprog> {
    regcomp1(s, true, ANY)
}

/// Compiles `s` with `.` matching newline too.
pub fn regcompnl(s: &[u8]) -> Result<Reprog> {
    regcomp1(s, false, ANYNL)
}
