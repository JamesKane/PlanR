//! A tree of files kept in memory, Plan 9's `lib9p` file trees: for a
//! server whose files are not on a disk. The tree keeps each file's name,
//! parent and children, and its [`Stat`], and enforces Plan 9's rules for
//! them: permission to walk, open, create and remove, the permissions a
//! new file inherits, and what `wstat` may change and who may change it.
//! Each file also has the server's own data, a `T`, which the tree does
//! not look at.
//!
//! A [`Node`] is shared: a fid holding one keeps the file, and its data,
//! after it is removed, as in Plan 9. Such a file can still be read and
//! written but no longer walked from, opened or changed.

use alloc::rc::{Rc, Weak};
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use abi::{DMAPPEND, DMDIR, DMEXCL, DMEXEC, DMREAD, DMTMP, DMWRITE, Dir};

use crate::srv::{EBADNAME, ECREATENONDIR, EISDIR, EPERM, Ename, Result, Stat};
use crate::{OEXEC, ORCLOSE, ORDWR, OTRUNC, OWRITE, Qid};

pub const ENONEXIST: &str = "file does not exist";
pub const EEXIST: &str = "file already exists";
pub const ENOTEMPTY: &str = "directory not empty";
pub const EDIRCHANGE: &str = "wstat can't convert between files and directories";
pub const EDIRLENGTH: &str = "wstat can't change a directory's length";

/// A file in a [`Tree`].
pub type Node<T> = Rc<RefCell<Entry<T>>>;

/// What the tree keeps of a file.
#[derive(Debug)]
pub struct Entry<T> {
    pub stat: Stat,
    /// The server's.
    pub data: T,
    parent: Weak<RefCell<Entry<T>>>,
    children: Vec<Node<T>>,
    removed: bool,
}

impl<T> Entry<T> {
    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.stat.mode & DMDIR != 0
    }

    #[must_use]
    pub fn removed(&self) -> bool {
        self.removed
    }

    /// Whether `uname` has every permission in `want` (`DMREAD` and so on):
    /// the other bits, the group's if `uname` is the group, the owner's if
    /// the owner.
    #[must_use]
    pub fn allows(&self, uname: &str, want: u32) -> bool {
        let mode = self.stat.mode;
        let mut m = mode & 7;
        if uname == self.stat.gid {
            m |= (mode >> 3) & 7;
        }
        if uname == self.stat.uid {
            m |= (mode >> 6) & 7;
        }
        m & want == want
    }
}

/// The mode bits a file may have.
const MODE_BITS: u32 = DMDIR | DMAPPEND | DMEXCL | DMTMP | 0o777;

/// A qid's type: the high byte of the mode.
#[allow(clippy::cast_possible_truncation)] // the top byte
fn qid_type(mode: u32) -> u8 {
    (mode >> 24) as u8
}

fn bad_name(name: &str) -> bool {
    name.is_empty() || name == "." || name == ".." || name.contains('/')
}

/// A tree of files; see the module.
#[derive(Debug)]
pub struct Tree<T> {
    root: Node<T>,
    next_path: u64,
    clock: fn() -> u32,
}

impl<T: Default> Tree<T> {
    /// A tree whose root, owned by `uid`, has permissions `perm`; `clock`
    /// gives the time, in seconds since the epoch.
    #[must_use]
    pub fn new(uid: &str, perm: u32, clock: fn() -> u32) -> Tree<T> {
        let now = clock();
        let root = Rc::new_cyclic(|me| {
            RefCell::new(Entry {
                stat: Stat {
                    qid: Qid {
                        path: 0,
                        vers: 0,
                        ty: qid_type(DMDIR),
                    },
                    mode: DMDIR | (perm & 0o777),
                    atime: now,
                    mtime: now,
                    length: 0,
                    name: String::from("/"),
                    uid: uid.into(),
                    gid: uid.into(),
                    muid: uid.into(),
                },
                data: T::default(),
                parent: me.clone(),
                children: Vec::new(),
                removed: false,
            })
        });
        Tree {
            root,
            next_path: 1,
            clock,
        }
    }

    /// Makes `name` in `dir` for `uname`, who must be allowed to write
    /// `dir`. The new file is owned by `uname`, in `dir`'s group, and
    /// takes from `perm` only the permissions `dir` has (all but write
    /// for a file, as Plan 9's `0666` mask).
    pub fn create(&mut self, dir: &Node<T>, name: &str, perm: u32, uname: &str) -> Result<Node<T>> {
        let now = self.now();
        let mut d = dir.borrow_mut();
        if d.removed {
            return Err(ENONEXIST.into());
        }
        if !d.is_dir() {
            return Err(ECREATENONDIR.into());
        }
        if bad_name(name) {
            return Err(EBADNAME.into());
        }
        if !d.allows(uname, DMWRITE) {
            return Err(EPERM.into());
        }
        if d.children.iter().any(|c| c.borrow().stat.name == name) {
            return Err(EEXIST.into());
        }
        let mode = if perm & DMDIR != 0 {
            perm & (!0o777 | (d.stat.mode & 0o777))
        } else {
            perm & (!0o666 | (d.stat.mode & 0o666))
        } & MODE_BITS;
        let path = self.next_path;
        self.next_path += 1;
        let node = Rc::new(RefCell::new(Entry {
            stat: Stat {
                qid: Qid {
                    path,
                    vers: 0,
                    ty: qid_type(mode),
                },
                mode,
                atime: now,
                mtime: now,
                length: 0,
                name: name.into(),
                uid: uname.into(),
                gid: d.stat.gid.clone(),
                muid: uname.into(),
            },
            data: T::default(),
            parent: Rc::downgrade(dir),
            children: Vec::new(),
            removed: false,
        }));
        d.children.push(node.clone());
        d.stat.mtime = now;
        d.stat.muid = uname.into();
        d.stat.qid.vers = d.stat.qid.vers.wrapping_add(1);
        Ok(node)
    }
}

impl<T> Tree<T> {
    #[must_use]
    pub fn root(&self) -> Node<T> {
        self.root.clone()
    }

    #[must_use]
    pub fn now(&self) -> u32 {
        (self.clock)()
    }

    /// The parent of `f`; the root is its own.
    #[must_use]
    pub fn parent(&self, f: &Node<T>) -> Node<T> {
        f.borrow().parent.upgrade().unwrap_or_else(|| self.root.clone())
    }

    /// The file `name` in `dir`, which `uname` must be allowed to search;
    /// `..` is the parent.
    pub fn walk(&self, dir: &Node<T>, name: &str, uname: &str) -> Result<Node<T>> {
        let d = dir.borrow();
        if d.removed {
            return Err(ENONEXIST.into());
        }
        if !d.is_dir() {
            return Err(crate::srv::EWALKNODIR.into());
        }
        if !d.allows(uname, DMEXEC) {
            return Err(EPERM.into());
        }
        if name == ".." {
            drop(d);
            return Ok(self.parent(dir));
        }
        d.children
            .iter()
            .find(|c| c.borrow().stat.name == name)
            .cloned()
            .ok_or(Ename::Borrowed(ENONEXIST))
    }

    /// Checks that `uname` may open `f` with `mode`: read, write or
    /// execute permission as the mode asks, write for `OTRUNC`, and write
    /// on the parent for `ORCLOSE`.
    pub fn open(&self, f: &Node<T>, mode: u8, uname: &str) -> Result<()> {
        let e = f.borrow();
        if e.removed {
            return Err(ENONEXIST.into());
        }
        let mut want = match mode & 3 {
            OWRITE => DMWRITE,
            ORDWR => DMREAD | DMWRITE,
            OEXEC => DMEXEC,
            _ => DMREAD,
        };
        if mode & OTRUNC != 0 {
            if e.is_dir() {
                return Err(EISDIR.into());
            }
            want |= DMWRITE;
        }
        if !e.allows(uname, want) {
            return Err(EPERM.into());
        }
        drop(e);
        if mode & ORCLOSE != 0 && !self.parent(f).borrow().allows(uname, DMWRITE) {
            return Err(EPERM.into());
        }
        Ok(())
    }

    /// Entry `i` of directory `dir`.
    #[must_use]
    pub fn dirent(&self, dir: &Node<T>, i: usize) -> Option<Stat> {
        Some(dir.borrow().children.get(i)?.borrow().stat.clone())
    }

    /// Takes `f` out of the tree, if `uname` may write its parent and it
    /// is not a directory with something in it.
    pub fn remove(&mut self, f: &Node<T>, uname: &str) -> Result<()> {
        if Rc::ptr_eq(f, &self.root) {
            return Err(EPERM.into());
        }
        let e = f.borrow();
        if e.removed {
            return Err(ENONEXIST.into());
        }
        let parent = self.parent(f);
        if !parent.borrow().allows(uname, DMWRITE) {
            return Err(EPERM.into());
        }
        if !e.children.is_empty() {
            return Err(ENOTEMPTY.into());
        }
        drop(e);
        let now = self.now();
        let mut p = parent.borrow_mut();
        p.children.retain(|c| !Rc::ptr_eq(c, f));
        p.stat.mtime = now;
        p.stat.qid.vers = p.stat.qid.vers.wrapping_add(1);
        drop(p);
        f.borrow_mut().removed = true;
        Ok(())
    }

    /// Notes that `uname` changed `f`'s contents: its version, time and
    /// `muid`.
    pub fn modified(&self, f: &Node<T>, uname: &str) {
        let now = self.now();
        let mut e = f.borrow_mut();
        e.stat.qid.vers = e.stat.qid.vers.wrapping_add(1);
        e.stat.mtime = now;
        if e.stat.muid != uname {
            e.stat.muid = uname.into();
        }
    }

    /// Notes that `f` was read.
    pub fn accessed(&self, f: &Node<T>) {
        f.borrow_mut().stat.atime = self.now();
    }

    /// Changes what `d` asks of `f`, for `uname`, or nothing if any of it
    /// is refused:
    ///
    /// - the name, if `uname` may write the parent and no sibling has it;
    /// - the length (through `resize`, which must also set
    ///   `stat.length`), if `uname` may write `f`, which is not a
    ///   directory;
    /// - the mode, the time and the group, if `uname` owns `f`; a file
    ///   cannot become a directory, or a directory a file;
    /// - not the owner.
    pub fn wstat(
        &mut self,
        f: &Node<T>,
        d: &Dir<'_>,
        uname: &str,
        resize: impl FnOnce(&mut Entry<T>, u64),
    ) -> Result<()> {
        let e = f.borrow();
        if e.removed {
            return Err(ENONEXIST.into());
        }
        let owner = uname == e.stat.uid;
        let rename = !d.name.is_empty() && d.name != e.stat.name;
        if rename {
            if Rc::ptr_eq(f, &self.root) {
                return Err(EPERM.into());
            }
            if bad_name(d.name) {
                return Err(EBADNAME.into());
            }
            let parent = self.parent(f);
            let p = parent.borrow();
            if !p.allows(uname, DMWRITE) {
                return Err(EPERM.into());
            }
            if p.children
                .iter()
                .any(|c| !Rc::ptr_eq(c, f) && c.borrow().stat.name == d.name)
            {
                return Err(EEXIST.into());
            }
        }
        let length = d.length != u64::MAX && d.length != e.stat.length;
        if length {
            if e.is_dir() {
                return Err(EDIRLENGTH.into());
            }
            if !e.allows(uname, DMWRITE) {
                return Err(EPERM.into());
            }
        }
        let mode = d.mode != u32::MAX && d.mode != e.stat.mode;
        if mode {
            if (d.mode ^ e.stat.mode) & DMDIR != 0 {
                return Err(EDIRCHANGE.into());
            }
            if !owner {
                return Err(EPERM.into());
            }
        }
        let mtime = d.mtime != u32::MAX && d.mtime != e.stat.mtime;
        let gid = !d.gid.is_empty() && d.gid != e.stat.gid;
        if (mtime || gid) && !owner {
            return Err(EPERM.into());
        }
        if !d.uid.is_empty() && d.uid != e.stat.uid {
            return Err(EPERM.into());
        }
        drop(e);

        let now = self.now();
        let mut e = f.borrow_mut();
        if rename {
            e.stat.name = d.name.into();
        }
        if mode {
            e.stat.mode = d.mode & MODE_BITS;
            e.stat.qid.ty = qid_type(e.stat.mode);
        }
        if gid {
            e.stat.gid = d.gid.into();
        }
        if length {
            resize(&mut e, d.length);
            e.stat.qid.vers = e.stat.qid.vers.wrapping_add(1);
            e.stat.mtime = now;
        }
        if mtime {
            e.stat.mtime = d.mtime;
        }
        Ok(())
    }
}
