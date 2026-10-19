//! Words: variables, subscripts, counts, concatenation, a command's
//! output, pipes named as files, and file name patterns matched against
//! the directories they name.

use alloc::format;
use alloc::vec::Vec;

use rc::pattern::Pattern;
use rc::tree::Word;

use crate::exec::{Run, Shell};
use crate::var::List;

impl Shell {
    /// The value of `w`: words that may hold pattern characters.
    pub(crate) fn eval(&mut self, w: &Word) -> Run<Vec<Pattern>> {
        Ok(match w {
            Word::Lit(t, q) => alloc::vec![Pattern::written(t, *q)],
            Word::Var(n) => {
                let name = self.name(n)?;
                plain(self.lookup(&name))
            }
            Word::Sub(n, subs) => {
                let name = self.name(n)?;
                let subs = self.plain(subs)?;
                let val = self.vars.get(&name);
                let mut out = Vec::new();
                for s in &subs {
                    subscript(val, s, &mut out);
                }
                plain(out)
            }
            Word::Count(n) => {
                let name = self.name(n)?;
                let n = self.vars.get(&name).len();
                alloc::vec![Pattern::plain(format!("{n}").into_bytes())]
            }
            Word::Join(n) => {
                let name = self.name(n)?;
                alloc::vec![Pattern::plain(self.vars.get(&name).join(&b' '))]
            }
            Word::Concat(a, b) => {
                let a = self.eval(a)?;
                let b = self.eval(b)?;
                if a.is_empty() || b.is_empty() {
                    return self.error("null list in concatenation");
                }
                if a.len() != 1 && b.len() != 1 && a.len() != b.len() {
                    return self.error("mismatched list lengths in concatenation");
                }
                (0..a.len().max(b.len()))
                    .map(|i| a[i.min(a.len() - 1)].concat(&b[i.min(b.len() - 1)]))
                    .collect()
            }
            Word::List(ws) => self.eval_words(ws)?,
            Word::Backquote(c) => {
                let stop = self.vars.get(b"ifs").first().cloned();
                let out = self.backquote(c)?;
                plain(split_at(&out, &stop.unwrap_or_default()))
            }
            Word::PipeFd(kind, _, c) => {
                let fd = self.pipefd(*kind, c)?;
                alloc::vec![Pattern::plain(format!("/fd/{fd}").into_bytes())]
            }
        })
    }

    pub(crate) fn eval_words(&mut self, ws: &[Word]) -> Run<Vec<Pattern>> {
        let mut out = Vec::new();
        for w in ws {
            out.extend(self.eval(w)?);
        }
        Ok(out)
    }

    /// The words of `ws`, their patterns taken as plain text.
    pub(crate) fn plain(&mut self, ws: &[Word]) -> Run<List> {
        Ok(self.eval_words(ws)?.into_iter().map(|p| p.text).collect())
    }

    /// The words of `ws`, each pattern replaced by the files it matches.
    pub(crate) fn glob_words(&mut self, ws: &[Word]) -> Run<List> {
        let mut out = Vec::new();
        for p in self.eval_words(ws)? {
            glob(&p, &mut out);
        }
        Ok(out)
    }

    /// The name a variable word gives, which must be one word.
    pub(crate) fn name(&mut self, w: &Word) -> Run<Vec<u8>> {
        let mut v = self.eval(w)?;
        if v.len() != 1 {
            return self.error("variable name not singleton!");
        }
        Ok(v.remove(0).text)
    }

    /// `$name`: a number `n` is `$*(n)`.
    fn lookup(&self, name: &[u8]) -> List {
        let n = number(name);
        match n {
            Some(n) if n > 0 && name.iter().all(u8::is_ascii_digit) => {
                self.vars.get(b"*").get(n - 1).into_iter().cloned().collect()
            }
            _ => self.vars.get(name).clone(),
        }
    }
}

fn plain(l: List) -> Vec<Pattern> {
    l.into_iter().map(Pattern::plain).collect()
}

/// Whether the rune `utf` is in `s`, at a rune of it (Plan 9's
/// `utfutf`).
fn is_in(s: &[u8], utf: &[u8]) -> bool {
    let mut i = 0;
    while i < s.len() {
        if s[i..].starts_with(utf) {
            return true;
        }
        i += r::utf::decode(&s[i..]).1.max(1);
    }
    false
}

/// The words of `text` between the characters of `stop`; none are empty
/// (Plan 9's `Xbackq`). The text is read a rune at a time as `rutf`
/// reads it: a bad sequence a byte at a time, and one the text ends in
/// the middle of as U+FFFD for each byte.
fn split_at(text: &[u8], stop: &[u8]) -> List {
    let mut words = Vec::new();
    let mut word = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let c = text[i];
        let need = match c {
            0..0x80 => 1,
            0x80..0xe0 => 2,
            0xe0..0xf0 => 3,
            _ => 4,
        };
        let utf: &[u8] = if c < 0x80 {
            &text[i..=i]
        } else if i + need <= text.len() {
            &text[i..i + r::utf::decode(&text[i..i + need]).1]
        } else {
            "\u{fffd}".as_bytes()
        };
        i += if i + need <= text.len() || c < 0x80 {
            utf.len()
        } else {
            1
        };
        if is_in(stop, utf) {
            if !word.is_empty() {
                words.push(core::mem::take(&mut word));
            }
        } else {
            word.extend_from_slice(utf);
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// The number `s` starts with, if it starts with a digit.
fn number(s: &[u8]) -> Option<usize> {
    let digits = s.iter().take_while(|c| c.is_ascii_digit()).count();
    (digits > 0).then(|| {
        s[..digits].iter().fold(0usize, |n, &c| {
            n.saturating_mul(10).saturating_add(usize::from(c - b'0'))
        })
    })
}

/// Adds the words of `val` that subscript `s` picks: `n`, `n-m`, or
/// `n-` for the rest.
fn subscript(val: &List, s: &[u8], out: &mut List) {
    let n = number(s).unwrap_or(0);
    let digits = s.iter().take_while(|c| c.is_ascii_digit()).count();
    let rest = &s[digits..];
    let last = match rest.strip_prefix(b"-") {
        None => n,
        Some([]) => val.len(),
        Some(m) => match number(m) {
            Some(m) if m >= n => m,
            Some(_) => return,
            None => n,
        },
    };
    if n < 1 || n > val.len() {
        return;
    }
    out.extend(val[n - 1..last.min(val.len())].iter().cloned());
}

/// Adds what `p` gives: the files it matches, in order, or if it is not a
/// pattern or matches none, its text.
fn glob(p: &Pattern, out: &mut List) {
    if !p.is_pattern() {
        out.push(p.text.clone());
        return;
    }
    let parts = p.split_slashes();
    let mut found = Vec::new();
    globdir(Vec::new(), &parts, &mut found);
    if found.is_empty() {
        out.push(p.text.clone());
    } else {
        found.sort();
        out.extend(found);
    }
}

/// Adds the files under `name` (empty, or ending in a slash) that the
/// path elements `parts` match.
fn globdir(mut name: Vec<u8>, parts: &[Pattern], found: &mut List) {
    let mut k = 0;
    while k < parts.len() && !parts[k].is_pattern() {
        name.extend_from_slice(&parts[k].text);
        if k + 1 < parts.len() {
            name.push(b'/');
        }
        k += 1;
    }
    if k == parts.len() {
        if r::cstr(&name).is_ok_and(|c| r::dirstat(&c).is_ok()) {
            found.push(name);
        }
        return;
    }
    let dir = match &name[..] {
        [] => &b"."[..],
        [b'/'] => &name[..],
        n => n.strip_suffix(b"/").unwrap_or(n),
    };
    let Ok(dir) = r::cstr(dir) else {
        return;
    };
    let Ok(fd) = r::open(&dir, crate::fd::mode(r::Access::Read, r::OpenFlags::empty())) else {
        return;
    };
    let entries = r::dirreadall(fd).unwrap_or_default();
    let _ = r::close(fd);
    let more = k + 1 < parts.len();
    let pat = &parts[k];
    for e in entries {
        if more && !e.is_dir() {
            continue;
        }
        let n = e.name.as_bytes();
        if (n == b"." || n == b"..") && pat.text.first() != Some(&b'.') {
            continue;
        }
        if pat.matches(n) {
            let mut full = name.clone();
            full.extend_from_slice(n);
            if more {
                full.push(b'/');
                globdir(full, &parts[k + 1..], found);
            } else {
                found.push(full);
            }
        }
    }
}
