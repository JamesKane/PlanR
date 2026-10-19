//! The DFA, made as it is needed, and classes of runes as bytes, as
//! `comp.c` and `main.c`'s `initstate`.

use alloc::vec;
use alloc::vec::Vec;

use r::utf;

use crate::{CBEGIN, Fatal, Grep, NIL, Re2, TALT, TBEGIN, TCASE, TCLASS, TEND, TOR, error};

/// The most a rune can be, as the bits of one (Plan 9's `Runemask`).
const RUNEMASK: u32 = 0x1f_ffff;

/// Where a rune's encoding gets longer.
const TAB1: [u32; 2] = [0x7f, 0x7ff];
/// The ranges a byte after the first spans.
const TAB2: [u32; 2] = [0x3f, 0xfff];

/// Plan 9's `runetochar`: a rune too big, or a surrogate, is encoded as
/// the error rune.
fn runetochar(r: u32) -> Vec<u8> {
    let c = char::from_u32(r).unwrap_or(utf::RUNE_ERROR);
    c.encode_utf8(&mut [0; utf::UTF_MAX]).as_bytes().to_vec()
}

impl Grep {
    /// Adds to state `s` the way on from it for byte `c`.
    pub(crate) fn increment(&mut self, s: usize, c: u8) -> Result<(), Fatal> {
        self.follow.clear();
        self.generation += 1;
        self.matched = false;
        for i in 0..self.states[s].re.len() {
            self.fol1(self.states[s].re[i], u32::from(c))?;
        }
        self.follow.sort_unstable();
        // Find the state in the tree, or where it goes.
        let mut t = self.state0;
        let mut link: Option<(usize, bool)> = None;
        while t != NIL {
            let st = &self.states[t];
            let left = match st.re.len().cmp(&self.follow.len()) {
                core::cmp::Ordering::Greater => Some(true),
                core::cmp::Ordering::Less => Some(false),
                core::cmp::Ordering::Equal => match st.re.as_slice().cmp(self.follow.as_slice()) {
                    core::cmp::Ordering::Greater => Some(true),
                    core::cmp::Ordering::Less => Some(false),
                    core::cmp::Ordering::Equal if self.matched && !st.matched => Some(true),
                    core::cmp::Ordering::Equal if !self.matched && st.matched => Some(false),
                    core::cmp::Ordering::Equal => None,
                },
            };
            match left {
                None => {
                    self.states[s].next[usize::from(c)] = t;
                    return Ok(());
                }
                Some(true) => {
                    link = Some((t, true));
                    t = st.linkleft;
                }
                Some(false) => {
                    link = Some((t, false));
                    t = st.linkright;
                }
            }
        }
        let t = self.sal(self.follow.len());
        match link {
            Some((p, true)) => self.states[p].linkleft = t,
            Some((p, false)) => self.states[p].linkright = t,
            None => self.state0 = t,
        }
        self.states[t].re.copy_from_slice(&self.follow);
        self.states[s].next[usize::from(c)] = t;
        self.states[t].matched = self.matched;
        Ok(())
    }

    /// Adds to `follow` the nodes node `r` leads to on `c`.
    fn fol1(&mut self, mut r: usize, c: u32) -> Result<(), Fatal> {
        loop {
            if self.re[r].generation == self.generation {
                return Ok(());
            }
            if self.follow.len() >= self.maxfollow {
                return Err(error("nfollow"));
            }
            self.re[r].generation = self.generation;
            let re = &self.re[r];
            match re.ty {
                TCASE => {
                    if c < 256 {
                        let r1 = re.cases.as_ref().map_or(NIL, |t| t[c as usize]);
                        if r1 != NIL {
                            self.follow.push(r1);
                        }
                    }
                    r = self.re[r].next;
                    if r == NIL {
                        return Ok(());
                    }
                }
                TALT | TOR => {
                    let (alt, next) = (re.alt, re.next);
                    self.fol1(alt, c)?;
                    r = next;
                }
                TBEGIN => {
                    if c == u32::from(b'\n') || c == CBEGIN {
                        self.follow.push(re.next);
                    }
                    return Ok(());
                }
                TEND => {
                    if c != u32::from(b'\n') {
                        return Ok(());
                    }
                    if re.next == NIL {
                        self.matched = true;
                        return Ok(());
                    }
                    r = re.next;
                }
                TCLASS => {
                    if c >= re.lo && c <= re.hi {
                        self.follow.push(re.next);
                    }
                    return Ok(());
                }
                _ => return Err(error("fol1")),
            }
        }
    }

    /// The runes `p0` to `p1` as alternations of byte sequences.
    fn rclass(&mut self, p0: u32, p1: u32) -> Result<Re2, Fatal> {
        if p0 > p1 {
            // No match.
            return Ok(self.re2char(0xff, 0xff));
        }
        // Split the range into runes of the same length.
        for m in TAB1 {
            if p0 <= m && p1 > m {
                let a = self.rclass(p0, m)?;
                let b = self.rclass(m + 1, p1)?;
                return Ok(self.re2or(a, b));
            }
        }
        // Into part of one page, or whole pages.
        for m in TAB2 {
            if (p0 & !m) != (p1 & !m) {
                if (p0 & m) != 0 {
                    let a = self.rclass(p0, p0 | m)?;
                    let b = self.rclass((p0 | m) + 1, p1)?;
                    return Ok(self.re2or(a, b));
                }
                if (p1 & m) != m {
                    let a = self.rclass(p0, (p1 & !m) - 1)?;
                    let b = self.rclass(p1 & !m, p1)?;
                    return Ok(self.re2or(a, b));
                }
            }
        }
        let xc0 = runetochar(p0);
        let xc1 = runetochar(p1);
        if xc0.len() != xc1.len() {
            return Err(error("length"));
        }
        let mut x = self.re2char(u32::from(xc0[0]), u32::from(xc1[0]));
        for i in 1..xc0.len() {
            let y = self.re2char(u32::from(xc0[i]), u32::from(xc1[i]));
            x = self.re2cat(x, y);
        }
        Ok(x)
    }

    /// A class, as `getclass` gathered it, as ranges of bytes: the key to
    /// searching UTF-8 a byte at a time.
    pub(crate) fn re2class(&mut self, class: &[u8]) -> Result<Re2, Fatal> {
        let (nc, mut s) = match class.split_first() {
            Some((b'^', rest)) => (true, rest),
            _ => (false, class),
        };
        // The next rune of the class; 0 at its end.
        let mut next = || {
            if s.first().is_none_or(|&b| b == 0) {
                return 0;
            }
            let (r, n) = utf::decode(s);
            s = &s[n..];
            u32::from(r)
        };
        let mut pairs: Vec<u32> = vec![0; 2];
        let mut end = 0;
        pairs[end] = next();
        loop {
            if pairs[end] == u32::from(b'\\') {
                pairs[end] = next();
            }
            if pairs[end] == 0 {
                break;
            }
            pairs[end + 1] = pairs[end];
            end += 2;
            if end >= 200 {
                return Err(error("class too big"));
            }
            pairs.resize(end + 2, 0);
            pairs[end] = next();
            if pairs[end] != u32::from(b'-') {
                continue;
            }
            pairs[end] = next();
            if pairs[end] == u32::from(b'\\') {
                pairs[end] = next();
            }
            if pairs[end] == 0 {
                break;
            }
            pairs[end - 1] = pairs[end];
            pairs[end] = next();
        }
        pairs[end] = 0;
        let (sorted, _) = pairs[..end].as_chunks_mut::<2>();
        sorted.sort_unstable();
        // Spans that overlap as one; C's list ends at a 0.
        pairs.resize(end + 4, 0);
        let mut last = 0;
        let mut i = 2;
        while pairs[i] != 0 {
            let (lo, hi) = (pairs[i], pairs[i + 1]);
            i += 2;
            if lo > hi {
                continue;
            }
            if lo > pairs[last + 1] || hi < pairs[last] {
                pairs[last + 2] = lo;
                pairs[last + 3] = hi;
                last += 2;
                continue;
            }
            if lo < pairs[last] {
                pairs[last] = lo;
            }
            if hi > pairs[last + 1] {
                pairs[last + 1] = hi;
            }
        }
        pairs[last + 2] = 0;

        let mut set;
        let mut i = 2;
        if nc {
            set = self.rclass(0, pairs[0].wrapping_sub(1))?;
            let mut ov = pairs[1] + 1;
            while pairs[i] != 0 {
                let more = self.rclass(ov, pairs[i] - 1)?;
                set = self.re2or(set, more);
                ov = pairs[i + 1] + 1;
                i += 2;
            }
            let more = self.rclass(ov, RUNEMASK)?;
            set = self.re2or(set, more);
        } else {
            set = self.rclass(pairs[0], pairs[1])?;
            while pairs[i] != 0 {
                let more = self.rclass(pairs[i], pairs[i + 1])?;
                set = self.re2or(set, more);
                i += 2;
            }
        }
        Ok(set)
    }

    /// The first state, at the start of the text (`main.c`).
    pub fn initstate(&mut self) -> Result<(), Fatal> {
        let r = self.topre.beg;
        self.addcase(r)?;
        self.follow.clear();
        self.generation += 1;
        self.fol1(r, CBEGIN)?;
        self.follow.push(r);
        self.follow.sort_unstable();
        let s = self.sal(self.follow.len());
        self.states[s].re.copy_from_slice(&self.follow);
        self.state0 = s;
        Ok(())
    }
}
