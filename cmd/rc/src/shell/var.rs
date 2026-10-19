//! Variables and functions, and their copies in `/env`.
//!
//! A variable is a list of words; unset is the empty list. Locals (from
//! `x=v cmd`, `for` and a function's `$*`) are a stack over the globals,
//! searched first, so a function sees its caller's. Each variable and
//! function marks itself changed when set, and [`Vars::export`] writes the
//! changed ones to `/env` before a command runs, as Plan 9's `Updenv`
//! does: a list as its words each followed by a NUL, a function as
//! `fn#name` holding its definition.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::rc::Rc;
use alloc::vec::Vec;

use rc::parse::Parser;
use rc::tree::Cmd;

use crate::fd::mode;

pub(crate) type List = Vec<Vec<u8>>;

#[derive(Debug, Default)]
struct Var {
    val: List,
    changed: bool,
}

#[derive(Debug)]
struct Func {
    body: Option<Rc<Cmd>>,
    changed: bool,
}

#[derive(Debug, Default)]
pub(crate) struct Vars {
    globals: BTreeMap<Vec<u8>, Var>,
    locals: Vec<(Vec<u8>, Var)>,
    fns: BTreeMap<Vec<u8>, Func>,
}

const EMPTY: &List = &Vec::new();

impl Vars {
    /// The value of `name`.
    pub(crate) fn get(&self, name: &[u8]) -> &List {
        if let Some((_, v)) = self.locals.iter().rev().find(|(n, _)| n == name) {
            return &v.val;
        }
        self.globals.get(name).map_or(EMPTY, |v| &v.val)
    }

    /// Sets `name` where it is visible: the innermost local of that name,
    /// else the global.
    pub(crate) fn set(&mut self, name: &[u8], val: List) {
        let v = match self.locals.iter_mut().rev().find(|(n, _)| n == name) {
            Some((_, v)) => v,
            None => self.globals.entry(name.to_vec()).or_default(),
        };
        v.val = val;
        v.changed = true;
    }

    /// Sets a global as it came from the environment: not changed.
    pub(crate) fn import(&mut self, name: &[u8], val: List) {
        self.globals.insert(name.to_vec(), Var { val, changed: false });
    }

    pub(crate) fn push_local(&mut self, name: &[u8], val: List) {
        self.locals.push((name.to_vec(), Var { val, changed: true }));
    }

    /// Takes away the innermost local; what it hid must go to `/env`
    /// again.
    pub(crate) fn pop_local(&mut self) {
        let Some((name, _)) = self.locals.pop() else {
            return;
        };
        match self.locals.iter_mut().rev().find(|(n, _)| *n == name) {
            Some((_, v)) => v.changed = true,
            None => self.globals.entry(name).or_default().changed = true,
        }
    }

    pub(crate) fn func(&self, name: &[u8]) -> Option<Rc<Cmd>> {
        self.fns.get(name).and_then(|f| f.body.clone())
    }

    /// Defines function `name`, or with `None` deletes it.
    pub(crate) fn set_func(&mut self, name: &[u8], body: Option<Rc<Cmd>>) {
        self.fns.insert(name.to_vec(), Func { body, changed: true });
    }

    /// Defines a function as it came from the environment.
    pub(crate) fn import_func(&mut self, name: &[u8], body: Rc<Cmd>) {
        self.fns.insert(
            name.to_vec(),
            Func {
                body: Some(body),
                changed: false,
            },
        );
    }

    /// Takes the variables in `/env`, as the shell starts.
    pub(crate) fn import_vars(&mut self) {
        for name in env_names() {
            if !name.starts_with(b"fn#")
                && let Some(val) = r::env::getenv_list(&name)
            {
                self.import(&name, val);
            }
        }
    }

    /// Takes the functions in `/env` (`fn#name`), as `finit` does.
    pub(crate) fn import_fns(&mut self) {
        for name in env_names() {
            if let Some(f) = name.strip_prefix(b"fn#")
                && let Some(body) = r::env::getenv_raw(&name).and_then(|t| function(&t))
            {
                self.import_func(f, body);
            }
        }
    }

    /// Marks everything changed, for a fresh environment.
    pub(crate) fn mark_all(&mut self) {
        for v in self.globals.values_mut() {
            v.changed = true;
        }
        for (_, v) in &mut self.locals {
            v.changed = true;
        }
        for f in self.fns.values_mut() {
            f.changed = true;
        }
    }

    /// Writes what changed to `/env`: the globals, then the locals from
    /// the outermost in, so the visible value is written last.
    pub(crate) fn export(&mut self) {
        for (name, v) in &mut self.globals {
            if v.changed {
                v.changed = false;
                put(name, &v.val);
            }
        }
        for (name, v) in &mut self.locals {
            if v.changed {
                v.changed = false;
                put(name, &v.val);
            }
        }
        for (name, f) in &mut self.fns {
            if f.changed {
                f.changed = false;
                let mut env = b"fn#".to_vec();
                env.extend_from_slice(name);
                let text = f.body.as_ref().map_or(Vec::new(), |b| definition(name, b));
                let _ = r::env::putenv(&env, &text);
            }
        }
    }
}

fn put(name: &[u8], val: &List) {
    let words: Vec<&[u8]> = val.iter().map(Vec::as_slice).collect();
    let _ = r::env::putenv_list(name, &words);
}

/// `fn name {body}`, as `whatis` prints it and `/env` keeps it.
pub(crate) fn definition(name: &[u8], body: &Cmd) -> Vec<u8> {
    format!("fn {} {{{body}}}\n", r::RcQuoted(name)).into_bytes()
}

/// The names in `/env`.
fn env_names() -> Vec<Vec<u8>> {
    let Ok(fd) = r::open(c"/env", mode(r::Access::Read, r::OpenFlags::empty())) else {
        return Vec::new();
    };
    let entries = r::dirreadall(fd).unwrap_or_default();
    let _ = r::close(fd);
    entries.into_iter().map(|e| e.name.as_bytes().to_vec()).collect()
}

/// The body of the function that `fn name {body}` defines.
fn function(text: &[u8]) -> Option<Rc<Cmd>> {
    let mut p = Parser::new(Box::new(rc::lex::Text::new(text)));
    match p.line() {
        Ok(Some(Cmd::Fn(_, Some(body)))) => Some(Rc::new(*body)),
        _ => None,
    }
}
