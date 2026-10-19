//! `sed [-gn] [-e script] [-f sfile] [file ...]`: the stream editor, as
//! Plan 9's `sed.c`. The script is compiled to a list of commands, each
//! with up to two addresses (a line number, `$`, or a regular expression,
//! as `regexp(6)`); then each line of the files, or of standard input, is
//! read into the pattern space and run through the commands, and printed
//! unless `-n`. `-g` makes every `s` global.
//!
//! Plan 9's sed is kept as it is: `a`, `c` and `i` take their text after
//! `\` and a newline; `c` on a range prints its text at the start; `D`
//! starts the script again without reading; `N` at the end deletes;
//! `w` files are appended to; an empty regular expression is the last
//! one compiled. Its limits stand too: 512 commands, 50 labels of 7
//! runes, 10000 runes of text, 120 output files, lines of 8192 runes.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write as _;

use r::{Access, Bin, Bout, File, OpenFlags, OpenMode, Text, Whence};
use regexp::{Reprog, Resub, regcomp, rregexec};

r::main!(run);

/// Most nesting of `{}`.
const DEPTH: usize = 20;
/// Most commands.
const MAXCMDS: usize = 512;
/// Runes of `a`, `c`, `i` and `r` text and `s` replacements.
const ADDSIZE: usize = 10000;
/// Most `a` and `r` waiting at once.
const MAXADDS: usize = 20;
/// Runes in a line.
const LBSIZE: usize = 8192;
/// Most labels.
const LABSIZE: usize = 50;
/// Subexpressions recorded.
const MAXSUB: usize = 10;
/// Most output files.
const MAXFILES: usize = 120;
/// Bytes of a regular expression.
const EXPSIZE: usize = 512;

/// The marker before a quoted rune in a replacement (C's `Runemax`).
const QUOTE: char = '\u{10ffff}';

// The command codes.
const ACOM: u8 = 0o1;
const BCOM: u8 = 0o20;
const CCOM: u8 = 0o2;
const CDCOM: u8 = 0o25;
const CNCOM: u8 = 0o22;
const DCOM: u8 = 0o3;
const EQCOM: u8 = 0o13;
const GCOM: u8 = 0o27;
const CGCOM: u8 = 0o30;
const HCOM: u8 = 0o31;
const CHCOM: u8 = 0o32;
const ICOM: u8 = 0o4;
const LCOM: u8 = 0o5;
const NCOM: u8 = 0o12;
const PCOM: u8 = 0o10;
const CPCOM: u8 = 0o23;
const QCOM: u8 = 0o11;
const RCOM: u8 = 0o6;
const SCOM: u8 = 0o7;
const TCOM: u8 = 0o21;
const WCOM: u8 = 0o14;
const XCOM: u8 = 0o33;
const YCOM: u8 = 0o26;

const CGMES: &str = "command garbled:";
const TMMES: &str = "Too much text:";
const LTL: &str = "Label too long:";
const AD0MES: &str = "No addresses allowed:";
const AD1MES: &str = "Only one address allowed:";

/// An address: a line number, a regular expression, `$`, the last
/// regular expression, or none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Addr {
    #[default]
    None,
    Dol,
    Line(i64),
    /// A compiled expression, or none (`//` before any).
    Re(Option<usize>),
    Last,
}

/// A command. C keeps `re1`, `text` and `lb1` in a union.
#[derive(Debug, Clone, Default)]
struct SedCom {
    ad1: Addr,
    ad2: Addr,
    re1: Option<usize>,
    /// Text, a file name, or `y`'s table.
    text: Vec<char>,
    /// Where a branch goes.
    lb1: Option<usize>,
    rhs: Vec<char>,
    /// The output file of `w` and `s///w`.
    fcode: Option<usize>,
    command: u8,
    gfl: bool,
    pfl: u8,
    /// 1 on the line that started its range, 2 after.
    active: u8,
    negfl: bool,
}

#[derive(Debug, Clone, Default)]
struct Label {
    name: Vec<char>,
    /// Branches to it before it was defined, through their `lb1`.
    chain: Option<usize>,
    address: Option<usize>,
}

/// Where the script comes from.
enum Prog {
    Arg(Option<(&'static [u8], usize)>),
    File(Bin<File>),
}

/// What gline has read past a line's newline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Peek {
    Nothing,
    Eof,
    Rune(char),
}

#[allow(clippy::struct_excessive_bools)] // C's flags
struct Sed {
    // The compiled script.
    pspace: Vec<SedCom>,
    rep: usize,
    res: Vec<Reprog>,
    lastre: Option<usize>,
    ltab: Vec<Label>,
    /// Runes of text kept so far.
    addused: usize,
    cmpend: Vec<usize>,
    depth: usize,
    prog: Prog,
    /// The script line being compiled, and where.
    cline: Vec<char>,
    cp: usize,
    seof: char,
    fname: Vec<Vec<u8>>,
    fcode: Vec<Bout<File>>,

    // Running it.
    subexp: [Resub; MAXSUB],
    loc1: usize,
    loc2: usize,
    linebuf: Vec<char>,
    holdsp: Vec<char>,
    nflag: bool,
    gflag: bool,
    dolflag: bool,
    sflag: bool,
    jflag: bool,
    delflag: bool,
    lnum: i64,
    abuf: Vec<usize>,
    files: Vec<Option<&'static [u8]>>,
    f: Option<Bin<File>>,
    peekc: Peek,
}

const READ: OpenMode = OpenMode {
    access: Access::Read,
    flags: OpenFlags::empty(),
};

const WRITE: OpenMode = OpenMode {
    access: Access::Write,
    flags: OpenFlags::empty(),
};

fn runes(s: &[char]) -> String {
    s.iter().take_while(|&&c| c != '\0').collect()
}

impl Sed {
    /// Flushes the output, as Plan 9's bio does at exit.
    fn flushall(&mut self) {
        for b in &mut self.fcode {
            let _ = b.flush();
        }
    }

    fn errexit(&mut self) -> ! {
        self.flushall();
        r::exits(Some(b"error"))
    }

    /// C's `quit`: says what is wrong and exits.
    fn quit(&mut self, msg: &str) -> ! {
        let mut m = String::from("sed: ");
        m.push_str(msg);
        m.push('\n');
        let _ = r::write(2, m.as_bytes());
        self.errexit()
    }

    /// `msg` about the script line.
    fn quitl(&mut self, msg: &str) -> ! {
        let m = alloc::format!("{msg} {}", runes(&self.cline));
        self.quit(&m)
    }

    /// "`what` command garbled" about the script line.
    fn garbled(&mut self, what: &str) -> ! {
        let m = alloc::format!("{what} {CGMES} {}", runes(&self.cline));
        self.quit(&m)
    }

    /// The script line's rune at `i`; NUL past its end.
    fn at(&self, i: usize) -> char {
        self.cline.get(i).copied().unwrap_or('\0')
    }

    fn c(&self) -> char {
        self.at(self.cp)
    }

    /// The rune at the compile point, moving past it.
    fn next(&mut self) -> char {
        let c = self.c();
        self.cp += 1;
        c
    }

    /// Takes `n` runes of the text space.
    fn addspace(&mut self, n: usize) -> bool {
        if self.addused + n > ADDSIZE {
            return false;
        }
        self.addused += n;
        true
    }

    #[allow(clippy::too_many_lines)] // one switch, as C's
    fn fcomp(&mut self) {
        while self.rline() {
            self.cp = 0;
            'comploop: loop {
                while matches!(self.c(), ' ' | '\t') {
                    self.cp += 1;
                }
                if matches!(self.c(), '\0' | '#') {
                    break;
                }
                if self.c() == ';' {
                    self.cp += 1;
                    continue;
                }

                let rep = self.rep;
                let ad1 = self.address();
                self.pspace[rep].ad1 = ad1;
                if ad1 != Addr::None {
                    if ad1 == Addr::Last {
                        if self.lastre.is_none() {
                            self.quit("First RE may not be null");
                        }
                        self.pspace[rep].ad1 = Addr::Re(self.lastre);
                    }
                    if matches!(self.c(), ',' | ';') {
                        self.cp += 1;
                        let mut ad2 = self.address();
                        if ad2 == Addr::Last {
                            ad2 = Addr::Re(self.lastre);
                        }
                        self.pspace[rep].ad2 = ad2;
                    } else {
                        self.pspace[rep].ad2 = Addr::None;
                    }
                }
                while matches!(self.c(), ' ' | '\t') {
                    self.cp += 1;
                }

                loop {
                    let rep = self.rep;
                    match self.next() {
                        '!' => {
                            self.pspace[rep].negfl = true;
                            continue;
                        }
                        '{' => {
                            self.pspace[rep].command = BCOM;
                            self.pspace[rep].negfl = !self.pspace[rep].negfl;
                            self.cmpend.truncate(self.depth);
                            self.cmpend.push(rep);
                            self.depth += 1;
                            if self.depth > DEPTH {
                                self.quit("Too many {'s");
                            }
                            self.rep += 1;
                            if self.rep >= MAXCMDS {
                                self.quitl("Too many commands:");
                            }
                            if self.c() == '\0' {
                                break 'comploop;
                            }
                            continue 'comploop;
                        }
                        '}' => {
                            if self.pspace[rep].ad1 != Addr::None {
                                self.quitl(AD0MES);
                            }
                            if self.depth == 0 {
                                self.quit("Too many }'s");
                            }
                            self.depth -= 1;
                            let open = self.cmpend[self.depth];
                            self.pspace[open].lb1 = Some(rep);
                            if self.c() == '\0' {
                                break 'comploop;
                            }
                            continue 'comploop;
                        }
                        '=' => {
                            self.pspace[rep].command = EQCOM;
                            self.one_address();
                        }
                        ':' => {
                            if self.pspace[rep].ad1 != Addr::None {
                                self.quitl(AD0MES);
                            }
                            while self.c() == ' ' {
                                self.cp += 1;
                            }
                            let mut name = Vec::new();
                            while !matches!(self.c(), '\0' | ';' | ' ' | '\t' | '#') {
                                name.push(self.next());
                                if name.len() >= 8 {
                                    self.quitl(LTL);
                                }
                            }
                            if name.is_empty() {
                                self.garbled(":");
                            }
                            let lpt = if let Some(l) = self.search(&name) {
                                if self.ltab[l].address.is_some() {
                                    self.quitl("Duplicate labels:");
                                }
                                l
                            } else {
                                self.newlabel(name, None);
                                self.ltab.len() - 1
                            };
                            self.ltab[lpt].address = Some(rep);
                            if self.c() == '#' {
                                break 'comploop;
                            }
                            // This slot is used again.
                            if !self.done(false) {
                                break 'comploop;
                            }
                            continue 'comploop;
                        }
                        c @ ('a' | 'c' | 'i') => {
                            self.pspace[rep].command = match c {
                                'a' => ACOM,
                                'c' => CCOM,
                                _ => ICOM,
                            };
                            if c != 'c' {
                                self.one_address();
                            }
                            if self.c() == '\\' {
                                self.cp += 1;
                            }
                            if self.next() != '\n' {
                                self.garbled(c.encode_utf8(&mut [0; 4]));
                            }
                            let t = self.stext();
                            self.pspace[rep].text = t;
                        }
                        'g' => self.pspace[rep].command = GCOM,
                        'G' => self.pspace[rep].command = CGCOM,
                        'h' => self.pspace[rep].command = HCOM,
                        'H' => self.pspace[rep].command = CHCOM,
                        c @ ('t' | 'b') => {
                            self.pspace[rep].command = if c == 't' { TCOM } else { BCOM };
                            self.branch(rep);
                        }
                        'n' => self.pspace[rep].command = NCOM,
                        'N' => self.pspace[rep].command = CNCOM,
                        'p' => self.pspace[rep].command = PCOM,
                        'P' => self.pspace[rep].command = CPCOM,
                        'r' => {
                            self.pspace[rep].command = RCOM;
                            self.one_address();
                            if self.next() != ' ' {
                                self.garbled("r");
                            }
                            let t = self.stext();
                            self.pspace[rep].text = t;
                        }
                        'd' => self.pspace[rep].command = DCOM,
                        'D' => {
                            self.pspace[rep].command = CDCOM;
                            self.pspace[rep].lb1 = Some(0);
                        }
                        'q' => {
                            self.pspace[rep].command = QCOM;
                            self.one_address();
                        }
                        'l' => self.pspace[rep].command = LCOM,
                        's' => self.scom(rep),
                        'w' => {
                            self.pspace[rep].command = WCOM;
                            if self.next() != ' ' {
                                self.garbled("w");
                            }
                            let f = self.wfile(false);
                            self.pspace[rep].fcode = Some(f);
                        }
                        'x' => self.pspace[rep].command = XCOM,
                        'y' => {
                            self.pspace[rep].command = YCOM;
                            self.seof = self.next();
                            if !self.ycomp(rep) {
                                self.garbled("y");
                            }
                        }
                        _ => self.quitl("Unrecognized command:"),
                    }
                    break;
                }
                // done:
                if !self.done(true) {
                    break;
                }
            }
        }
    }

    /// After a command: the next slot, and the rest of the line, which
    /// may be another command after `;`; false at its end. A label's slot
    /// (`advance` false) is used again.
    fn done(&mut self, advance: bool) -> bool {
        if advance {
            self.rep += 1;
            if self.rep >= MAXCMDS {
                self.quitl("Too many commands, last:");
            }
        }
        match self.next() {
            '\0' => false,
            ';' => true,
            _ => {
                let rest = runes(&self.cline[self.cp - 1..]);
                self.garbled(&rest)
            }
        }
    }

    fn one_address(&mut self) {
        if self.pspace[self.rep].ad2 != Addr::None {
            self.quitl(AD1MES);
        }
    }

    fn newlabel(&mut self, name: Vec<char>, chain: Option<usize>) {
        self.ltab.push(Label {
            name,
            chain,
            address: None,
        });
        if self.ltab.len() >= LABSIZE {
            self.quitl("Too many labels:");
        }
    }

    /// The label after `b` or `t`, or the end.
    fn branch(&mut self, rep: usize) {
        while self.c() == ' ' {
            self.cp += 1;
        }
        if matches!(self.c(), '\0' | ';') {
            // No label: jump to the end.
            if let Some(mut pt) = self.ltab[0].chain {
                while let Some(pt1) = self.pspace[pt].lb1 {
                    pt = pt1;
                }
                self.pspace[pt].lb1 = Some(rep);
            } else {
                self.ltab[0].chain = Some(rep);
            }
            return;
        }
        let mut name = Vec::new();
        while !matches!(self.c(), '\0' | ';') {
            name.push(self.next());
            if name.len() >= 8 {
                self.quitl(LTL);
            }
        }
        if let Some(lpt) = self.search(&name) {
            if let Some(a) = self.ltab[lpt].address {
                self.pspace[rep].lb1 = Some(a);
            } else {
                let mut pt = self.ltab[lpt].chain;
                while let Some(p) = pt {
                    match self.pspace[p].lb1 {
                        Some(p1) => pt = Some(p1),
                        None => break,
                    }
                }
                if let Some(p) = pt {
                    self.pspace[p].lb1 = Some(rep);
                }
            }
        } else {
            self.newlabel(name, Some(rep));
        }
    }

    fn scom(&mut self, rep: usize) {
        self.pspace[rep].command = SCOM;
        self.seof = self.next();
        let re = match self.compile() {
            Some(re) => Some(re),
            None if self.lastre.is_none() => self.quit("First RE may not be null."),
            None => self.lastre,
        };
        self.pspace[rep].re1 = re;
        match self.compsub() {
            Some(rhs) => self.pspace[rep].rhs = rhs,
            None => self.garbled("s"),
        }
        if self.c() == 'g' {
            self.cp += 1;
            self.pspace[rep].gfl = true;
        } else if self.gflag {
            self.pspace[rep].gfl = true;
        }
        if self.c() == 'p' {
            self.cp += 1;
            self.pspace[rep].pfl = 1;
        }
        if self.c() == 'P' {
            self.cp += 1;
            self.pspace[rep].pfl = 2;
        }
        if self.c() == 'w' {
            self.cp += 1;
            if self.next() != ' ' {
                self.garbled("s");
            }
            let f = self.wfile(true);
            self.pspace[rep].fcode = Some(f);
        }
    }

    /// The output file named next, opened once.
    fn wfile(&mut self, from_s: bool) -> usize {
        let name = self.text();
        if let Some(i) = self.fname.iter().rposition(|n| *n == name) {
            return i;
        }
        if self.fname.len() >= MAXFILES {
            if from_s {
                self.quit("Too many files in w commands 1");
            }
            let n = self.fname.len();
            r::eprint!("sed: Too many files in w commands 2 \n");
            r::eprint!("nfiles = {n}; MAXF = {MAXFILES}\n");
            self.errexit();
        }
        let fd = r::cstr(&name).and_then(|c| r::open(&c, WRITE).or_else(|_| r::create(&c, WRITE, 0o666)));
        let Ok(fd) = fd else {
            let m = alloc::format!("Cannot create {}", Text(&name));
            self.quit(&m)
        };
        let _ = r::seek(fd, 0, Whence::End);
        self.fname.push(name);
        self.fcode.push(Bout::new(File(fd)));
        self.fcode.len() - 1
    }

    /// A replacement: runes up to the delimiter, a quoted one after
    /// `QUOTE` (`\n` a newline).
    fn compsub(&mut self) -> Option<Vec<char>> {
        let mut rhs = Vec::new();
        loop {
            let mut r = self.next();
            if r == '\0' {
                return None;
            }
            if r == '\\' {
                if !self.addspace(1) {
                    return None;
                }
                rhs.push(QUOTE);
                r = self.next();
                if r == 'n' {
                    r = '\n';
                }
            } else if r == self.seof {
                if !self.addspace(1) {
                    return None;
                }
                return Some(rhs);
            }
            if !self.addspace(1) {
                return None;
            }
            rhs.push(r);
        }
    }

    /// The regular expression up to the delimiter; `None` if it is empty.
    fn compile(&mut self) -> Option<usize> {
        let mut c = self.next();
        if c == self.seof {
            return None;
        }
        let mut exp = Vec::new();
        let put = |s: &mut Sed, exp: &mut Vec<u8>, c: char| {
            if exp.len() >= EXPSIZE {
                s.quitl(TMMES);
            }
            exp.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
        };
        loop {
            if c == '\0' || c == '\n' {
                self.quitl(TMMES);
            }
            if c == '\\' {
                put(self, &mut exp, c);
                c = self.next();
                if c == 'n' {
                    c = '\n';
                }
            }
            put(self, &mut exp, c);
            c = self.next();
            if c == self.seof {
                break;
            }
        }
        match regcomp(&exp) {
            Ok(p) => {
                self.res.push(p);
                self.lastre = Some(self.res.len() - 1);
                self.lastre
            }
            // sed's regerror.
            Err(_) => self.garbled("r.e.-using"),
        }
    }

    /// Reads a line of the script into `cline`, keeping a `\` and what it
    /// quotes, a newline too; false at the end.
    fn rline(&mut self) -> bool {
        self.cline.clear();
        while let Some(mut r) = self.getrune() {
            if r == '\\' {
                if self.cline.len() <= LBSIZE {
                    self.cline.push(r);
                }
                let Some(c) = self.getrune() else { break };
                r = c;
            } else if r == '\n' {
                return true;
            }
            if self.cline.len() <= LBSIZE {
                self.cline.push(r);
            }
        }
        false
    }

    /// The script's next rune; an argument ends with a newline.
    fn getrune(&mut self) -> Option<char> {
        match &mut self.prog {
            Prog::Arg(curr) => {
                let (s, at) = (*curr)?;
                if at < s.len() {
                    let (r, n) = r::utf::decode(&s[at..]);
                    *curr = Some((s, at + n));
                    Some(r)
                } else {
                    // Fake an end of line.
                    *curr = None;
                    Some('\n')
                }
            }
            Prog::File(b) => b.getrune().ok().flatten(),
        }
    }

    fn newfile(&mut self, file: bool, name: &'static [u8]) {
        if file {
            let Ok(fd) = r::cstr(name).and_then(|c| r::open(&c, READ)) else {
                let m = alloc::format!("Cannot open pattern-file: {}\n", Text(name));
                self.quit(&m)
            };
            self.prog = Prog::File(Bin::new(File(fd)));
        } else {
            self.prog = Prog::Arg(Some((name, 0)));
        }
    }

    fn address(&mut self) -> Addr {
        let c = self.next();
        if c == '$' {
            return Addr::Dol;
        }
        if c == '/' {
            self.seof = c;
            return match self.compile() {
                Some(re) => Addr::Re(Some(re)),
                None => Addr::Last,
            };
        }
        if let Some(d) = c.to_digit(10) {
            let mut lno = i64::from(d);
            while let Some(d) = self.c().to_digit(10) {
                lno = lno * 10 + i64::from(d);
                self.cp += 1;
            }
            if lno == 0 {
                self.quit("line number 0 is illegal");
            }
            return Addr::Line(lno);
        }
        self.cp -= 1;
        Addr::None
    }

    /// A file name: to the end of the line, `\` quoting, blanks after a
    /// newline skipped.
    fn text(&mut self) -> Vec<u8> {
        while matches!(self.c(), ' ' | '\t') {
            self.cp += 1;
        }
        let mut p = String::new();
        while self.c() != '\0' {
            let mut r = self.next();
            if r == '\\' {
                r = self.next();
                if r == '\0' {
                    break;
                }
            }
            if r == '\n' {
                while matches!(self.c(), ' ' | '\t') {
                    self.cp += 1;
                }
            }
            p.push(r);
        }
        p.into_bytes()
    }

    /// Text: to the end of the line, `\` quoting, blanks after a newline
    /// skipped.
    fn stext(&mut self) -> Vec<char> {
        while matches!(self.c(), ' ' | '\t') {
            self.cp += 1;
        }
        let mut p = Vec::new();
        while self.c() != '\0' {
            if self.c() == '\\' {
                self.cp += 1;
                if self.c() == '\0' {
                    break;
                }
            }
            if !self.addspace(1) || self.addused >= ADDSIZE {
                self.quitl(TMMES);
            }
            let r = self.next();
            p.push(r);
            if r == '\n' {
                while matches!(self.c(), ' ' | '\t') {
                    self.cp += 1;
                }
            }
        }
        // Its NUL.
        self.addused += 1;
        p
    }

    fn search(&self, name: &[char]) -> Option<usize> {
        self.ltab.iter().position(|l| l.name == name)
    }

    /// Points each branch at its label.
    fn dechain(&mut self) {
        for l in 0..self.ltab.len() {
            let Some(address) = self.ltab[l].address else {
                let m = alloc::format!("Undefined label: {}", runes(&self.ltab[l].name));
                self.quit(&m)
            };
            let mut rptr = self.ltab[l].chain;
            while let Some(p) = rptr {
                let next = self.pspace[p].lb1;
                self.pspace[p].lb1 = Some(address);
                rptr = next;
            }
        }
    }

    /// `y`: the table of what each rune up to the largest becomes, after
    /// that largest.
    fn ycomp(&mut self, rep: usize) -> bool {
        let delim = self.seof;
        let mut highc = 0u32;
        let mut tsp = self.cp;
        while self.at(tsp) != delim {
            if self.at(tsp) == '\\' {
                tsp += 1;
            }
            if matches!(self.at(tsp), '\n' | '\0') {
                return false;
            }
            highc = highc.max(u32::from(self.at(tsp)));
            tsp += 1;
        }
        tsp += 1;
        let mut map: Vec<char> = (0..=highc).map(|i| char::from_u32(i).unwrap_or('\0')).collect();
        let mut sp = self.cp;
        loop {
            let mut c = self.at(sp);
            sp += 1;
            if c == delim {
                break;
            }
            if c == '\\' && self.at(sp) == 'n' {
                sp += 1;
                c = '\n';
            }
            let mut to = self.at(tsp);
            tsp += 1;
            if to == '\\' && self.at(tsp) == 'n' {
                to = '\n';
                tsp += 1;
            }
            if to == delim || to == '\0' {
                return false;
            }
            let i = c as usize;
            if i >= map.len() {
                // Past C's table.
                let from = u32::try_from(map.len()).unwrap_or(u32::MAX);
                map.extend((from..=u32::from(c)).map(|i| char::from_u32(i).unwrap_or('\0')));
            }
            map[i] = to;
        }
        if self.at(tsp) != delim {
            return false;
        }
        self.cp = tsp + 1;
        let mut text = vec![char::from_u32(highc).unwrap_or('\0')];
        text.extend(map);
        self.pspace[rep].text = text;
        true
    }

    fn execute(&mut self) {
        while self.gline(0) {
            let mut ipc = 0;
            while self.pspace[ipc].command != 0 {
                if !self.executable(ipc) {
                    ipc += 1;
                    continue;
                }
                self.command(ipc);
                if self.delflag {
                    break;
                }
                if self.jflag {
                    self.jflag = false;
                    match self.pspace[ipc].lb1 {
                        Some(t) => ipc = t,
                        None => break,
                    }
                } else {
                    ipc += 1;
                }
            }
            if !self.nflag && !self.delflag {
                self.putline(0, false);
            }
            if !self.abuf.is_empty() {
                self.arout();
            }
            self.delflag = false;
        }
    }

    /// Whether command `ipc` applies to this line.
    fn executable(&mut self, ipc: usize) -> bool {
        let negfl = self.pspace[ipc].negfl;
        if self.pspace[ipc].active != 0 {
            // The first address matched: take lines to the second.
            if self.pspace[ipc].active == 1 {
                self.pspace[ipc].active = 2;
            }
            match self.pspace[ipc].ad2 {
                Addr::None => self.pspace[ipc].active = 0,
                Addr::Dol => return !negfl,
                Addr::Line(l) => {
                    if self.lnum <= l {
                        if l == self.lnum {
                            self.pspace[ipc].active = 0;
                        }
                        return !negfl;
                    }
                    self.pspace[ipc].active = 0;
                    return negfl;
                }
                Addr::Re(re) => {
                    if self.matches(re, 0) {
                        self.pspace[ipc].active = 0;
                    }
                    return !negfl;
                }
                Addr::Last => self.quit("Internal error"),
            }
        }
        let hit = match self.pspace[ipc].ad1 {
            Addr::None => return !negfl,
            Addr::Dol => {
                if self.dolflag {
                    return !negfl;
                }
                false
            }
            Addr::Line(l) => l == self.lnum,
            Addr::Re(re) => self.matches(re, 0),
            Addr::Last => self.quit("Internal error"),
        };
        if hit {
            self.pspace[ipc].active = 1;
            return !negfl;
        }
        negfl
    }

    /// Whether `pattern` matches the line from `from`; where, in `loc1`
    /// and `loc2`.
    fn matches(&mut self, pattern: Option<usize>, from: usize) -> bool {
        let Some(p) = pattern else { return false };
        self.subexp[0] = Resub {
            sp: Some(from),
            ep: None,
        };
        if rregexec(&self.res[p], &self.linebuf, &mut self.subexp) > 0 {
            self.loc1 = self.subexp[0].sp.unwrap_or(0);
            self.loc2 = self.subexp[0].ep.unwrap_or(0);
            return true;
        }
        self.loc1 = 0;
        self.loc2 = 0;
        false
    }

    fn substitute(&mut self, ipc: usize) -> bool {
        let re = self.pspace[ipc].re1;
        if !self.matches(re, 0) {
            return false;
        }
        // Some patterns, '^' or '$', match nothing: a global substitute
        // steps over such a match, so as not to loop.
        self.sflag = true;
        let rhs = core::mem::take(&mut self.pspace[ipc].rhs);
        if self.pspace[ipc].gfl {
            loop {
                let len = self.loc2 - self.loc1;
                self.dosub(&rhs);
                if self.loc2 >= self.linebuf.len() {
                    break;
                }
                if len == 0 {
                    self.loc2 += 1;
                }
                if self.loc2 >= self.linebuf.len() {
                    break;
                }
                if !self.matches(re, self.loc2) {
                    break;
                }
            }
        } else {
            self.dosub(&rhs);
        }
        self.pspace[ipc].rhs = rhs;
        true
    }

    /// Replaces the match with `rhsbuf`; `loc2` is then after the
    /// replacement.
    fn dosub(&mut self, rhsbuf: &[char]) {
        let too_long = |g: &Vec<char>| {
            if g.len() >= LBSIZE {
                r::eprint!("sed: Output line too long.\n");
            }
        };
        let mut genbuf: Vec<char> = self.linebuf[..self.loc1].to_vec();
        let mut rp = rhsbuf.iter().copied();
        while let Some(mut c) = rp.next() {
            if c == '&' {
                self.place(&mut genbuf, self.loc1, self.loc2);
                continue;
            }
            if c == QUOTE {
                c = rp.next().unwrap_or('\0');
                if ('1'..='9').contains(&c) {
                    let n = c as usize - '0' as usize;
                    let (Some(a), Some(b)) = (self.subexp[n].sp, self.subexp[n].ep) else {
                        r::eprint!("sed: Invalid back reference \\{n}\n");
                        self.errexit();
                    };
                    self.place(&mut genbuf, a, b);
                    continue;
                }
            }
            genbuf.push(c);
            too_long(&genbuf);
        }
        let rest = self.loc2;
        self.loc2 = genbuf.len();
        for &c in &self.linebuf[rest..] {
            genbuf.push(c);
            too_long(&genbuf);
        }
        self.linebuf = genbuf;
    }

    fn place(&self, sp: &mut Vec<char>, l1: usize, l2: usize) {
        for &c in &self.linebuf[l1..l2] {
            sp.push(c);
            if sp.len() >= LBSIZE {
                r::eprint!("sed: Output line too long.\n");
            }
        }
    }

    fn out(&mut self) -> &mut Bout<File> {
        &mut self.fcode[0]
    }

    fn putrunes(&mut self, f: usize, s: &[char]) {
        for &c in s {
            let _ = self.fcode[f].putrune(c);
        }
        let _ = self.fcode[f].putc(b'\n');
    }

    /// Writes the line to file `f`: as far as the first newline, if
    /// `first` (`P`, whose runes C writes with `Bputc`, a byte each, cut
    /// short; these are whole).
    fn putline(&mut self, f: usize, first: bool) {
        let line = core::mem::take(&mut self.linebuf);
        let end = if first {
            line.iter().position(|&c| c == '\n').unwrap_or(line.len())
        } else {
            line.len()
        };
        self.putrunes(f, &line[..end]);
        self.linebuf = line;
    }

    #[allow(clippy::too_many_lines)] // one switch, as C's
    fn command(&mut self, ipc: usize) {
        match self.pspace[ipc].command {
            ACOM | RCOM => {
                self.abuf.push(ipc);
                if self.abuf.len() >= MAXADDS {
                    let what = if self.pspace[ipc].command == ACOM {
                        "appends"
                    } else {
                        "reads"
                    };
                    let m = alloc::format!("sed: Too many {what} after line {}\n", self.lnum);
                    self.quit(&m);
                }
            }
            CCOM => {
                self.delflag = true;
                if self.pspace[ipc].active == 1 {
                    let t = core::mem::take(&mut self.pspace[ipc].text);
                    self.putrunes(0, &t);
                    self.pspace[ipc].text = t;
                }
            }
            DCOM => self.delflag = true,
            CDCOM => {
                let Some(nl) = self.linebuf.iter().position(|&c| c == '\n') else {
                    self.delflag = true;
                    return;
                };
                self.linebuf.drain(..=nl);
                self.jflag = true;
            }
            EQCOM => {
                let n = self.lnum;
                let _ = self.out().print(format_args!("{n}\n"));
            }
            GCOM => self.linebuf.clone_from(&self.holdsp),
            CGCOM => {
                self.linebuf.push('\n');
                let room = (LBSIZE - 1).saturating_sub(self.linebuf.len());
                let h = &self.holdsp[..self.holdsp.len().min(room)];
                self.linebuf.extend_from_slice(h);
            }
            HCOM => self.holdsp.clone_from(&self.linebuf),
            CHCOM => {
                self.holdsp.push('\n');
                let room = (LBSIZE - 1).saturating_sub(self.holdsp.len());
                let l = &self.linebuf[..self.linebuf.len().min(room)];
                self.holdsp.extend_from_slice(l);
            }
            ICOM => {
                let t = core::mem::take(&mut self.pspace[ipc].text);
                self.putrunes(0, &t);
                self.pspace[ipc].text = t;
            }
            BCOM => self.jflag = true,
            LCOM => self.lcom(),
            NCOM => {
                if !self.nflag {
                    self.putline(0, false);
                }
                if !self.abuf.is_empty() {
                    self.arout();
                }
                if !self.gline(0) {
                    self.delflag = true;
                }
            }
            CNCOM => {
                if !self.abuf.is_empty() {
                    self.arout();
                }
                self.linebuf.push('\n');
                let at = self.linebuf.len();
                if !self.gline(at) {
                    self.delflag = true;
                }
            }
            PCOM => self.putline(0, false),
            CPCOM => self.putline(0, true),
            QCOM => {
                if !self.nflag {
                    self.putline(0, false);
                }
                if !self.abuf.is_empty() {
                    self.arout();
                }
                self.flushall();
                r::exits(None);
            }
            SCOM => {
                let done = self.substitute(ipc);
                if done && self.pspace[ipc].pfl != 0 {
                    self.putline(0, self.pspace[ipc].pfl == 2);
                }
                if done && let Some(f) = self.pspace[ipc].fcode {
                    self.putline(f, false);
                }
            }
            TCOM => {
                if self.sflag {
                    self.sflag = false;
                    self.jflag = true;
                }
            }
            WCOM => {
                if let Some(f) = self.pspace[ipc].fcode {
                    self.putline(f, false);
                }
            }
            XCOM => core::mem::swap(&mut self.linebuf, &mut self.holdsp),
            YCOM => {
                let t = &self.pspace[ipc].text;
                let high = t.first().map_or(0, |&c| c as usize);
                for c in &mut self.linebuf {
                    let i = *c as usize;
                    if i <= high {
                        *c = t[i + 1];
                    }
                }
            }
            _ => {}
        }
    }

    /// `l`: the line with what is not printable ASCII escaped, folded at
    /// 72 columns.
    fn lcom(&mut self) {
        let line = core::mem::take(&mut self.linebuf);
        let mut col = 0;
        let mut last = '\0';
        let mut s = String::new();
        let mut put = |s: &mut String, ch: char| {
            s.push(ch);
            col += 1;
            if col > 72 {
                s.push_str("\\\n");
                col = 0;
            }
        };
        for &c in &line {
            last = c;
            if (' '..'\u{7f}').contains(&c) && c != '\\' {
                put(&mut s, c);
            } else {
                for t in trans(c).chars() {
                    put(&mut s, t);
                }
            }
        }
        if last == ' ' {
            s.push_str("\\n");
        }
        s.push('\n');
        let _ = self.out().write(s.as_bytes());
        self.linebuf = line;
    }

    /// The `a` text and `r` files waiting, out.
    fn arout(&mut self) {
        let pending = core::mem::take(&mut self.abuf);
        for ipc in pending {
            let t = self.pspace[ipc].text.clone();
            if self.pspace[ipc].command == ACOM {
                self.putrunes(0, &t);
                continue;
            }
            let name: String = t.iter().collect();
            let Ok(fd) = r::cstr(name.as_bytes()).and_then(|c| r::open(&c, READ)) else {
                continue;
            };
            let mut buf = [0u8; 8192];
            while let Ok(n @ 1..) = r::read(fd, &mut buf) {
                let _ = self.out().write(&buf[..n]);
            }
            let _ = r::close(fd);
        }
    }

    /// Reads the next line into the pattern space from `addr` on, across
    /// the files; false at the end.
    fn gline(&mut self, addr: usize) -> bool {
        if self.f.is_none() && !self.opendata() {
            return false;
        }
        self.sflag = false;
        self.lnum += 1;
        loop {
            self.linebuf.truncate(addr);
            let mut c = match core::mem::replace(&mut self.peekc, Peek::Nothing) {
                Peek::Rune(r) => Some(r),
                Peek::Eof => None,
                Peek::Nothing => self.getdata(),
            };
            while let Some(r) = c {
                if r == '\n' {
                    let p = self.getdata();
                    if p.is_none() && self.files.is_empty() {
                        self.dolflag = true;
                    }
                    // A NUL read ahead is lost, as C's is.
                    self.peekc = match p {
                        None => Peek::Eof,
                        Some('\0') => Peek::Nothing,
                        Some(r) => Peek::Rune(r),
                    };
                    return true;
                }
                if r != '\0' && self.linebuf.len() < LBSIZE {
                    self.linebuf.push(r);
                }
                c = self.getdata();
            }
            // The last line, without its newline.
            if self.linebuf.len() != addr {
                self.peekc = Peek::Eof;
                if self.files.is_empty() {
                    self.dolflag = true;
                }
                return true;
            }
            self.peekc = Peek::Nothing;
            self.f = None;
            if !self.opendata() {
                return false;
            }
        }
    }

    fn getdata(&mut self) -> Option<char> {
        self.f.as_mut()?.getrune().ok().flatten()
    }

    /// Opens the next input file.
    fn opendata(&mut self) -> bool {
        if self.files.is_empty() {
            return false;
        }
        match self.files.remove(0) {
            Some(name) => {
                let Ok(fd) = r::cstr(name).and_then(|c| r::open(&c, READ)) else {
                    let m = alloc::format!("Can't open {}", Text(name));
                    self.quit(&m)
                };
                self.f = Some(Bin::new(File(fd)));
            }
            None => self.f = Some(Bin::new(r::STDIN)),
        }
        true
    }
}

/// How `l` shows `c`.
fn trans(c: char) -> String {
    match c {
        '\u{8}' => "\\b".into(),
        '\n' => "\\n".into(),
        '\r' => "\\r".into(),
        '\t' => "\\t".into(),
        '\\' => "\\\\".into(),
        _ => {
            let mut s = String::new();
            let _ = write!(s, "\\x{:04x}", u32::from(c) & 0xffff);
            s
        }
    }
}

#[allow(clippy::unnecessary_wraps)] // the signature `main!` wants
fn run(cmdline: r::Args) -> Result<(), &'static str> {
    let argv: Vec<&'static [u8]> = cmdline.iter().collect();
    let mut sed = Sed {
        pspace: vec![SedCom::default(); MAXCMDS],
        rep: 0,
        res: Vec::new(),
        lastre: None,
        ltab: vec![Label::default()],
        addused: 0,
        cmpend: Vec::new(),
        depth: 0,
        prog: Prog::Arg(None),
        cline: Vec::new(),
        cp: 0,
        seof: '\0',
        fname: vec![Vec::new()],
        fcode: vec![Bout::new(r::STDOUT)],
        subexp: [Resub::default(); MAXSUB],
        loc1: 0,
        loc2: 0,
        linebuf: Vec::new(),
        holdsp: Vec::new(),
        nflag: false,
        gflag: false,
        dolflag: false,
        sflag: false,
        jflag: false,
        delflag: false,
        lnum: 0,
        abuf: Vec::new(),
        files: Vec::new(),
        f: None,
        peekc: Peek::Nothing,
    };
    if argv.len() == 1 {
        return Ok(());
    }
    // ARGBEGIN, by hand: `-e` and `-f` as the last argument are missing
    // their script, even with it attached.
    let mut compfl = false;
    let mut i = 1;
    while i < argv.len() && argv[i].len() > 1 && argv[i][0] == b'-' {
        if argv[i] == b"--" {
            i += 1;
            break;
        }
        let mut at = 1;
        while at < argv[i].len() {
            let (c, n) = r::utf::decode(&argv[i][at..]);
            at += n;
            match c {
                'e' | 'f' => {
                    if argv.len() - i <= 1 {
                        sed.quit("missing pattern");
                    }
                    let arg = if at < argv[i].len() {
                        &argv[i][at..]
                    } else {
                        i += 1;
                        argv[i]
                    };
                    at = usize::MAX;
                    sed.newfile(c == 'f', arg);
                    sed.fcomp();
                    compfl = true;
                }
                'g' => sed.gflag = true,
                'n' => sed.nflag = true,
                _ => r::eprint!("sed: Unknown flag: {c}\n"),
            }
        }
        i += 1;
    }
    let mut rest = argv.get(i..).unwrap_or_default().iter().copied();
    if !compfl {
        let Some(script) = rest.next() else {
            sed.quit("missing pattern")
        };
        sed.newfile(false, script);
        sed.fcomp();
    }
    if sed.depth > 0 {
        sed.quit("Too many {'s");
    }
    sed.ltab[0].address = Some(sed.rep);
    sed.dechain();
    sed.files = rest.map(Some).collect();
    if sed.files.is_empty() {
        sed.files.push(None);
    }
    sed.execute();
    sed.flushall();
    Ok(())
}
