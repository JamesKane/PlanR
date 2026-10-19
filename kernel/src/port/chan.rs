//! Chans and names: what a file name means in a process's name space.
//!
//! A [`Chan`] is a file on some device, found by walking to it, and once
//! opened, what a file descriptor holds. It keeps the name it was reached
//! by, cleaned, for `fd2path`.
//!
//! **Resolving a name** ([`namec`]). A name starts at the process's root
//! (`/…`), at a device (`#c/…`), or at its current directory. It is first
//! cleaned lexically, as Plan 9 does: `.` goes, and `..` takes away the
//! element before it, so `..` means the parent by name, whatever was bound
//! where. A relative name with no `..` walks from the current directory;
//! any other walks from the root of its cleaned form. Walking a name
//! follows the mount table ([`Pgrp`]) after every element: a file that is
//! a mount point is replaced by what is mounted there.
//!
//! **Unions.** What is mounted on a directory is a list of directories, a
//! union. Walking in a union tries each member in turn; creating in one
//! goes to the first member mounted with `CREATE`; reading it reads each
//! member in turn.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;
use core::sync::atomic::AtomicU64;

use abi::{Access, OpenFlags, OpenMode, Qid};

use crate::port::dev::{self, Dev};
use crate::port::error::{EBADCHAR, EBADOFFSET, EBADSHARP, EEXIST, ENOATTACH, ENOCREATE, ENONEXIST, ENOTDIR, Result};
use crate::port::lock::SpinLock;
use crate::port::pgrp::{Mount, Pgrp};

/// A file on a device, and, once opened, how it was opened.
pub(crate) struct Chan {
    pub(crate) dev: &'static dyn Dev,
    /// Which instance of the device (Plan 9's `c->dev`).
    pub(crate) devno: u32,
    pub(crate) qid: Qid,
    /// The cleaned name it was reached by.
    pub(crate) name: String,
    /// `None` until opened.
    pub(crate) mode: Option<OpenMode>,
    /// The offset `read` and `write` use.
    pub(crate) offset: AtomicU64,
    /// For a union directory, its members.
    pub(crate) union: Option<Arc<[Mount]>>,
    /// Where a read of the union has got to.
    union_read: SpinLock<Option<UnionRead>>,
    /// The device's own state for the file, which chans walked from it
    /// share (Plan 9's `c->aux`): a pipe's queues, say.
    aux: Option<Arc<dyn Any + Send + Sync>>,
}

/// How far through a union a directory read has got.
struct UnionRead {
    member: usize,
    open: Option<Arc<Chan>>,
    /// The offset in the member, and in the union.
    inner: u64,
    outer: u64,
}

impl fmt::Debug for Chan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Chan")
            .field("dev", &self.dev.dc())
            .field("devno", &self.devno)
            .field("qid", &self.qid)
            .field("name", &self.name)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl Chan {
    /// An unopened chan for the file `qid` on `dev`.
    pub(crate) fn new(dev: &'static dyn Dev, devno: u32, qid: Qid, name: String) -> Chan {
        Chan {
            dev,
            devno,
            qid,
            name,
            mode: None,
            offset: AtomicU64::new(0),
            union: None,
            union_read: SpinLock::new(None),
            aux: None,
        }
    }

    /// The chan with `aux` as its device state.
    pub(crate) fn with_aux(mut self, aux: Arc<dyn Any + Send + Sync>) -> Chan {
        self.aux = Some(aux);
        self
    }

    /// The device state, if it is a `T`.
    pub(crate) fn aux<T: Any>(&self) -> Option<&T> {
        let aux: &dyn Any = self.aux.as_deref()?;
        aux.downcast_ref()
    }

    /// The file `qid`, called `elem`, in this directory, unopened, with
    /// the same device state.
    pub(crate) fn child(&self, qid: Qid, elem: &str) -> Chan {
        let mut c = Chan::new(self.dev, self.devno, qid, join(&self.name, elem));
        c.aux.clone_from(&self.aux);
        c
    }

    /// The same file, unopened, called `name`.
    pub(crate) fn renamed(&self, name: &str) -> Chan {
        let mut c = Chan::new(self.dev, self.devno, self.qid, String::from(name));
        c.aux.clone_from(&self.aux);
        c
    }

    /// The same file, unopened, with the same name.
    pub(crate) fn unopened(&self) -> Chan {
        self.renamed(&self.name)
    }

    /// Marks the chan open in `mode`.
    pub(crate) fn opened(mut self, mode: OpenMode) -> Arc<Chan> {
        self.mode = Some(mode);
        Arc::new(self)
    }

    /// True if both name the same file: the mount table's test.
    pub(crate) fn same_file(&self, other: &Chan) -> bool {
        self.dev.dc() == other.dev.dc()
            && self.devno == other.devno
            && self.qid.path == other.qid.path
            && self.qid.ty == other.qid.ty
    }

    /// True if opened for reading (an exec open reads too).
    pub(crate) fn readable(&self) -> bool {
        self.mode
            .is_some_and(|m| matches!(m.access, Access::Read | Access::ReadWrite | Access::Exec))
    }

    pub(crate) fn writable(&self) -> bool {
        self.mode
            .is_some_and(|m| matches!(m.access, Access::Write | Access::ReadWrite))
    }

    /// Reads at `offset`; a union directory reads its members in turn,
    /// each from its start, so it reads on only from where the last read
    /// stopped, or from 0.
    pub(crate) fn read(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        let Some(members) = self.union.as_ref().filter(|_| self.qid.is_dir()) else {
            return self.dev.read(self, buf, offset);
        };
        let mut st = self.union_read.lock().take();
        if offset == 0 {
            st = None;
        }
        let mut st = st.unwrap_or(UnionRead {
            member: 0,
            open: None,
            inner: 0,
            outer: 0,
        });
        if st.outer != offset {
            return Err(EBADOFFSET.into());
        }
        let r = loop {
            let Some(m) = members.get(st.member) else {
                break Ok(0);
            };
            if st.open.is_none() {
                let read = OpenMode {
                    access: Access::Read,
                    flags: OpenFlags::empty(),
                };
                // A member that cannot be read is left out.
                st.open = m.to.dev.open(m.to.renamed(&self.name), read).ok();
            }
            let n = match &st.open {
                Some(c) => match c.dev.read(c, buf, st.inner) {
                    Ok(n) => n,
                    Err(e) => break Err(e),
                },
                None => 0,
            };
            if n > 0 {
                st.inner += n as u64;
                st.outer += n as u64;
                break Ok(n);
            }
            st.member += 1;
            st.open = None;
            st.inner = 0;
        };
        let old = self.union_read.lock().replace(st);
        drop(old);
        r
    }
}

impl Drop for Chan {
    fn drop(&mut self) {
        if let Some(m) = self.mode {
            self.dev.close(self);
            if m.flags.contains(OpenFlags::RCLOSE) {
                let _ = self.dev.remove(self);
            }
        }
    }
}

/// `dir/elem`, without doubling a slash.
pub(crate) fn join(dir: &str, elem: &str) -> String {
    let mut s = String::from(dir);
    if !s.ends_with('/') {
        s.push('/');
    }
    s.push_str(elem);
    s
}

/// The last element of a cleaned name: `cons` for `/dev/cons`, `/` for
/// `/`, `#c` for `#c`.
pub(crate) fn last_elem(name: &str) -> &str {
    match name.rfind('/') {
        Some(0) if name.len() == 1 => "/",
        Some(i) => &name[i + 1..],
        None => name,
    }
}

/// `name` cleaned lexically: no empty or `.` elements, and each `..`
/// takes away the element before it. A `/` or a device (`#c`) at the
/// start is a root, which `..` stops at.
pub(crate) fn clean(name: &str) -> String {
    let (root, rest) = if let Some(rest) = name.strip_prefix('/') {
        ("/", rest)
    } else if name.starts_with('#') {
        let end = name.find('/').unwrap_or(name.len());
        (&name[..end], &name[end..])
    } else {
        ("", name)
    };
    let mut elems: Vec<&str> = Vec::new();
    for e in rest.split('/') {
        match e {
            "" | "." => {}
            ".." if elems.last().is_some_and(|&l| l != "..") => {
                elems.pop();
            }
            ".." if !root.is_empty() => {}
            e => elems.push(e),
        }
    }
    let mut s = String::from(root);
    for (i, e) in elems.iter().enumerate() {
        if i > 0 || (!root.is_empty() && root != "/") {
            s.push('/');
        }
        s.push_str(e);
    }
    if s.is_empty() {
        s.push('.');
    }
    s
}

/// Where a process's names start: its mount table, root and current
/// directory.
pub(crate) struct Names<'a> {
    pub(crate) pgrp: &'a Pgrp,
    pub(crate) slash: &'a Chan,
    pub(crate) dot: &'a Chan,
}

/// A walked name: the file, and the union mounted on it, if any.
pub(crate) struct Walked {
    pub(crate) chan: Chan,
    pub(crate) union: Option<Arc<[Mount]>>,
}

/// Follows the mount table from `c`: what is mounted there, or `c`.
fn domount(pgrp: &Pgrp, c: Chan) -> Walked {
    match pgrp.lookup(&c) {
        Some(members) if !members.is_empty() => Walked {
            chan: members[0].to.renamed(&c.name),
            union: Some(members),
        },
        _ => Walked { chan: c, union: None },
    }
}

/// The starting point and elements of `name`.
fn start(ns: &Names<'_>, name: &str) -> Result<(Chan, String)> {
    if name.is_empty() {
        return Err(ENONEXIST.into());
    }
    if name.contains('\0') {
        return Err(EBADCHAR.into());
    }
    let relative = !name.starts_with('/') && !name.starts_with('#');
    if relative && !name.split('/').any(|e| e == "..") {
        let rest = clean(name);
        let rest = if rest == "." { String::new() } else { rest };
        return Ok((ns.dot.unopened(), rest));
    }
    let full = clean(&if relative {
        join(&ns.dot.name, name)
    } else {
        String::from(name)
    });
    if let Some(rest) = full.strip_prefix('/') {
        return Ok((ns.slash.renamed("/"), String::from(rest)));
    }
    // A device: `#` and its letter, then the attach specifier. After
    // `RFNOMNT` only the devices that give access to nothing new remain,
    // as in Plan 9.
    let end = full.find('/').unwrap_or(full.len());
    let mut chars = full[1..end].chars();
    let dc = chars.next().ok_or(EBADSHARP)?;
    if ns.pgrp.noattach() && !"|decp".contains(dc) {
        return Err(ENOATTACH.into());
    }
    let d = dev::find(dc).ok_or(EBADSHARP)?;
    let root = d.attach(chars.as_str())?.renamed(&full[..end]);
    let rest = full.get(end + 1..).unwrap_or("");
    Ok((root, String::from(rest)))
}

/// Walks `name`. Mount points are followed after every element and at
/// the start, but at the last only if `follow_last`: binding over a mount
/// point changes what is mounted there, not what is mounted on that.
pub(crate) fn walk(ns: &Names<'_>, name: &str, follow_last: bool) -> Result<Walked> {
    let (root, rest) = start(ns, name)?;
    let elems: Vec<&str> = rest.split('/').filter(|e| !e.is_empty()).collect();
    let mut w = if elems.is_empty() && !follow_last {
        Walked {
            chan: root,
            union: None,
        }
    } else {
        domount(ns.pgrp, root)
    };
    for (i, e) in elems.iter().enumerate() {
        if !w.chan.qid.is_dir() {
            return Err(ENOTDIR.into());
        }
        let mut next = match &w.union {
            Some(members) => {
                let mut found = Err(ENONEXIST.into());
                for m in members.iter() {
                    found = m.to.dev.walk(&m.to, e);
                    if found.is_ok() {
                        break;
                    }
                }
                found?
            }
            None => w.chan.dev.walk(&w.chan, e)?,
        };
        next.name = join(&w.chan.name, e);
        w = if i + 1 < elems.len() || follow_last {
            domount(ns.pgrp, next)
        } else {
            Walked {
                chan: next,
                union: None,
            }
        };
    }
    Ok(w)
}

/// Resolves `name` and opens it in `mode`.
pub(crate) fn open(ns: &Names<'_>, name: &str, mode: OpenMode) -> Result<Arc<Chan>> {
    let Walked { mut chan, union } = walk(ns, name, true)?;
    chan.union = union;
    chan.dev.open(chan, mode)
}

/// Creates `name` with permissions `perm` and opens it in `mode`. If it
/// exists already it is opened and truncated instead, unless `mode` says
/// `EXCL` or it is a directory.
pub(crate) fn create(ns: &Names<'_>, name: &str, mode: OpenMode, perm: u32) -> Result<Arc<Chan>> {
    let full = clean(name);
    let elem = last_elem(&full);
    if matches!(elem, "/" | "." | "..") || elem.starts_with('#') {
        return Err(EEXIST.into());
    }
    if let Ok(w) = walk(ns, &full, true) {
        if mode.flags.contains(OpenFlags::EXCL) || perm & abi::DMDIR != 0 {
            return Err(EEXIST.into());
        }
        let mut c = w.chan;
        c.union = w.union;
        let trunc = OpenMode {
            access: mode.access,
            flags: mode.flags | OpenFlags::TRUNC,
        };
        return c.dev.open(c, trunc);
    }
    let parent = &full[..full.len() - elem.len()];
    let parent = if parent.is_empty() { "." } else { parent };
    let w = walk(ns, parent, true)?;
    let dir = match &w.union {
        Some(members) => {
            let m = members.iter().find(|m| m.create).ok_or(ENOCREATE)?;
            m.to.renamed(&w.chan.name)
        }
        None => w.chan,
    };
    if !dir.qid.is_dir() {
        return Err(ENOTDIR.into());
    }
    dir.dev.create(dir, elem, mode, perm)
}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use crate::port::ktest::TestResult;

    /// Names clean as Plan 9's `cleanname` cleans them.
    pub(crate) fn names_clean() -> TestResult {
        const CASES: &[(&str, &str)] = &[
            ("/", "/"),
            ("//a//b/", "/a/b"),
            ("/a/./b/../c", "/a/c"),
            ("/..", "/"),
            ("/a/../..", "/"),
            ("a/b/../..", "."),
            ("../a", "../a"),
            ("a/../../b", "../b"),
            ("#c", "#c"),
            ("#c/", "#c"),
            ("#c/../x", "#c/x"),
            ("#c/cons/..", "#c"),
            ("#sfoo/a/./b", "#sfoo/a/b"),
            (".", "."),
            ("", "."),
        ];
        for &(name, want) in CASES {
            let got = super::clean(name);
            if got != want {
                crate::println!("ktest: clean({name:?}) = {got:?}, want {want:?}");
                return Err("a name cleaned wrongly".into());
            }
        }
        for (name, want) in [("/", "/"), ("/dev/cons", "cons"), ("#c", "#c"), ("#c/cons", "cons")] {
            if super::last_elem(name) != want {
                return Err("wrong last element".into());
            }
        }
        Ok(())
    }
}
