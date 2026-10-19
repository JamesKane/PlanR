//! `sort [-cmuvbdfginrwM] [-o out] [-t c] [-T dir] [-l lines] [-k pos1[,pos2]] [+pos1 [-pos2]] ... [file ...]`:
//! sorts the lines of the files, or of standard input, as Plan 9's
//! `sort.c`. Each line gets a key, made from its fields (`-k`, or
//! `+pos -pos`) and then the whole line, each mapped by its flags:
//! `-n`/`-g` numbers, `-M` months, `-f` folded, `-d` dictionary order,
//! `-i` printable only, `-w` without blanks, `-r` reversed, `-b` from the
//! first non-blank. The keys are compared as bytes, by a radix sort.
//! `-u` keeps one of the lines with the same key, `-c` checks the order,
//! `-o` writes to a file; more than `-l` lines (100000) go through
//! sorted temporary files in `-T` (`/tmp`), merged.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]
// Plan 9's names.
#![allow(clippy::many_single_char_names)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::CStr;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use r::runetype::{is_alpha_rune, is_digit_rune, is_upper_rune, to_base_rune, to_lower_rune, to_upper_rune};
use r::utf::{self, is_space_rune};
use r::{Access, Bin, Bout, File, OpenFlags, OpenMode, Rerr, Text};

r::main!(run);

/// Lines kept in memory before a temporary file.
const NLINE: i64 = 100_000;
/// Temporary files merged at once.
const NMERGE: usize = 10;
/// Most fields.
const NFIELD: usize = 20;

// Flags of a field.
const BFLAG: u32 = 1 << 0;
const B1FLAG: u32 = 1 << 1;
const DFLAG: u32 = 1 << 2;
const FFLAG: u32 = 1 << 3;
const GFLAG: u32 = 1 << 4;
const IFLAG: u32 = 1 << 5;
const MFLAG: u32 = 1 << 6;
const NFLAG: u32 = 1 << 7;
const RFLAG: u32 = 1 << 8;
const WFLAG: u32 = 1 << 9;

const READ: OpenMode = OpenMode {
    access: Access::Read,
    flags: OpenFlags::empty(),
};
const WRITE: OpenMode = OpenMode {
    access: Access::Write,
    flags: OpenFlags::empty(),
};

/// A line, ending in a newline, and its key.
struct Line {
    key: Vec<u8>,
    line: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dokey {
    Plain,
    Dfi,
    Gn,
    M,
    R,
}

#[derive(Clone)]
struct Field {
    beg1: i32,
    beg2: i32,
    end1: i32,
    end2: i32,
    flags: u32,
    mapto: [u8; 256],
    dokey: Dokey,
}

impl Default for Field {
    fn default() -> Field {
        Field {
            beg1: 0,
            beg2: 0,
            end1: 0,
            end2: 0,
            flags: 0,
            mapto: [0; 256],
            dokey: Dokey::Plain,
        }
    }
}

#[allow(clippy::struct_excessive_bools)] // C's flags
struct Args {
    ofile: Option<&'static [u8]>,
    tname: Option<&'static [u8]>,
    tabchar: char,
    cflag: bool,
    uflag: bool,
    vflag: bool,
    nfield: usize,
    nfile: usize,
    field: Vec<Field>,
    linep: Vec<Line>,
    /// Lines for each temporary file.
    mline: i64,
}

/// How many temporary files there are, and the names' start, for the
/// note handler to remove them.
static NTEMP: AtomicUsize = AtomicUsize::new(0);
static TEMPNAME: AtomicPtr<Vec<u8>> = AtomicPtr::new(core::ptr::null_mut());

/// The name of temporary file `n`.
fn tempfile(n: usize) -> Vec<u8> {
    let p = TEMPNAME.load(Ordering::Relaxed);
    // SAFETY: set once, to a leaked box, before any temporary file.
    let prefix = unsafe { p.as_ref() }.map_or(&b""[..], |v| v.as_slice());
    let mut name = prefix.to_vec();
    name.extend_from_slice(format!("{n:04}").as_bytes());
    name
}

fn settempname(dir: Option<&[u8]>) {
    let dir = dir.unwrap_or(b"/tmp");
    if dir.len() >= 100 - 20 {
        r::eprintln!("temp file directory name is too long: {}", Text(dir));
        done(Some(b"tdir"));
    }
    let mut pid = r::getpid().unwrap_or(0);
    if pid == 0 {
        pid = u64::try_from(r::time::time()).unwrap_or(1).max(1);
    }
    let mut p = dir.to_vec();
    p.extend_from_slice(format!("/sort.{:04}.", pid % 10000).as_bytes());
    TEMPNAME.store(
        alloc::boxed::Box::into_raw(alloc::boxed::Box::new(p)),
        Ordering::Relaxed,
    );
}

/// Removes the temporary files and exits.
fn done(xs: Option<&[u8]>) -> ! {
    for i in 0..NTEMP.load(Ordering::Relaxed) {
        if let Ok(c) = r::cstr(&tempfile(i)) {
            let _ = r::remove(&c);
        }
    }
    r::exits(xs)
}

extern "C" fn notifyf(_: *mut r::Ureg, s: *const u8) {
    // SAFETY: the kernel passes the note's NUL-terminated text.
    let s = unsafe { CStr::from_ptr(s.cast()) }.to_bytes();
    if s == b"interrupt" || s == b"hangup" || s == b"kill" || s.starts_with(b"sys: write on closed pipe") {
        done(None);
    }
    r::eprintln!("sort: note: {}", Text(s));
    r::abort();
}

fn create(name: &[u8]) -> r::Fd {
    let Ok(f) = r::cstr(name).and_then(|c| r::create(&c, WRITE, 0o666)) else {
        r::eprintln!("sort: create {}: {Rerr}", Text(name));
        done(Some(b"create"))
    };
    f
}

impl Args {
    /// The next line of `b`, with its key; a last line gets a newline.
    fn newline(&self, b: &mut Bin<File>) -> Option<Line> {
        let p = b.line(b'\n').ok().flatten()?;
        let mut line = p.to_vec();
        if line.last() != Some(&b'\n') {
            r::eprintln!("sort: newline added");
            line.push(b'\n');
        }
        let key = self.buildkey(&line);
        Some(Line { key, line })
    }

    fn dofile(&mut self, b: &mut Bin<File>) {
        if self.cflag {
            let Some(mut ol) = self.newline(b) else { return };
            while let Some(l) = self.newline(b) {
                let n = kcmp(&ol.key, &l.key);
                if n.is_gt() || (n.is_eq() && self.uflag) {
                    r::eprintln!("sort: -c file not in sort");
                    done(Some(b"order"));
                }
                ol = l;
            }
            return;
        }
        while let Some(l) = self.newline(b) {
            if i64::try_from(self.linep.len()).unwrap_or(i64::MAX) >= self.mline {
                self.tempout();
            }
            self.linep.push(l);
        }
    }

    /// Sorts the lines so far into another temporary file.
    fn tempout(&mut self) {
        let mut a: Vec<usize> = (0..self.linep.len()).collect();
        sort4(&mut a, &|i| &self.linep[i].key);
        let n = NTEMP.fetch_add(1, Ordering::Relaxed);
        let tf = tempfile(n);
        let f = create(&tf);
        let mut tb = Bout::new(File(f));
        for &i in &a {
            lineout(&mut tb, &self.linep[i]);
        }
        let _ = tb.flush();
        drop(tb);
        let _ = r::close(f);
        self.linep.clear();
    }

    fn printout(&mut self, b: &mut Bout<File>) {
        let mut a: Vec<usize> = (0..self.linep.len()).collect();
        sort4(&mut a, &|i| &self.linep[i].key);
        let mut ok: Option<usize> = None;
        for &i in &a {
            let l = &self.linep[i];
            if self.uflag && ok.is_some_and(|o| kcmp(&self.linep[o].key, &l.key).is_eq()) {
                continue;
            }
            lineout(b, l);
            ok = Some(i);
        }
    }

    fn mergeout(&mut self, b: &mut Bout<File>) {
        let mut i = 0;
        while i < NTEMP.load(Ordering::Relaxed) {
            let mut n = NTEMP.load(Ordering::Relaxed) - i;
            if n > NMERGE {
                let t = NTEMP.fetch_add(1, Ordering::Relaxed);
                let tf = tempfile(t);
                let f = create(&tf);
                let mut tb = Bout::new(File(f));
                n = NMERGE;
                self.mergefiles(i, n, &mut tb);
                let _ = tb.flush();
                drop(tb);
                let _ = r::close(f);
            } else {
                self.mergefiles(i, n, b);
            }
            i += n;
        }
    }

    /// Merges temporary files `t` to `t+n` into `b`.
    fn mergefiles(&mut self, t: usize, n: usize, b: &mut Bout<File>) {
        // Each file's reader and the line at its head, and the files
        // with a head, in order.
        let mut ins: Vec<(Bin<File>, r::Fd)> = Vec::new();
        let mut heads: Vec<Option<Line>> = Vec::new();
        let mut mmp: Vec<usize> = Vec::new();
        for i in 0..n {
            let tf = tempfile(t + i);
            let Ok(f) = r::cstr(&tf).and_then(|c| r::open(&c, READ)) else {
                r::eprintln!("sort: reopen {}: {Rerr}", Text(&tf));
                done(Some(b"open"))
            };
            let mut bin = Bin::new(File(f));
            let l = self.newline(&mut bin);
            ins.push((bin, f));
            if l.is_some() {
                mmp.push(i);
            }
            heads.push(l);
        }
        let key = |heads: &[Option<Line>], m: usize| -> Vec<u8> {
            heads[m].as_ref().map(|l| l.key.clone()).unwrap_or_default()
        };
        let mut ok: Option<Vec<u8>> = None;
        loop {
            {
                let h = &heads;
                sort4(&mut mmp, &|m| h[m].as_ref().map_or(&[][..], |l| &l.key));
            }
            let Some(&m) = mmp.first() else { break };
            while let Some(l) = heads[m].take() {
                if !(self.uflag && ok.as_ref().is_some_and(|o| kcmp(o, &l.key).is_eq())) {
                    lineout(b, &l);
                    ok = Some(l.key);
                }
                let Some(next) = self.newline(&mut ins[m].0) else {
                    mmp.swap_remove(0);
                    break;
                };
                heads[m] = Some(next);
                if mmp.len() > 1 && kcmp(&key(&heads, mmp[0]), &key(&heads, mmp[1])).is_gt() {
                    break;
                }
            }
        }
        for (_, f) in ins {
            let _ = r::close(f);
        }
    }

    fn setfield(&mut self, n: usize, c: char) {
        let f = &mut self.field[n];
        f.flags |= match c {
            'b' => BFLAG,
            'd' => DFLAG,
            'f' => FFLAG,
            'g' => GFLAG,
            'i' => IFLAG,
            'M' => MFLAG,
            'n' => NFLAG,
            'r' => RFLAG,
            'w' => WFLAG,
            _ => {
                r::eprintln!("sort: unknown option: field.{c}");
                done(Some(b"option"))
            }
        };
    }

    /// A position, `n.m` and flags; `off1` and `off2` are taken from
    /// `n` and `m` (1 for `-k`'s, which count from 1).
    fn dofield(&mut self, s: &[u8], which: Which, off1: i32, off2: i32) {
        let mut s = s.iter().copied();
        let mut c = s.next().unwrap_or(0);
        let num = |c: &mut u8, s: &mut dyn Iterator<Item = u8>| {
            let mut n: i32 = 0;
            while c.is_ascii_digit() {
                n = n.saturating_mul(10).saturating_add(i32::from(*c - b'0'));
                *c = s.next().unwrap_or(0);
            }
            n
        };
        let nf = self.nfield;
        if c.is_ascii_digit() {
            let n = num(&mut c, &mut s) - off1;
            if n < 0 {
                r::eprintln!("sort: field offset must be positive");
                done(Some(b"option"));
            }
            match which {
                Which::Beg => self.field[nf].beg1 = n,
                Which::End => self.field[nf].end1 = n,
            }
        }
        if c == b'.' {
            c = s.next().unwrap_or(0);
            if c.is_ascii_digit() {
                let n = num(&mut c, &mut s) - off2;
                if n < 0 {
                    r::eprintln!("sort: character offset must be positive");
                    done(Some(b"option"));
                }
                match which {
                    Which::Beg => self.field[nf].beg2 = n,
                    Which::End => self.field[nf].end2 = n,
                }
            }
        }
        while c != 0 {
            self.setfield(nf, char::from(c));
            c = s.next().unwrap_or(0);
        }
    }

    fn newfield(&mut self) {
        let n = self.nfield + 1;
        if n >= NFIELD {
            r::eprintln!("sort: too many fields specified");
            done(Some(b"option"));
        }
        self.nfield = n;
        let f = &mut self.field[n];
        f.beg1 = -1;
        f.beg2 = -1;
        f.end1 = -1;
        f.end2 = -1;
    }

    fn printargs(&self) {
        let mut s = String::from("sort");
        for (i, f) in self.field[..=self.nfield].iter().enumerate() {
            let mut prefix = " -";
            if i > 0 {
                let pos = |s: &mut String, lead: &str, n: i32| {
                    s.push_str(lead);
                    if n >= 0 {
                        let _ = write!(s, "{n}");
                    } else {
                        s.push('*');
                    }
                };
                pos(&mut s, " +", f.beg1);
                pos(&mut s, ".", f.beg2);
                if f.flags & B1FLAG != 0 {
                    s.push('b');
                }
                pos(&mut s, " -", f.end1);
                pos(&mut s, ".", f.end2);
                prefix = "";
            }
            for (fl, c) in [
                (BFLAG, 'b'),
                (DFLAG, 'd'),
                (FFLAG, 'f'),
                (GFLAG, 'g'),
                (IFLAG, 'i'),
                (MFLAG, 'M'),
                (NFLAG, 'n'),
                (RFLAG, 'r'),
                (WFLAG, 'w'),
            ] {
                if f.flags & fl != 0 {
                    s.push_str(prefix);
                    s.push(c);
                }
            }
        }
        if self.cflag {
            s.push_str(" -c");
        }
        if self.uflag {
            s.push_str(" -u");
        }
        if let Some(o) = self.ofile {
            let _ = write!(s, " -o {}", Text(o));
        }
        if self.mline != NLINE {
            let _ = write!(s, " -l {}", self.mline);
        }
        r::eprintln!("{s}");
    }

    fn buildkey(&self, line: &[u8]) -> Vec<u8> {
        let ll = line.len() - 1;
        let mut k = Vec::new();
        for f in &self.field[1..=self.nfield] {
            let lp = skip(line, f.beg1, f.beg2, f.flags & B1FLAG != 0, false, self.tabchar).unwrap_or(ll);
            let lpe = skip(line, f.end1, f.end2, f.flags & BFLAG != 0, true, self.tabchar).unwrap_or(ll);
            dokey(&mut k, line, lp, lpe, f);
        }
        // The whole line last.
        if !self.uflag || k.is_empty() {
            dokey(&mut k, line, 0, ll, &self.field[0]);
        }
        if self.vflag {
            let _ = r::write(2, line);
            let mut s = String::new();
            for &b in &k {
                let _ = write!(s, " {b:02x}");
                if b == 0x00 || b == 0xff {
                    s.push('\n');
                }
            }
            let _ = r::write(2, s.as_bytes());
        }
        k
    }
}

#[derive(Clone, Copy)]
enum Which {
    Beg,
    End,
}

fn lineout(b: &mut Bout<File>, l: &Line) {
    if b.write(&l.line).is_err() {
        r::exits(Some(b"write"));
    }
}

/// Compares keys over the length of the shorter.
fn kcmp(a: &[u8], b: &[u8]) -> core::cmp::Ordering {
    let n = a.len().min(b.len());
    a[..n].cmp(&b[..n])
}

/// The rune at `l[i]`, and its length.
fn chartorune(l: &[u8], i: usize) -> (char, usize) {
    let (r, n) = utf::decode(&l[i..]);
    (r, n.max(1))
}

/// Where in `l` field `n1` (skipping blanks first if `bflag`) and rune
/// `n2` of it start, or with `endfield`, where field `n1` ends; `None` for
/// the end of the line. Fields are separated by `tc`, or runs of blanks.
fn skip(l: &[u8], n1: i32, n2: i32, bflag: bool, endfield: bool, tc: char) -> Option<usize> {
    if endfield && n1 < 0 {
        return None;
    }
    let blank = |c: u32| c == u32::from(b' ') || c == u32::from(b'\t');
    let nl = u32::from(b'\n');
    // C's `c = *l++`: `c` is what was at `p - 1`.
    let mut p = 0;
    let get = |p: &mut usize| {
        let c = u32::from(l[*p]);
        *p += 1;
        c
    };
    let mut c = get(&mut p);
    let tc = u32::from(tc);
    if tc != 0 {
        if tc < u32::from(utf::RUNE_SELF) {
            let mut i = n1;
            while i > 0 {
                while c != tc {
                    if c == nl {
                        return None;
                    }
                    c = get(&mut p);
                }
                if !(endfield && i == 1) {
                    c = get(&mut p);
                }
                i -= 1;
            }
        } else {
            p -= 1;
            let rune = |p: &mut usize| {
                let (r, n) = chartorune(l, *p);
                *p += n;
                u32::from(r)
            };
            let mut r = rune(&mut p);
            let mut i = n1;
            while i > 0 {
                while r != tc {
                    if r == nl {
                        return None;
                    }
                    r = rune(&mut p);
                }
                if !(endfield && i == 1) {
                    r = rune(&mut p);
                }
                i -= 1;
            }
            // As C's, `p` is now after the rune, and goes back a byte.
            c = r;
        }
    } else {
        let mut i = n1;
        while i > 0 {
            while blank(c) {
                c = get(&mut p);
            }
            while !blank(c) {
                if c == nl {
                    return None;
                }
                c = get(&mut p);
            }
            i -= 1;
        }
    }
    if bflag {
        while blank(c) {
            c = get(&mut p);
        }
    }
    p -= 1;
    let mut i = n2;
    while i > 0 {
        let c = l[p];
        if c < utf::RUNE_SELF {
            if c == b'\n' {
                return None;
            }
            p += 1;
        } else {
            p += chartorune(l, p).1;
        }
        i -= 1;
    }
    Some(p)
}

fn dokey(k: &mut Vec<u8>, line: &[u8], lp: usize, lpe: usize, f: &Field) {
    match f.dokey {
        Dokey::Plain => {
            if lpe > lp {
                k.extend_from_slice(&line[lp..lpe]);
            }
            k.push(0);
        }
        Dokey::R => {
            if lpe > lp {
                k.extend(line[lp..lpe].iter().map(|&b| !b));
            }
            k.push(!0);
        }
        Dokey::Gn => dokey_gn(k, &line[lp..lpe.max(lp)], f),
        Dokey::M => dokey_m(k, &line[lp..lpe.max(lp)], f),
        Dokey::Dfi => dokey_dfi(k, &line[lp..lpe.max(lp)], f),
    }
}

/// The states of reading a number.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ns {
    Start,
    Sign,
    Zero,
    Digit,
    Point,
    Fract,
    ZeroFract,
    Exp,
    ExpSign,
    ExpDigit,
}

/// A number as a key: a sign byte, the exponent (where the point is)
/// in two, the digits without the trailing zeros, and an end; all
/// inverted for a negative number or `-r`, and a zero alone between.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // bytes of the key, as C's
fn dokey_gn(k: &mut Vec<u8>, s: &[u8], f: &Field) {
    let start = k.len();
    k.extend_from_slice(&[0, 0, 0]);
    let mut nzero = 0usize;
    let mut exp: i32 = 0;
    let mut expsign = false;
    let mut dp: i32 = 0x4040;
    let mut rflag = f.flags & RFLAG != 0;
    let mut state = Ns::Start;
    let digit = |k: &mut Vec<u8>, c: u8, rflag: bool| k.push(if rflag { !c } else { c });
    for &c in s {
        match c {
            b' ' | b'\t' if matches!(state, Ns::Start | Ns::Sign) => {}
            b'+' | b'-' if state == Ns::Start => {
                state = Ns::Sign;
                if c == b'-' {
                    rflag = !rflag;
                }
            }
            b'+' | b'-' if state == Ns::Exp => {
                state = Ns::ExpSign;
                if c == b'-' {
                    expsign = true;
                }
            }
            b'0' => match state {
                Ns::Digit => {
                    digit(k, c, rflag);
                    nzero += 1;
                    dp += 1;
                }
                Ns::Fract => {
                    digit(k, c, rflag);
                    nzero += 1;
                }
                Ns::Start | Ns::Sign | Ns::Zero => state = Ns::Zero,
                Ns::ZeroFract | Ns::Point => {
                    dp -= 1;
                    state = Ns::ZeroFract;
                }
                Ns::ExpSign | Ns::Exp | Ns::ExpDigit => {
                    exp = exp.wrapping_mul(10);
                    state = Ns::ExpDigit;
                }
            },
            b'1'..=b'9' => match state {
                Ns::Zero | Ns::Start | Ns::Sign | Ns::Digit => {
                    digit(k, c, rflag);
                    nzero = 0;
                    dp += 1;
                    state = Ns::Digit;
                }
                Ns::ZeroFract | Ns::Point | Ns::Fract => {
                    digit(k, c, rflag);
                    nzero = 0;
                    state = Ns::Fract;
                }
                Ns::ExpSign | Ns::Exp | Ns::ExpDigit => {
                    exp = exp.wrapping_mul(10).wrapping_add(i32::from(c - b'0'));
                    state = Ns::ExpDigit;
                }
            },
            b'.' if matches!(state, Ns::Start | Ns::Sign) => state = Ns::Point,
            b'.' if state == Ns::Zero => state = Ns::ZeroFract,
            b'.' if state == Ns::Digit => state = Ns::Fract,
            b'e' | b'E' if f.flags & GFLAG != 0 && matches!(state, Ns::Digit | Ns::Fract) => state = Ns::Exp,
            _ => break,
        }
    }
    match state {
        // Zero.
        Ns::Start | Ns::Sign | Ns::Zero | Ns::ZeroFract | Ns::Point => {
            k.truncate(start);
            k.extend_from_slice(&[0x20, 0]);
            return;
        }
        Ns::ExpSign | Ns::Exp | Ns::ExpDigit => {
            if expsign {
                exp = -exp;
            }
            dp = dp.wrapping_add(exp);
            k.truncate(k.len() - nzero);
        }
        Ns::Digit | Ns::Fract => k.truncate(k.len() - nzero),
    }
    k.push(if rflag { !0 } else { 0 });
    let mut c = 0x30;
    if rflag {
        c = 0x10;
        dp = !dp;
    }
    k[start] = c;
    k[start + 1] = (dp >> 8) as u8;
    k[start + 2] = dp as u8;
}

const MONTH: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];

/// A month as a key: the first three letters, folded, as 10 to 21; 9 for
/// none.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_possible_wrap)] // a byte of the key, as C's
fn dokey_m(k: &mut Vec<u8>, s: &[u8], f: &Field) {
    let rflag = f.flags & RFLAG != 0;
    let mut place: Vec<u32> = Vec::new();
    let mut lp = 0;
    while lp < s.len() {
        let mut c = u32::from(s[lp]);
        if s[lp] >= utf::RUNE_SELF {
            let (r, n) = chartorune(s, lp);
            lp += n;
            c = u32::from(r);
        } else {
            lp += 1;
        }
        if c < 256 {
            c = u32::from(f.mapto[c as usize]);
            if c == 0 {
                continue;
            }
        }
        place.push(c);
        if place.len() < 3 {
            continue;
        }
        let m = MONTH
            .iter()
            .rposition(|m| m.chars().map(u32::from).eq(place.iter().copied()))
            .map_or(-1, |m| m as i32);
        let c = m + 10;
        k.push(if rflag { !c as u8 } else { c as u8 });
        break;
    }
    k.push(if rflag { !0 } else { 0 });
}

/// Folded, dictionary, printable or without blanks, as `mapto` maps ASCII
/// and Latin-1 and the rune tables the rest.
fn dokey_dfi(k: &mut Vec<u8>, s: &[u8], f: &Field) {
    let rflag = f.flags & RFLAG != 0;
    let mut lp = 0;
    while lp < s.len() {
        let mut c = u32::from(s[lp]);
        if s[lp] >= utf::RUNE_SELF {
            let (r, n) = chartorune(s, lp);
            lp += n;
            c = u32::from(r);
        } else {
            lp += 1;
        }
        // The common case is all in the table.
        if c != 0 && c < u32::from(utf::RUNE_SELF) {
            let m = f.mapto[c as usize];
            if m != 0 {
                k.push(m);
            }
            continue;
        }
        // The table does no -r for these; whether to ignore them is in
        // its last entry.
        if c != 0 && c < 256 {
            c = u32::from(f.mapto[c as usize]);
            if c == 0 {
                continue;
            }
        } else {
            if f.mapto[255] == 0 {
                continue;
            }
            let mut r = char::from_u32(c).unwrap_or(utf::RUNE_ERROR);
            if f.flags & FFLAG != 0 {
                r = to_lower_rune(to_base_rune(r));
            }
            if f.flags & DFLAG != 0 && !is_alpha_rune(r) && !is_digit_rune(r) && !is_space_rune(r) {
                continue;
            }
            if f.flags & WFLAG != 0 && is_space_rune(r) {
                continue;
            }
            c = u32::from(r);
        }
        let r = char::from_u32(c).unwrap_or(utf::RUNE_ERROR);
        let mut b = [0; utf::UTF_MAX];
        for &x in r.encode_utf8(&mut b).as_bytes() {
            k.push(if rflag { !x } else { x });
        }
    }
    k.push(if rflag { !0 } else { 0 });
}

#[allow(clippy::cast_possible_truncation)] // C's table is of bytes
fn makemapm(f: &mut Field, vflag: bool) {
    for i in 0..256u32 {
        let mut c = 1;
        if i == u32::from(b' ') || i == u32::from(b'\t') {
            c = 0;
        }
        if (u32::from(b'a')..=u32::from(b'z')).contains(&i) {
            c = i + u32::from(b'A') - u32::from(b'a');
        }
        if (u32::from(b'A')..=u32::from(b'Z')).contains(&i) {
            c = i;
        }
        f.mapto[i as usize] = c as u8;
    }
    if vflag {
        printmap(f);
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // C's table is of bytes
fn makemapd(f: &mut Field, vflag: bool) {
    for i in 0..256u32 {
        let mut c: i64 = i64::from(i);
        if f.flags & IFLAG != 0 && !(0o40..=0o176).contains(&c) {
            c = -1;
        }
        if f.flags & WFLAG != 0 && (c == 0x20 || c == 0x09) {
            c = -1;
        }
        if f.flags & DFLAG != 0 && c >= 0 {
            let b = c as u8;
            if !(b == b' ' || b == b'\t' || b.is_ascii_alphanumeric()) {
                let u = to_upper_rune(char::from(b));
                c = if is_upper_rune(u) { i64::from(u32::from(u)) } else { -1 };
            }
        }
        if f.flags & FFLAG != 0 && c >= 0 {
            let r = char::from_u32(c as u32).unwrap_or(utf::RUNE_ERROR);
            c = i64::from(u32::from(to_upper_rune(to_base_rune(r))));
        }
        if f.flags & RFLAG != 0 && c >= 0 && i > 0 && i < u32::from(utf::RUNE_SELF) {
            c = !c & 0xff;
        }
        if c < 0 {
            c = 0;
        }
        f.mapto[i as usize] = c as u8;
    }
    if vflag {
        printmap(f);
    }
}

fn printmap(f: &Field) {
    let mut s = String::new();
    for (i, &c) in f.mapto.iter().enumerate() {
        if i % 16 == 0 {
            s.push('\t');
        }
        let _ = write!(s, " {c:02x}");
        if i % 16 == 15 {
            s.push('\n');
        }
    }
    let _ = r::write(2, s.as_bytes());
}

/// Which `dokey` the flags of a field (less `b`) want; `None` for an
/// illegal combination. The folded, dictionary, printable and
/// blank-less ones are C's list, in which `-wr` alone is not.
fn choose(flags: u32) -> Option<Dokey> {
    const DFI: [u32; 29] = [
        DFLAG,
        DFLAG | FFLAG,
        DFLAG | FFLAG | IFLAG,
        DFLAG | FFLAG | IFLAG | RFLAG,
        DFLAG | FFLAG | IFLAG | RFLAG | WFLAG,
        DFLAG | FFLAG | IFLAG | WFLAG,
        DFLAG | FFLAG | RFLAG,
        DFLAG | FFLAG | RFLAG | WFLAG,
        DFLAG | FFLAG | WFLAG,
        DFLAG | IFLAG,
        DFLAG | IFLAG | RFLAG,
        DFLAG | IFLAG | RFLAG | WFLAG,
        DFLAG | IFLAG | WFLAG,
        DFLAG | RFLAG,
        DFLAG | RFLAG | WFLAG,
        DFLAG | WFLAG,
        FFLAG,
        FFLAG | IFLAG,
        FFLAG | IFLAG | RFLAG,
        FFLAG | IFLAG | RFLAG | WFLAG,
        FFLAG | IFLAG | WFLAG,
        FFLAG | RFLAG,
        FFLAG | RFLAG | WFLAG,
        FFLAG | WFLAG,
        IFLAG,
        IFLAG | RFLAG,
        IFLAG | RFLAG | WFLAG,
        IFLAG | WFLAG,
        WFLAG,
    ];
    match flags {
        0 => Some(Dokey::Plain),
        RFLAG => Some(Dokey::R),
        _ if flags & !(GFLAG | NFLAG | RFLAG) == 0 && flags & (GFLAG | NFLAG) != 0 => Some(Dokey::Gn),
        MFLAG | 0x140 => Some(Dokey::M),
        _ if DFI.contains(&flags) => Some(Dokey::Dfi),
        _ => None,
    }
}

/// C's argument loop: options are taken out of `argv` as they are read.
#[allow(clippy::too_many_lines)] // one loop, as C's
fn doargs(a: &mut Args, argv: &mut [Option<&'static [u8]>]) {
    let mut hadplus = false;
    let nargs = argv.len();
    let mut i = 1;
    while i < nargs {
        let Some(arg) = argv[i] else {
            i += 1;
            continue;
        };
        let mut s = arg.iter().copied().peekable();
        let c = s.next().unwrap_or(0);
        if c == b'-' {
            let c = s.peek().copied().unwrap_or(0);
            if c == 0 {
                // A forced end of the options.
                break;
            }
            argv[i] = None;
            if c == b'.' || c.is_ascii_digit() {
                if !hadplus {
                    a.newfield();
                }
                a.dofield(&arg[1..], Which::End, 0, 0);
                hadplus = false;
                i += 1;
                continue;
            }
            let mut at = 1;
            // The rest of the argument from `at`, or the next argument.
            let value = |at: &mut usize, i: &mut usize, argv: &mut [Option<&'static [u8]>]| -> Option<&'static [u8]> {
                if *at < arg.len() {
                    let v = &arg[*at..];
                    *at = arg.len();
                    Some(v)
                } else {
                    *i += 1;
                    let v = argv.get(*i).copied().flatten();
                    if *i < argv.len() {
                        argv[*i] = None;
                    }
                    v
                }
            };
            while at < arg.len() {
                let (c, n) = utf::decode(&arg[at..]);
                at += n;
                match c {
                    // The end of the options.
                    '-' => i = nargs,
                    'T' => a.tname = value(&mut at, &mut i, argv),
                    'o' => a.ofile = value(&mut at, &mut i, argv),
                    'k' => {
                        let Some(p) = value(&mut at, &mut i, argv) else {
                            continue;
                        };
                        a.newfield();
                        let (p, q) = match p.iter().position(|&b| b == b',') {
                            Some(comma) => (&p[..comma], Some(&p[comma + 1..])),
                            None => (p, None),
                        };
                        a.dofield(p, Which::Beg, 1, 1);
                        let nf = a.nfield;
                        let f = &mut a.field[nf];
                        if f.flags & BFLAG != 0 {
                            f.flags |= B1FLAG;
                            f.flags &= !BFLAG;
                        }
                        if let Some(q) = q {
                            a.dofield(q, Which::End, 1, 0);
                            let f = &mut a.field[nf];
                            if f.end2 <= 0 {
                                f.end1 += 1;
                            }
                        }
                        hadplus = false;
                    }
                    't' => {
                        if at < arg.len() {
                            let (r, n) = utf::decode(&arg[at..]);
                            a.tabchar = r;
                            at += n;
                        } else {
                            i += 1;
                            if let Some(Some(v)) = argv.get(i).copied() {
                                a.tabchar = utf::decode(v).0;
                                argv[i] = None;
                            }
                        }
                        if a.tabchar == '\n' {
                            r::eprintln!("aw come on, rob");
                            done(Some(b"rob"));
                        }
                    }
                    'c' => a.cflag = true,
                    'u' => a.uflag = true,
                    'v' => a.vflag = true,
                    'l' => {
                        if let Some(v) = value(&mut at, &mut i, argv) {
                            a.mline = r::num::atol(v);
                        }
                    }
                    'M' | 'b' | 'd' | 'f' | 'g' | 'i' | 'n' | 'r' | 'w' => {
                        if a.nfield > 0 {
                            r::eprintln!("sort: global field set after -k");
                        }
                        a.setfield(0, c);
                    }
                    // Ignored, but required by POSIX.
                    'm' => {}
                    _ => {
                        r::eprintln!("sort: unknown option: -{c}");
                        done(Some(b"option"));
                    }
                }
            }
            i += 1;
            continue;
        }
        if c == b'+' {
            argv[i] = None;
            let c = s.peek().copied().unwrap_or(0);
            if c == b'.' || c.is_ascii_digit() {
                a.newfield();
                a.dofield(&arg[1..], Which::Beg, 0, 0);
                let nf = a.nfield;
                let f = &mut a.field[nf];
                if f.flags & BFLAG != 0 {
                    f.flags |= B1FLAG;
                    f.flags &= !BFLAG;
                }
                hadplus = true;
                i += 1;
                continue;
            }
            r::eprintln!("sort: unknown option: +{}", char::from(c));
            done(Some(b"option"));
        }
        a.nfile += 1;
        i += 1;
    }

    let global = a.field[0].flags;
    for i in 0..=a.nfield {
        let vflag = a.vflag;
        let f = &mut a.field[i];
        // The global options are for fields that have none of their own.
        if f.flags == 0 {
            f.flags = global;
            if global & BFLAG != 0 {
                f.flags |= B1FLAG;
            }
        }
        let Some(d) = choose(f.flags & !(BFLAG | B1FLAG)) else {
            r::eprintln!("sort: illegal combination of flags: {:x}", f.flags);
            done(Some(b"option"))
        };
        f.dokey = d;
        match d {
            Dokey::M => makemapm(f, vflag),
            Dokey::Dfi => makemapd(f, vflag),
            _ => {}
        }
    }
    if a.nfile > 1 && a.cflag {
        r::eprintln!("sort: -c can have at most one input file");
        done(Some(b"option"));
    }
}

/// Lists shorter than this are sorted by bubbles, not radix.
const THRESHOLD: usize = 14;

/// Sorts `a` by `key` of each, as C's `sort4`.
fn sort4<'k>(a: &mut [usize], key: &dyn Fn(usize) -> &'k [u8]) {
    if a.len() > THRESHOLD {
        rsort4(a, key, 0);
    } else {
        bsort4(a, key, 0);
    }
}

/// A radix sort on byte `b` of the keys, keys that end before it first;
/// each part then by the next byte.
fn rsort4<'k>(a: &mut [usize], key: &dyn Fn(usize) -> &'k [u8], b: usize) {
    let byte = |e: usize| key(e).get(b).map_or(256, |&c| usize::from(c));
    // Count each byte, and the lowest and highest.
    let mut count = [0usize; 257];
    let (mut lowc, mut higc) = (256, 0);
    for &e in a.iter() {
        let c = byte(e);
        if c == 256 {
            count[256] += 1;
            continue;
        }
        if count[c] == 0 {
            lowc = lowc.min(c);
            higc = higc.max(c);
        }
        count[c] += 1;
    }
    // Where each part starts, and the parts there are.
    let mut part = [0usize; 257];
    let mut clist: Vec<(usize, usize)> = Vec::new();
    let ends = count[256];
    let mut t = ends;
    part[256] = 0;
    if lowc <= higc {
        for c in lowc..=higc {
            let n = count[c];
            if n > 0 {
                clist.push((n, c));
                part[c] = t;
                t += n;
            }
        }
    }
    // Chase each part's lowest misplaced entry round a cycle until it
    // comes back to where it started.
    for &(_, c) in &clist {
        while count[c] > 0 {
            let mut t1 = a[part[c]];
            loop {
                let n = byte(t1);
                let u = part[n];
                part[n] += 1;
                count[n] -= 1;
                let u1 = a[u];
                a[u] = t1;
                if n == c {
                    break;
                }
                t1 = u1;
            }
        }
    }
    // Each part by the next byte.
    let mut t = ends;
    for &(n, _) in &clist {
        let p = &mut a[t..t + n];
        if n > THRESHOLD {
            rsort4(p, key, b + 1);
        } else if n > 1 {
            bsort4(p, key, b + 1);
        }
        t += n;
    }
}

/// Bubbles, comparing from byte `b`: a key that ends there is no less
/// than any.
fn bsort4<'k>(a: &mut [usize], key: &dyn Fn(usize) -> &'k [u8], b: usize) {
    let greater = |x: usize, y: usize| {
        let (ka, kb) = (key(x), key(y));
        let n1 = ka.len().saturating_sub(b).min(kb.len().saturating_sub(b));
        if n1 == 0 {
            return false;
        }
        ka[b..b + n1] > kb[b..b + n1]
    };
    let mut j = 0;
    loop {
        let mut i = j;
        j += 1;
        if j >= a.len() {
            return;
        }
        if !greater(a[i], a[j]) {
            continue;
        }
        loop {
            a.swap(i, i + 1);
            if i == 0 {
                break;
            }
            i -= 1;
            if !greater(a[i], a[i + 1]) {
                break;
            }
        }
    }
}

#[allow(clippy::unnecessary_wraps)] // the signature `main!` wants
fn run(cmdline: r::Args) -> Result<(), &'static str> {
    let _ = r::notify(Some(notifyf));
    let mut argv: Vec<Option<&'static [u8]>> = cmdline.iter().map(Some).collect();
    let mut a = Args {
        ofile: None,
        tname: None,
        tabchar: '\0',
        cflag: false,
        uflag: false,
        vflag: false,
        nfield: 0,
        nfile: 0,
        field: vec![Field::default(); NFIELD],
        linep: Vec::new(),
        mline: NLINE,
    };
    doargs(&mut a, &mut argv);
    settempname(a.tname);
    if a.vflag {
        a.printargs();
    }
    for s in argv.iter().skip(1).copied().flatten() {
        if s == b"-" {
            a.dofile(&mut Bin::new(r::STDIN));
            continue;
        }
        let Ok(f) = r::cstr(s).and_then(|c| r::open(&c, READ)) else {
            r::eprintln!("sort: open {}: {Rerr}", Text(s));
            done(Some(b"open"))
        };
        a.dofile(&mut Bin::new(File(f)));
        let _ = r::close(f);
    }
    if a.nfile == 0 {
        a.dofile(&mut Bin::new(r::STDIN));
    }
    if a.cflag {
        done(None);
    }
    if a.vflag {
        r::eprintln!("=========");
    }
    let f = match a.ofile {
        Some(o) => create(o),
        None => 1,
    };
    let mut b = Bout::new(File(f));
    if NTEMP.load(Ordering::Relaxed) > 0 {
        a.tempout();
        a.mergeout(&mut b);
    } else {
        a.printout(&mut b);
    }
    let _ = b.flush();
    drop(b);
    done(None)
}
