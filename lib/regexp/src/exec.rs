//! The machine, as `regexec.c`, `rregexec.c` and `regaux.c`: two lists of
//! threads, this rune's and the next's, each an instruction and the
//! subexpressions so far. Every thread steps over each rune at once; a
//! thread that reaches an instruction another already has gives way to
//! the one that started earlier. The lists are small at first (10
//! threads) and larger if that runs out (250 for text, 50 for runes);
//! if that runs out too, the result is -1, as Plan 9's.

use alloc::vec;
use alloc::vec::Vec;

use r::utf;

use crate::{ANY, ANYNL, BOL, CCLASS, END, EOL, LBRA, NCCLASS, NIL, NSUBEXP, OR, RBRA, RUNE, Reprog, Resub};

const LISTSIZE: usize = 10;
const BIGLISTSIZE: usize = 25 * LISTSIZE;

/// What the machine runs over: UTF-8, a byte at a time below 0x80, or
/// runes. Either ends at a NUL or its end, as a C string does.
trait Subject {
    /// rregexec looks for its first rune only from a rune, not `^`, and
    /// starts a thread on every rune, even once it has matched.
    const RUNES: bool;
    /// The rune at `i` (NUL at the end) and how far it goes.
    fn rune(&self, i: usize) -> (u32, usize);
    /// Where `c` is first, from `i` on, before a NUL.
    fn find(&self, i: usize, c: char) -> Option<usize>;
    /// Whether the rune before `i` is a newline.
    fn after_newline(&self, i: usize) -> bool;
}

impl Subject for [u8] {
    const RUNES: bool = false;

    fn rune(&self, i: usize) -> (u32, usize) {
        match self.get(i) {
            None => (0, 1),
            Some(&b) if b < utf::RUNE_SELF => (u32::from(b), 1),
            Some(_) => {
                let (r, n) = utf::decode(&self[i..]);
                (u32::from(r), n)
            }
        }
    }

    fn find(&self, i: usize, c: char) -> Option<usize> {
        let s = self.get(i..)?;
        let s = &s[..s.iter().position(|&b| b == 0).unwrap_or(s.len())];
        utf::utf_rune(s, c).map(|at| i + at)
    }

    fn after_newline(&self, i: usize) -> bool {
        self[i - 1] == b'\n'
    }
}

impl Subject for [char] {
    const RUNES: bool = true;

    fn rune(&self, i: usize) -> (u32, usize) {
        (self.get(i).map_or(0, |&c| u32::from(c)), 1)
    }

    fn find(&self, i: usize, c: char) -> Option<usize> {
        let s = self.get(i..)?;
        let s = &s[..s.iter().position(|&r| r == '\0').unwrap_or(s.len())];
        s.iter().position(|&r| r == c).map(|at| i + at)
    }

    fn after_newline(&self, i: usize) -> bool {
        self[i - 1] == '\n'
    }
}

/// A list of threads: each an instruction (`NIL` after the last) and what
/// it has matched, `k` subexpressions a thread. C keeps all 32 a thread
/// and copies them only if more than the whole match is asked for; the
/// rest are never read, so they are not kept.
struct Relist {
    inst: Vec<usize>,
    se: Vec<Resub>,
    k: usize,
}

impl Relist {
    fn new(size: usize, ms: usize) -> Relist {
        let k = ms.clamp(1, NSUBEXP);
        Relist {
            inst: vec![NIL; size],
            se: vec![Resub::default(); size * k],
            k,
        }
    }

    fn se(&self, p: usize) -> &[Resub] {
        &self.se[p * self.k..(p + 1) * self.k]
    }

    fn se_mut(&mut self, p: usize) -> &mut [Resub] {
        &mut self.se[p * self.k..(p + 1) * self.k]
    }

    /// Sets where subexpression `id` of thread `p` starts or ends, if it
    /// is kept.
    fn mark(&mut self, p: usize, id: usize, s: usize, end: bool) {
        if let Some(m) = self.se_mut(p).get_mut(id) {
            if end {
                m.ep = Some(s);
            } else {
                m.sp = Some(s);
            }
        }
    }

    /// Adds a thread at `ip`, looking for one already there from `from`
    /// on; one that started earlier takes its place. The new end of the
    /// list, or `None` if the instruction was there.
    fn renewthread(&mut self, from: usize, ip: usize, sep: &[Resub]) -> Option<usize> {
        let mut p = from;
        while self.inst[p] != NIL {
            if self.inst[p] == ip {
                if sep[0].sp < self.se(p)[0].sp {
                    self.se_mut(p).copy_from_slice(sep);
                }
                return None;
            }
            p += 1;
        }
        self.inst[p] = ip;
        self.se_mut(p).copy_from_slice(sep);
        self.inst[p + 1] = NIL;
        Some(p + 1)
    }

    /// The same for the first instruction, starting at `sp` with nothing
    /// matched.
    fn renewemptythread(&mut self, ip: usize, sp: usize) {
        let mut p = 0;
        while self.inst[p] != NIL {
            if self.inst[p] == ip {
                if Some(sp) < self.se(p)[0].sp {
                    self.se_mut(p).fill(Resub::default());
                    self.se_mut(p)[0].sp = Some(sp);
                }
                return;
            }
            p += 1;
        }
        self.inst[p] = ip;
        self.se_mut(p).fill(Resub::default());
        self.se_mut(p)[0].sp = Some(sp);
        self.inst[p + 1] = NIL;
    }
}

/// Keeps `sp` as the match if it is to the left of what `mp` has, or as
/// far left and longer.
fn renewmatch(mp: &mut [Resub], sp: &[Resub]) {
    let Some(m0) = mp.first() else { return };
    if m0.sp.is_none() || sp[0].sp < m0.sp || (sp[0].sp == m0.sp && sp[0].ep > m0.ep) {
        for (i, m) in mp.iter_mut().enumerate() {
            *m = sp.get(i).copied().unwrap_or_default();
        }
    }
}

/// Where to run and what to look for first.
struct Junk {
    starttype: u16,
    startchar: char,
    starts: usize,
    eol: Option<usize>,
}

/// Whether `r` is in class `cp`.
fn in_class(prog: &Reprog, cp: usize, r: u32) -> bool {
    prog.class[cp].spans.iter().any(|&[lo, hi]| r >= lo && r <= hi)
}

/// 0 if no match, 1 if a match, -1 if the lists ran out.
#[allow(clippy::too_many_lines)] // one machine, as C's
fn regexec1<S: Subject + ?Sized>(prog: &Reprog, bol: &S, mp: &mut [Resub], junk: &Junk, size: usize) -> i32 {
    let j = junk;
    let ms = mp.len();
    let mut lists = [Relist::new(size, ms), Relist::new(size, ms)];
    let mut held = vec![Resub::default(); lists[0].k];
    // A list is full at two short of its end.
    let full = size - 2;
    let mut flag = 0;
    let mut matched = false;
    let mut checkstart = if S::RUNES {
        j.startchar != '\0'
    } else {
        j.starttype != 0
    };
    mp.fill(Resub::default());

    // Run the machine once for each rune, the terminal NUL included.
    let mut s = j.starts;
    loop {
        // A fast check for the first rune.
        if checkstart {
            match j.starttype {
                RUNE => match bol.find(s, j.startchar) {
                    Some(p) if Some(s) != j.eol => s = p,
                    _ => return i32::from(matched),
                },
                BOL if s != 0 => match bol.find(s, '\n') {
                    Some(p) if Some(s) != j.eol => s = p + 1,
                    _ => return i32::from(matched),
                },
                _ => {}
            }
        }
        let (r, n) = bol.rune(s);

        // Switch the lists.
        let tl = flag;
        flag ^= 1;
        let [first, second] = &mut lists;
        let (tlist, nlist) = if tl == 0 { (first, second) } else { (second, first) };
        nlist.inst[0] = NIL;

        // The first instruction goes on this list.
        if S::RUNES || !matched {
            tlist.renewemptythread(prog.startinst, s);
        }

        // Run the machine until this list is done.
        let mut tlp = 0;
        while tlist.inst[tlp] != NIL {
            let mut ip = tlist.inst[tlp];
            loop {
                let inst = prog.inst[ip];
                let step = match inst.ty {
                    RUNE => r == u32::from(inst.r),
                    LBRA => {
                        tlist.mark(tlp, inst.subid, s, false);
                        ip = inst.next;
                        continue;
                    }
                    RBRA => {
                        tlist.mark(tlp, inst.subid, s, true);
                        ip = inst.next;
                        continue;
                    }
                    ANY => r != u32::from('\n'),
                    ANYNL => true,
                    BOL => {
                        if s == 0 || bol.after_newline(s) {
                            ip = inst.next;
                            continue;
                        }
                        false
                    }
                    EOL => {
                        if Some(s) == j.eol || r == 0 || r == u32::from('\n') {
                            ip = inst.next;
                            continue;
                        }
                        false
                    }
                    CCLASS => in_class(prog, inst.cp, r),
                    NCCLASS => !in_class(prog, inst.cp, r),
                    OR => {
                        // The right choice later; this one now.
                        held.copy_from_slice(tlist.se(tlp));
                        if tlist.renewthread(tlp, inst.right, &held).is_some_and(|e| e >= full) {
                            return -1;
                        }
                        ip = inst.next;
                        continue;
                    }
                    END => {
                        matched = true;
                        tlist.se_mut(tlp)[0].ep = Some(s);
                        renewmatch(mp, tlist.se(tlp));
                        false
                    }
                    _ => false,
                };
                if step
                    && nlist
                        .renewthread(0, inst.next, tlist.se(tlp))
                        .is_some_and(|e| e >= full)
                {
                    return -1;
                }
                break;
            }
            tlp += 1;
        }
        if Some(s) == j.eol {
            break;
        }
        checkstart = (if S::RUNES {
            j.startchar != '\0'
        } else {
            j.starttype != 0
        }) && nlist.inst[0] == NIL;
        s += n;
        if r == 0 {
            break;
        }
    }
    i32::from(matched)
}

fn run<S: Subject + ?Sized>(prog: &Reprog, bol: &S, mp: &mut [Resub], big: usize) -> i32 {
    // Where the caller said to start and stop, if it did.
    let mut j = Junk {
        starttype: 0,
        startchar: '\0',
        starts: 0,
        eol: None,
    };
    if let Some(m) = mp.first() {
        if let Some(sp) = m.sp {
            j.starts = sp;
        }
        j.eol = m.ep;
    }
    let first = prog.inst[prog.startinst];
    if first.ty == RUNE && u32::from(first.r) < u32::from(utf::RUNE_SELF) {
        j.starttype = RUNE;
        j.startchar = first.r;
    }
    if first.ty == BOL {
        j.starttype = BOL;
    }
    let rv = regexec1(prog, bol, mp, &j, LISTSIZE);
    if rv >= 0 {
        return rv;
    }
    regexec1(prog, bol, mp, &j, big).max(-1)
}

/// Runs `prog` over `bol`, UTF-8 that ends at its end or a NUL: 1 if it
/// matches, 0 if not, -1 if the machine ran out of room. The match and its
/// subexpressions go in `mp`, as many as it holds; if `mp[0]` has a start
/// or end set, the search starts or stops there.
pub fn regexec(prog: &Reprog, bol: &[u8], mp: &mut [Resub]) -> i32 {
    run(prog, bol, mp, BIGLISTSIZE)
}

/// [`regexec`] over runes; offsets are in runes.
pub fn rregexec(prog: &Reprog, bol: &[char], mp: &mut [Resub]) -> i32 {
    run(prog, bol, mp, 5 * LISTSIZE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{regcomp, regcomplit, regcompnl};

    /// The match of `re` in `s`, and its first subexpression.
    fn m(re: &str, s: &str) -> Option<(&'static str, Option<String>)> {
        let p = regcomp(re.as_bytes()).unwrap();
        let mut sub = [Resub::default(); 2];
        if regexec(&p, s.as_bytes(), &mut sub) <= 0 {
            return None;
        }
        let whole = &s[sub[0].sp.unwrap()..sub[0].ep.unwrap()];
        let one = sub[1].sp.zip(sub[1].ep).map(|(a, b)| s[a..b].to_string());
        Some((whole.to_string().leak(), one))
    }

    fn whole(re: &str, s: &str) -> Option<&'static str> {
        m(re, s).map(|(w, _)| w)
    }

    #[test]
    fn leftmost_longest() {
        assert_eq!(whole("a+", "baaab"), Some("aaa"));
        assert_eq!(whole("a|ab", "xabc"), Some("ab"));
        assert_eq!(whole("(a|ab)(c|bcd)", "abcd"), Some("abcd"));
        assert_eq!(whole("x*", "abc"), Some(""));
        assert_eq!(whole("b*", "abbb"), Some(""));
        assert_eq!(whole("", "abc"), Some(""));
        assert_eq!(whole("z", "abc"), None);
    }

    #[test]
    fn subexpressions() {
        assert_eq!(m("a(b*)c", "xabbbc"), Some(("abbbc", Some("bbb".into()))));
        assert_eq!(m("(a)|b", "b"), Some(("b", None)));
        assert_eq!(m("(a*)*", "b").map(|x| x.0), Some(""));
    }

    #[test]
    fn anchors_and_dot() {
        assert_eq!(whole("^b", "ab\nbc"), Some("b"));
        assert_eq!(whole("a$", "ba\nc"), Some("a"));
        assert_eq!(whole("^$", ""), Some(""));
        assert_eq!(whole("a.c", "a\nc"), None);
        let p = regcompnl(b"a.c").unwrap();
        assert_eq!(regexec(&p, b"a\nc", &mut []), 1);
        assert_eq!(whole("é.", "xéyz"), Some("éy"));
    }

    #[test]
    fn classes() {
        assert_eq!(whole("[a-c]+", "xxbcaz"), Some("bca"));
        assert_eq!(whole("[^a-c]+", "abxyc"), Some("xy"));
        assert_eq!(whole("[^a]", "a\nb"), Some("b"), "a negated class skips newline");
        assert_eq!(whole("[]a]", "x]"), None, "']' first is not a member");
        assert_eq!(whole("[\\]a]+", "x]a"), Some("]a"));
        assert_eq!(whole("[α-γ]", "aβ"), Some("β"));
    }

    #[test]
    fn literal_and_quoting() {
        let p = regcomplit(b"a.*").unwrap();
        assert_eq!(regexec(&p, b"xa.*", &mut []), 1);
        assert_eq!(regexec(&p, b"abc", &mut []), 0);
        assert_eq!(whole("a\\.b", "axb a.b"), Some("a.b"));
    }

    #[test]
    fn errors() {
        for (re, e) in [
            ("a)", "unmatched right paren"),
            ("(a", "unmatched left paren"),
            ("*", "missing operand for *"),
            ("a|*", "missing operand for |"),
            ("()", "missing operand for ("),
            ("[a", "malformed '[]'"),
            ("[-a]", "malformed '[]'"),
            ("[a-]", "malformed '[]'"),
        ] {
            assert_eq!(regcomp(re.as_bytes()).unwrap_err(), e, "{re}");
        }
        let many = "(a)".repeat(32);
        assert_eq!(regcomp(many.as_bytes()).unwrap_err(), "too many subexpressions");
    }

    #[test]
    fn bounds_from_the_caller() {
        let p = regcomp(b"a").unwrap();
        let mut sub = [Resub { sp: Some(2), ep: None }];
        assert_eq!(regexec(&p, b"abab", &mut sub), 1);
        assert_eq!(
            sub[0],
            Resub {
                sp: Some(2),
                ep: Some(3)
            }
        );
        let mut sub = [Resub {
            sp: Some(1),
            ep: Some(2),
        }];
        assert_eq!(regexec(&p, b"abab", &mut sub), 0);
    }

    #[test]
    fn runes() {
        let p = regcomp(b"b(c+)").unwrap();
        let s: Vec<char> = "aébccd".chars().collect();
        let mut sub = [Resub::default(); 2];
        assert_eq!(rregexec(&p, &s, &mut sub), 1);
        assert_eq!(
            sub[0],
            Resub {
                sp: Some(2),
                ep: Some(5)
            }
        );
        assert_eq!(
            sub[1],
            Resub {
                sp: Some(3),
                ep: Some(5)
            }
        );
    }

    #[test]
    fn many_threads_use_the_big_lists() {
        let p = regcomp(b"(a|b|c|d|e|f|g|h|i|j|k|l)*z").unwrap();
        assert_eq!(regexec(&p, b"abcdefghijklz", &mut []), 1);
    }
}
