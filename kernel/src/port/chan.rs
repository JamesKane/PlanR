//! Chans and names: what a file name means in a process's name space
//! (Plan 9's `chan.c`).
//!
//! A [`Chan`] is a file on some device, found by walking to it, and once
//! opened, what a file descriptor holds. It keeps the name it was reached
//! by ([`Path`]), for `fd2path`, and the mount points crossed on the way,
//! which `..` goes back through.
//!
//! **Resolving a name** ([`namec`]). A name starts at the process's root
//! (`/…`), at a device (`#c/…`), or at its current directory, and is
//! walked an element at a time, as Plan 9 does: `.` and empty elements are
//! skipped, and `..` is walked on the device, to the parent of the file
//! reached so far, or back to the mount point if that was the root of what
//! is mounted there. Walking follows the mount table ([`Pgrp`]): a
//! directory that is a mount point is replaced by what is mounted there
//! before a name is walked in it, and at the end as the use of the name
//! asks ([`Amode`]). A device names a whole run of elements in one walk,
//! as a 9P server does.
//!
//! **Errors** name the part of the name walked when it failed, as Plan
//! 9's `namelenerror`: `'/a/b' does not exist`.
//!
//! **Unions.** What is mounted on a directory is a list of directories, a
//! union. Walking in a union tries each member in turn; creating in one
//! goes to the first member mounted with `CREATE`; reading it reads each
//! member in turn.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;
use core::sync::atomic::AtomicU64;

use abi::{Access, DMDIR, ERRMAX, OpenFlags, OpenMode, Qid};

use crate::port::dev::{self, Dev};
use crate::port::error::{EBADCHAR, EBADOFFSET, EBADSHARP, EEXIST, ENOATTACH, ENOCREATE, ENOTDIR, Error, Result};
use crate::port::lock::SpinLock;
use crate::port::pgrp::{Mount, Pgrp};

/// The most names one walk of a device takes (Plan 9's `MAXWELEM`).
pub(crate) const MAXWELEM: usize = 16;
/// The size of Plan 9's `up->genbuf`, where a `#` name's device part
/// and an error's name are put.
const GENBUF: usize = 128;
/// What a walk that stops partway says.
const EDOESNOTEXIST: &str = "does not exist";
const EFILENAME: &str = "file name syntax";

/// A chan's name, and the mount points crossed to reach it (Plan 9's
/// `Path`).
#[derive(Debug, Clone)]
pub(crate) struct Path {
    pub(crate) s: String,
    /// For each element of the name, the first standing for where it
    /// starts, the mount point crossed there, if one was: where `..` from
    /// what is mounted there goes back to.
    mtpt: Vec<Option<Arc<Chan>>>,
}

impl Path {
    /// A name with nothing crossed: `/`, or a device's `#c`.
    pub(crate) fn new(s: String) -> Path {
        Path {
            s,
            mtpt: alloc::vec![None],
        }
    }

    /// Plan 9's `addelem`: the name with `s` after it, crossed at mount
    /// point `from`; `..` takes away the last element instead.
    pub(crate) fn addelem(&mut self, s: &str, from: Option<Arc<Chan>>) {
        if s == "." {
            return;
        }
        if !self.s.is_empty() && !self.s.ends_with('/') && !s.starts_with('/') {
            self.s.push('/');
        }
        self.s.push_str(s);
        if s == ".." {
            self.fixdotdotname();
            if self.mtpt.len() > 1 {
                self.mtpt.pop();
            }
        } else {
            self.mtpt.push(from);
        }
    }

    /// Plan 9's `fixdotdotname`: the name cleaned, after a `..`; a
    /// device's root is `#c`, but the root device's is `#/`.
    fn fixdotdotname(&mut self) {
        if self.s.starts_with('#') {
            let Some(r) = self.s.find('/') else {
                return;
            };
            let mut rest = cleanname(&self.s[r..]);
            if rest == "/" && self.s.as_bytes()[1] != b'/' {
                rest.clear();
            }
            self.s.truncate(r);
            self.s.push_str(&rest);
        } else {
            self.s = cleanname(&self.s);
        }
    }
}

/// A file on a device, and, once opened, how it was opened.
pub(crate) struct Chan {
    pub(crate) dev: &'static dyn Dev,
    /// Which instance of the device (Plan 9's `c->dev`).
    pub(crate) devno: u32,
    pub(crate) qid: Qid,
    /// The name it was reached by.
    pub(crate) path: Path,
    /// `None` until opened.
    pub(crate) mode: Option<OpenMode>,
    /// The offset `read` and `write` use.
    pub(crate) offset: AtomicU64,
    /// For a union directory, its members (Plan 9's `c->umh`): kept by
    /// `bind`'s walk, and by an open of a union of more than one.
    pub(crate) union: Option<Arc<[Mount]>>,
    /// Reached by crossing a mount point at its last element (Plan 9's
    /// `ismtpt`), so not to be removed or renamed.
    pub(crate) ismtpt: bool,
    /// Where a read of the union has got to.
    union_read: SpinLock<Option<UnionRead>>,
    /// The device's own state for the file, which chans walked from it
    /// share (Plan 9's `c->aux`): a pipe's queues, say.
    aux: Option<Arc<dyn Any + Send + Sync>>,
    /// The 9P session it carries, once `fversion` or `mount` has set one
    /// up (Plan 9's `c->mux`): it lasts as long as the chan does.
    mux: SpinLock<Option<Arc<dyn Any + Send + Sync>>>,
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
            .field("name", &self.path.s)
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
            path: Path::new(name),
            mode: None,
            offset: AtomicU64::new(0),
            union: None,
            ismtpt: false,
            union_read: SpinLock::new(None),
            aux: None,
            mux: SpinLock::new(None),
        }
    }

    /// The name it was reached by.
    pub(crate) fn name(&self) -> &str {
        &self.path.s
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

    /// The 9P session the chan carries, if it is a `T`.
    pub(crate) fn mux<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        let m = self.mux.lock().clone()?;
        m.downcast().ok()
    }

    /// Makes the chan carry session `m`, if it carries none yet.
    pub(crate) fn set_mux(&self, m: Arc<dyn Any + Send + Sync>) {
        self.mux.lock().get_or_insert(m);
    }

    /// True if the chan carries a session (Plan 9's `CMSG`): a failed
    /// write is the session's to report, so a pipe posts no note for it.
    pub(crate) fn is_transport(&self) -> bool {
        self.mux.lock().is_some()
    }

    /// True if another chan holds the same device state.
    pub(crate) fn aux_shared(&self) -> bool {
        self.aux.as_ref().is_some_and(|a| Arc::strong_count(a) > 1)
    }

    /// The file `qid`, called `elem`, in this directory, unopened, with
    /// the same device state.
    pub(crate) fn child(&self, qid: Qid, elem: &str) -> Chan {
        let mut c = self.unopened();
        c.qid = qid;
        c.path.addelem(elem, None);
        c
    }

    /// The same file, unopened, with the same name (Plan 9's `cclone`).
    pub(crate) fn unopened(&self) -> Chan {
        let mut c = Chan::new(self.dev, self.devno, self.qid, String::new());
        c.path = self.path.clone();
        c.aux.clone_from(&self.aux);
        c
    }

    /// Marks the chan open in `mode`.
    pub(crate) fn opened(mut self, mode: OpenMode) -> Arc<Chan> {
        self.mode = Some(mode);
        Arc::new(self)
    }

    /// True if both name the same file: Plan 9's `eqchan`, as the mount
    /// table uses it, ignoring the version.
    pub(crate) fn same_file(&self, other: &Chan) -> bool {
        self.is(other.dev, other.devno, other.qid)
    }

    /// True if the chan is file `qid` of instance `devno` of `dev`
    /// (Plan 9's `eqchantdqid`, ignoring the version).
    pub(crate) fn is(&self, dev: &dyn Dev, devno: u32, qid: Qid) -> bool {
        self.dev.dc() == dev.dc() && self.devno == devno && self.qid.path == qid.path
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
                let mut c = m.to.unopened();
                c.path = self.path.clone();
                st.open = m.to.dev.open(c, read).ok();
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

/// The last element of a name: `cons` for `/dev/cons`, `/` for `/`, `#c`
/// for `#c`.
pub(crate) fn last_elem(name: &str) -> &str {
    match name.rfind('/') {
        Some(0) if name.len() == 1 => "/",
        Some(i) => &name[i + 1..],
        None => name,
    }
}

/// Plan 9's `cleanname`: `name` with no empty or `.` elements, and each
/// `..` taking away the element before it. A `/` or a device (`#c/`) at
/// the start is a root, which `..` stops at.
pub(crate) fn cleanname(name: &str) -> String {
    if name.is_empty() {
        return String::from(".");
    }
    let mut b = Vec::from(name.as_bytes());
    b.push(0);
    let sep = |x: u8| x == b'/' || x == 0;
    let mut rooted = false;
    let mut d0 = 0;
    if b[0] == b'#' {
        if b[1] == 0 {
            return String::from(name);
        }
        d0 = 1 + name[1..].chars().next().map_or(1, char::len_utf8);
        while !sep(b[d0]) {
            d0 += 1;
        }
        if b[d0] == 0 {
            return String::from(name);
        }
        d0 += 1;
        rooted = true;
    } else if b[0] == b'/' {
        rooted = true;
        d0 = 1;
    }
    let mut s = d0;
    if rooted {
        while b[s] == b'/' {
            s += 1;
        }
    }
    // No doubled slashes.
    let mut d = d0;
    while b[s] != 0 {
        b[d] = b[s];
        d += 1;
        if b[s] == b'/' {
            while b[s + 1] == b'/' {
                s += 1;
            }
        }
        s += 1;
    }
    b[d] = 0;
    d = d0;
    s = d0;
    while b[s] != 0 {
        if b[s] == b'.' && sep(b[s + 1]) {
            if b[s + 1] == 0 {
                break;
            }
            s += 2;
            continue;
        }
        if b[s] == b'.' && b[s + 1] == b'.' && sep(b[s + 2]) {
            if d == d0 {
                if rooted {
                    // `/../x` is `/x`.
                    if b[s + 2] == 0 {
                        break;
                    }
                    s += 3;
                    continue;
                }
                // `../x` stays, and its `../` is never taken away.
                d0 += 3;
            }
            if d > d0 {
                // `a/../x` is `x`.
                d -= 2;
                while d > d0 && b[d - 1] != b'/' {
                    d -= 1;
                }
                if b[s + 2] == 0 {
                    break;
                }
                s += 3;
                continue;
            }
        }
        while !sep(b[s]) {
            b[d] = b[s];
            d += 1;
            s += 1;
        }
        if b[s] == 0 {
            break;
        }
        b[d] = b[s];
        d += 1;
        s += 1;
    }
    if d > 1 && b[d - 1] == b'/' {
        d -= 1;
    }
    b.truncate(d);
    if b.is_empty() {
        return String::from(".");
    }
    String::from_utf8(b).unwrap_or_else(|_| String::from(name))
}

/// `s` cut to at most `n` bytes, at a character's start.
pub(crate) fn truncate(s: &mut String, n: usize) {
    if s.len() > n {
        let mut at = n;
        while !s.is_char_boundary(at) {
            at -= 1;
        }
        s.truncate(at);
    }
}

/// Plan 9's `%q`, as the kernel has it, or `%#q` if `sharp`: `s` in
/// single quotes, each doubled, if it is empty or holds a space, a
/// control character or a quote, or always if `sharp`.
pub(crate) fn quote(s: &str, sharp: bool) -> String {
    if !sharp && !s.is_empty() && !s.chars().any(|c| c <= ' ' || c == '\'') {
        return String::from(s);
    }
    let mut q = String::from("'");
    for c in s.chars() {
        q.push(c);
        if c == '\'' {
            q.push(c);
        }
    }
    q.push('\'');
    q
}

/// Plan 9's `isfrog`: the bytes no name may hold, `/` being one only
/// inside an element.
fn isfrog(c: u8) -> bool {
    c < 0x20 || c == b'/' || c == 0x7f
}

/// Plan 9's `validname`: a name holds no control characters, and no `/`
/// unless `slashok`.
pub(crate) fn validname(aname: &str, slashok: bool) -> Result<()> {
    if aname.bytes().any(|c| isfrog(c) && (!slashok || c != b'/')) {
        let mut e = alloc::format!("{EBADCHAR}: {}", quote(aname, false));
        truncate(&mut e, GENBUF - 1);
        return Err(e.into());
    }
    Ok(())
}

/// Plan 9's `namelenerror`: `err`, after the first `len` bytes of
/// `aname`, quoted; a long name is shown by as much of its end as fits,
/// from a slash.
fn namelenerror(aname: &str, len: usize, err: &str) -> Error {
    let short = |len: usize| len < ERRMAX / 3 || len + err.len() < 2 * ERRMAX / 3;
    let shown = if short(len) {
        String::from(&aname[..len])
    } else {
        let b = aname.as_bytes();
        // Plan 9's `memrchr`, which never looks at the first byte.
        let memrchr = |n: usize| (1..n).rev().find(|&i| b[i] == b'/');
        let ename = len;
        let mut next = ename;
        let mut name;
        loop {
            name = next;
            next = memrchr(name).unwrap_or(0);
            if !short(ename - next) {
                break;
            }
        }
        if name == ename {
            // One element too long to show: its last bytes.
            name = ename - ERRMAX / 4;
            let mut i = 0;
            while b[name] & 0xc0 == 0x80 && i < 4 {
                name += 1;
                i += 1;
            }
        }
        alloc::format!("...{}", &aname[name..ename])
    };
    let mut e = alloc::format!("{} {err}", quote(&shown, true));
    truncate(&mut e, ERRMAX - 1);
    e.into()
}

/// Where a process's names start: its mount table, root and current
/// directory.
pub(crate) struct Names<'a> {
    pub(crate) pgrp: &'a Pgrp,
    pub(crate) slash: &'a Chan,
    pub(crate) dot: &'a Chan,
}

/// Plan 9's `domount`: if `c` is a mount point, it becomes the first of
/// what is mounted there, `m` that union, and the last element of `path`
/// records the crossing.
fn domount(pgrp: &Pgrp, c: &mut Chan, m: &mut Option<Arc<[Mount]>>, path: Option<&mut Path>) -> bool {
    let Some((from, members)) = pgrp.findmount(c.dev, c.devno, c.qid) else {
        return false;
    };
    *c = members[0].to.unopened();
    if let Some(slot) = path.and_then(|p| p.mtpt.last_mut()) {
        *slot = Some(from);
    }
    *m = Some(members);
    true
}

/// Plan 9's `undomount`: after a `..`, if the directory reached is the
/// root of what is mounted on a mount point, the mount point instead.
fn undomount(c: Chan, path: &mut Path) -> Chan {
    match path.mtpt.last_mut().and_then(Option::take) {
        Some(from) => from.unopened(),
        None => c,
    }
}

/// What a device's walk gives (Plan 9's `Walkqid`): the qid of each name
/// walked, and the file reached, if every name was.
#[derive(Debug)]
pub(crate) struct Walkqid {
    pub(crate) clone: Option<Chan>,
    pub(crate) qids: Vec<Qid>,
}

/// Plan 9's `walk`: walks `names` from `start`, following the mount table
/// unless `nomount`, all the way or not at all. A failure says how many
/// elements of the name its message should show (Plan 9's `*nerror`).
pub(crate) fn walk(
    pgrp: &Pgrp,
    start: &Chan,
    names: &[&str],
    nomount: bool,
) -> core::result::Result<Chan, (Error, usize)> {
    let mut c = start.unopened();
    let mut path = start.path.clone();
    let mut mh: Option<Arc<[Mount]>> = None;
    // Whether `c` is already what is mounted on the file reached.
    let mut didmount = false;
    let mut nhave = 0;
    while nhave < names.len() {
        if !c.qid.is_dir() {
            return Err((ENOTDIR.into(), nhave));
        }
        // As many names as a walk takes, up to a `..`, which goes alone.
        let mut ntry = (names.len() - nhave).min(MAXWELEM);
        let mut dotdot = false;
        if let Some(i) = names[nhave..nhave + ntry].iter().position(|&n| n == "..") {
            if i == 0 {
                dotdot = true;
                ntry = 1;
            } else {
                ntry = i;
            }
        }
        if !dotdot && !nomount && !didmount {
            domount(pgrp, &mut c, &mut mh, Some(&mut path));
        }
        let tried = &names[nhave..nhave + ntry];
        let mut on = (c.dev, c.devno);
        let wq = match c.dev.walk(&c, tried) {
            Ok(wq) => wq,
            Err(mut e) => {
                // The rest of a union, if any: its first is `c`.
                let mut got = None;
                if let Some(m) = mh.as_ref().filter(|_| !nomount) {
                    for f in m.iter().skip(1) {
                        match f.to.dev.walk(&f.to, tried) {
                            Ok(wq) => {
                                on = (f.to.dev, f.to.devno);
                                got = Some(wq);
                                break;
                            }
                            Err(e2) => e = e2,
                        }
                    }
                }
                got.ok_or((e, nhave + 1))?
            }
        };
        didmount = false;
        let (nc, nmh, n);
        if dotdot {
            path.addelem("..", None);
            let up = wq.clone.ok_or((Error::from(EDOESNOTEXIST), nhave + 1))?;
            nc = undomount(up, &mut path);
            nmh = None;
            n = 1;
        } else {
            // The first mount point on the way, short of the end.
            let mut found = None;
            if !nomount {
                for (i, &q) in wq.qids.iter().enumerate().take(ntry - 1) {
                    if let Some(f) = pgrp.findmount(on.0, on.1, q) {
                        found = Some((i, f));
                        break;
                    }
                }
            }
            match found {
                None => {
                    let Some(clone) = wq.clone else {
                        let nqid = wq.qids.len();
                        return Err(if wq.qids.last().is_none_or(Qid::is_dir) {
                            (EDOESNOTEXIST.into(), nhave + nqid + 1)
                        } else {
                            (ENOTDIR.into(), nhave + nqid)
                        });
                    };
                    n = wq.qids.len();
                    nc = clone;
                    nmh = None;
                }
                Some((i, (from, members))) => {
                    // Stopped at a mount point: on from what is mounted
                    // there.
                    didmount = true;
                    nc = members[0].to.unopened();
                    n = i + 1;
                    nmh = Some((from, members));
                }
            }
            for (i, name) in names[nhave..nhave + n].iter().enumerate() {
                let from = if i == n - 1 {
                    nmh.as_ref().map(|(f, _)| f.clone())
                } else {
                    None
                };
                path.addelem(name, from);
            }
        }
        c = nc;
        mh = nmh.map(|(_, m)| m);
        nhave += n;
    }
    c.path = path;
    Ok(c)
}

/// What a name is resolved for (Plan 9's `Aaccess` and the rest), which
/// decides whether a mount point at its end is crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Amode {
    /// To stat or wstat: crossed.
    Access,
    /// The new file of a `bind`: crossed, and a union kept whole.
    Bind,
    /// To `chdir` to: not crossed, so binding on `.` changes what it is.
    Todir,
    /// To open.
    Open,
    /// The mount point of a `bind` or `mount`: not crossed.
    Mount,
    /// To create.
    Create,
    /// To remove: crossed.
    Remove,
}

/// A name's elements (Plan 9's `Elemlist`): where each lies in it, where
/// the name shown with the first `k` ends (`off[k]`), and whether it ends
/// in `/` and so must be a directory.
struct Elemlist {
    elems: Vec<(usize, usize)>,
    off: Vec<usize>,
    mustbedir: bool,
}

/// Plan 9's `skipslash`: past the slashes and `./`s at `at`.
fn skipslash(b: &[u8], mut at: usize) -> usize {
    while at < b.len() && (b[at] == b'/' || (b[at] == b'.' && (at + 1 == b.len() || b[at + 1] == b'/'))) {
        at += 1;
    }
    at
}

/// Plan 9's `parsename`.
fn parsename(name: &str) -> Elemlist {
    let b = name.as_bytes();
    let mut e = Elemlist {
        elems: Vec::new(),
        off: alloc::vec![skipslash(b, 0)],
        mustbedir: false,
    };
    let mut at = 0;
    loop {
        at = skipslash(b, at);
        if at == b.len() {
            *e.off.last_mut().expect("an offset") = b.len();
            e.mustbedir = true;
            return e;
        }
        let end = b[at..].iter().position(|&c| c == b'/').map_or(b.len(), |i| at + i);
        e.elems.push((at, end));
        e.off.push(end);
        if end == b.len() {
            return e;
        }
        at = end + 1;
    }
}

/// What [`namec`] gives: the file, or, for an open or create, the open
/// chan.
enum Named {
    Chan(Box<Chan>),
    Open(Arc<Chan>),
}

/// Plan 9's `namec`: resolves `aname` for `amode`; an open or create opens
/// in `omode`, and a create makes the file with permissions `perm`.
fn namec_any(ns: &Names<'_>, aname: &str, amode: Amode, omode: OpenMode, perm: u32) -> Result<Named> {
    if aname.is_empty() {
        return Err("empty file name".into());
    }
    validname(aname, true)?;
    let b = aname.as_bytes();
    let (c, prefix, nomount) = match b[0] {
        b'/' => (ns.slash.unopened(), 0, false),
        b'#' => {
            // `#`, the device's letter and its attach specifier; `#/` is
            // the root device.
            let mut n = 0;
            while n < b.len() && (b[n] != b'/' || n < 2) {
                if n >= GENBUF - 1 {
                    return Err(EFILENAME.into());
                }
                n += 1;
            }
            let genbuf = &aname[..n];
            let dc = genbuf[1..].chars().next();
            if dc == Some('M') {
                return Err(ENOATTACH.into());
            }
            // After `RFNOMNT` only the devices that give access to
            // nothing new remain.
            if ns.pgrp.noattach() && dc.is_some_and(|dc| !"|decp".contains(dc)) {
                return Err(ENOATTACH.into());
            }
            let dc = dc.ok_or(EBADSHARP)?;
            let device = dev::find(dc).ok_or(EBADSHARP)?;
            let spec = &genbuf[1 + dc.len_utf8()..];
            let mut c = device.attach(spec)?;
            c.path = Path::new(alloc::format!("#{dc}{spec}"));
            (c, n, true)
        }
        _ => (ns.dot.unopened(), 0, false),
    };
    let e = parsename(&aname[prefix..]);
    let mut nerror = 0;
    namec_from(
        ns.pgrp,
        &c,
        &aname[prefix..],
        &e,
        nomount,
        amode,
        omode,
        perm,
        &mut nerror,
    )
    .map_err(|err| {
        if nerror == 0 {
            err
        } else {
            namelenerror(aname, prefix + e.off[nerror], err.as_str())
        }
    })
}

/// [`namec_any`] once the name's start is known: `c`, from which `name`
/// is walked. Sets how much of the name an error shows.
#[allow(clippy::too_many_arguments)] // Plan 9's namec, split at its waserror
fn namec_from(
    pgrp: &Pgrp,
    c: &Chan,
    name: &str,
    e: &Elemlist,
    nomount: bool,
    amode: Amode,
    omode: OpenMode,
    perm: u32,
    nerror: &mut usize,
) -> Result<Named> {
    let names: Vec<&str> = e.elems.iter().map(|&(a, b)| &name[a..b]).collect();
    let mut nelems = names.len();
    if amode == Amode::Create {
        // A name ending `/` or `/.` makes a directory.
        if e.mustbedir && perm & DMDIR == 0 {
            *nerror = nelems;
            return Err("create without DMDIR".into());
        }
        if nelems == 0 {
            return Err(EEXIST.into());
        }
        // The last element is created, not walked.
        nelems -= 1;
    }
    let mut c = match walk(pgrp, c, &names[..nelems], nomount) {
        Ok(c) => {
            *nerror = nelems;
            c
        }
        Err((err, n)) => {
            *nerror = if n > nelems { 0 } else { n };
            return Err(err);
        }
    };
    if e.mustbedir && !c.qid.is_dir() {
        return Err("not a directory".into());
    }
    if amode == Amode::Open && omode.access == Access::Exec && c.qid.is_dir() {
        return Err("cannot exec directory".into());
    }
    match amode {
        Amode::Bind => {
            let mut m = None;
            if !nomount {
                domount(pgrp, &mut c, &mut m, None);
            }
            c.union = m;
            Ok(Named::Chan(Box::new(c)))
        }
        Amode::Access | Amode::Remove | Amode::Open => open_at(pgrp, c, nomount, amode, omode),
        Amode::Todir => {
            if !c.qid.is_dir() {
                return Err(ENOTDIR.into());
            }
            Ok(Named::Chan(Box::new(c)))
        }
        Amode::Mount => Ok(Named::Chan(Box::new(c))),
        Amode::Create => {
            *nerror += 1;
            let elem = names[nelems];
            let trunc = OpenMode {
                access: omode.access,
                flags: omode.flags | OpenFlags::TRUNC,
            };
            // If it exists, it is opened and truncated, unless `EXCL`.
            if let Ok(found) = walk(pgrp, &c, &[elem], nomount) {
                if omode.flags.contains(OpenFlags::EXCL) {
                    return Err(EEXIST.into());
                }
                return open_at(pgrp, found, nomount, amode, trunc);
            }
            match create_in(pgrp, &c, nomount, elem, omode, perm) {
                Ok(c) => Ok(Named::Open(c)),
                Err(err) => {
                    // Another may have made it since the walk: as Plan 9,
                    // it is then opened, but only with `EXCL` is the
                    // create's own error given.
                    if omode.flags.contains(OpenFlags::EXCL) {
                        return Err(err);
                    }
                    match walk(pgrp, &c, &[elem], nomount) {
                        Ok(found) => open_at(pgrp, found, nomount, amode, trunc),
                        Err(_) => Err(err),
                    }
                }
            }
        }
    }
}

/// Makes `elem` in directory `c`, or in the member of the union mounted
/// there that takes creates.
fn create_in(pgrp: &Pgrp, c: &Chan, nomount: bool, elem: &str, omode: OpenMode, perm: u32) -> Result<Arc<Chan>> {
    let mut cnew = c.unopened();
    if !nomount && let Some((_, m)) = pgrp.findmount(c.dev, c.devno, c.qid) {
        cnew = m.iter().find(|m| m.create()).ok_or(ENOCREATE)?.to.unopened();
    }
    cnew.path = c.path.clone();
    let flags = omode.flags.bits() & !(OpenFlags::EXCL.bits() | OpenFlags::CEXEC.bits());
    let mode = OpenMode {
        access: omode.access,
        flags: OpenFlags::from_bits(flags).unwrap_or(OpenFlags::empty()),
    };
    cnew.dev.create(cnew, elem, mode, perm)
}

/// Plan 9's `Open:` in `namec`: crosses a mount point at the end of the
/// name, then for an open opens it.
fn open_at(pgrp: &Pgrp, mut c: Chan, nomount: bool, amode: Amode, omode: OpenMode) -> Result<Named> {
    let mut path = c.path.clone();
    let mut m = None;
    if !nomount {
        domount(pgrp, &mut c, &mut m, Some(&mut path));
    }
    c.path = path;
    c.ismtpt = m.is_some();
    if matches!(amode, Amode::Access | Amode::Remove) {
        return Ok(Named::Chan(Box::new(c)));
    }
    // Only a union of more than one is read as a union.
    c.union = m.filter(|m| m.len() > 1);
    let flags = omode.flags.bits() & !OpenFlags::CEXEC.bits();
    let mode = OpenMode {
        access: omode.access,
        flags: OpenFlags::from_bits(flags).unwrap_or(OpenFlags::empty()),
    };
    Ok(Named::Open(c.dev.open(c, mode)?))
}

fn no_mode() -> OpenMode {
    OpenMode {
        access: Access::Read,
        flags: OpenFlags::empty(),
    }
}

/// Resolves `name` for `amode`, which is not an open or create.
pub(crate) fn namec(ns: &Names<'_>, name: &str, amode: Amode) -> Result<Chan> {
    match namec_any(ns, name, amode, no_mode(), 0)? {
        Named::Chan(c) => Ok(*c),
        Named::Open(_) => unreachable!("namec opens only for Open and Create"),
    }
}

/// Resolves `name` and opens it in `mode`.
pub(crate) fn open(ns: &Names<'_>, name: &str, mode: OpenMode) -> Result<Arc<Chan>> {
    match namec_any(ns, name, Amode::Open, mode, 0)? {
        Named::Open(c) => Ok(c),
        Named::Chan(_) => unreachable!("an open opens"),
    }
}

/// Creates `name` with permissions `perm` and opens it in `mode`. If it
/// exists already it is opened and truncated instead, unless `mode` says
/// `EXCL`.
pub(crate) fn create(ns: &Names<'_>, name: &str, mode: OpenMode, perm: u32) -> Result<Arc<Chan>> {
    match namec_any(ns, name, Amode::Create, mode, perm)? {
        Named::Open(c) => Ok(c),
        Named::Chan(_) => unreachable!("a create opens"),
    }
}

/// Tests run by [`crate::port::ktest`].
pub(crate) mod ktests {
    use alloc::string::String;

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
            ("../../a/..", "../.."),
            ("#c", "#c"),
            ("#c/", "#c"),
            ("#c/../x", "#c/x"),
            ("#c/cons/..", "#c"),
            ("#sfoo/a/./b", "#sfoo/a/b"),
            (".", "."),
            ("", "."),
        ];
        for &(name, want) in CASES {
            let got = super::cleanname(name);
            if got != want {
                crate::println!("ktest: cleanname({name:?}) = {got:?}, want {want:?}");
                return Err("a name cleaned wrongly".into());
            }
        }
        for (name, want) in [("/", "/"), ("/dev/cons", "cons"), ("#c", "#c"), ("#c/cons", "cons")] {
            if super::last_elem(name) != want {
                return Err("wrong last element".into());
            }
        }
        let mut p = super::Path::new(String::from("#c"));
        p.addelem("cons", None);
        p.addelem("..", None);
        let mut q = super::Path::new(String::from("#/"));
        q.addelem("..", None);
        if p.s != "#c" || q.s != "#/" {
            return Err("a device's root named wrongly after ..".into());
        }
        Ok(())
    }

    /// Errors name as much of the name as was walked, as Plan 9's do.
    pub(crate) fn errors_name_files() -> TestResult {
        let cases: &[(&str, usize, &str, &str)] = &[
            ("/a/b", 4, "does not exist", "'/a/b' does not exist"),
            ("it's", 4, "file does not exist", "'it''s' file does not exist"),
            (
                "/a/very/long/name/indeed/that/goes/on/and/on/for/ever/and/ever/until/the/end/of/time",
                84,
                "file does not exist",
                "'.../that/goes/on/and/on/for/ever/and/ever/until/the/end/of/time' file does not exist",
            ),
            (
                "/x/yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy",
                93,
                "file does not exist",
                "'...yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy' file does not exist",
            ),
        ];
        for &(name, len, err, want) in cases {
            let got = super::namelenerror(name, len, err);
            if got.as_str() != want {
                crate::println!("ktest: namelenerror({name:?}) = {:?}, want {want:?}", got.as_str());
                return Err("a name error worded wrongly".into());
            }
        }
        let e = super::parsename("/a//./b/");
        if e.elems.len() != 2 || !e.mustbedir || e.off != [1, 2, 8] {
            return Err("a name parsed wrongly".into());
        }
        Ok(())
    }
}
