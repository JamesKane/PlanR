//! `#/`, the root device: the tree every name space starts from. It holds
//! the empty directories other trees are bound onto (`/dev`, `/env`, …)
//! and the boot archive's files (`lib/bootarc`), which 9boot loaded and
//! which stay where it put them, in `Boot` memory the kernel never reuses.
//! So a program in the archive pages straight from it ([`Dev::mapped`]).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicPtr, Ordering};

use abi::{DMDIR, OpenMode, QTDIR, QTFILE, Qid};
use bootarc::Archive;
use bootinfo::BootInfo;

use crate::port::chan::{Chan, Walkqid};
use crate::port::dev::{self, Dev, Entry};
use crate::port::error::{EBADSPEC, EPERM, Result};
use crate::println;

/// The directories every root has, as Plan 9's does.
const DIRS: &[&str] = &[
    "bin", "boot", "dev", "env", "fd", "mnt", "n", "net", "proc", "root", "srv", "tmp",
];

#[derive(Debug)]
struct Node {
    name: &'static str,
    /// Permissions, with `DMDIR` for a directory.
    perm: u32,
    /// A directory's children, by index, in name order.
    children: Vec<usize>,
    /// Its directory's index: the root is its own.
    parent: usize,
    data: &'static [u8],
}

/// The tree: node 0 is the root, and a node's index is its qid path.
#[derive(Debug)]
struct Tree {
    nodes: Vec<Node>,
}

impl Tree {
    fn dir(name: &'static str, parent: usize) -> Node {
        Node {
            name,
            perm: DMDIR | 0o555,
            children: Vec::new(),
            parent,
            data: &[],
        }
    }

    /// The child of `parent` called `name`, made a directory if missing.
    fn subdir(&mut self, parent: usize, name: &'static str) -> usize {
        if let Some(&i) = self.nodes[parent]
            .children
            .iter()
            .find(|&&i| self.nodes[i].name == name)
        {
            return i;
        }
        self.nodes.push(Tree::dir(name, parent));
        let i = self.nodes.len() - 1;
        self.insert(parent, i);
        i
    }

    fn insert(&mut self, parent: usize, child: usize) {
        let name = self.nodes[child].name;
        let at = self.nodes[parent]
            .children
            .partition_point(|&i| self.nodes[i].name < name);
        self.nodes[parent].children.insert(at, child);
    }

    fn build(archive: Option<&Archive<'static>>) -> Tree {
        let mut t = Tree {
            nodes: alloc::vec![Tree::dir("/", 0)],
        };
        for d in DIRS {
            t.subdir(0, d);
        }
        for f in archive.iter().flat_map(|a| a.files()) {
            let mut parent = 0;
            let mut elems = f.name.split('/').peekable();
            while let Some(e) = elems.next() {
                if elems.peek().is_some() {
                    parent = t.subdir(parent, e);
                } else if t.nodes[parent].children.iter().all(|&i| t.nodes[i].name != e) {
                    t.nodes.push(Node {
                        name: e,
                        // The archive cannot be written.
                        perm: f.mode & 0o555,
                        children: Vec::new(),
                        parent,
                        data: f.data,
                    });
                    let i = t.nodes.len() - 1;
                    t.insert(parent, i);
                }
            }
        }
        t
    }

    fn qid(&self, i: usize) -> Qid {
        Qid {
            path: i as u64,
            vers: 0,
            ty: if self.nodes[i].perm & DMDIR != 0 { QTDIR } else { QTFILE },
        }
    }

    fn entry(&self, i: usize) -> Entry {
        let n = &self.nodes[i];
        Entry::new(n.name, self.qid(i), n.data.len() as u64, n.perm)
    }
}

static TREE: AtomicPtr<Tree> = AtomicPtr::new(core::ptr::null_mut());

/// Builds the tree from the archive in the handoff, if there is a good
/// one. Runs once, at boot.
pub(crate) fn init(info: &'static BootInfo) {
    let data = info.archive();
    let archive = if data.is_empty() {
        println!("archive: none");
        None
    } else {
        match Archive::parse(data) {
            Ok(a) => {
                println!("archive: {} files, {} KiB", a.len(), data.len().div_ceil(1024));
                Some(a)
            }
            Err(e) => {
                println!("archive: {e}; ignored");
                None
            }
        }
    };
    let t = alloc::boxed::Box::leak(alloc::boxed::Box::new(Tree::build(archive.as_ref())));
    TREE.store(t, Ordering::Release);
}

fn tree() -> &'static Tree {
    let t = TREE.load(Ordering::Acquire);
    assert!(!t.is_null(), "#/ used before devroot::init");
    // SAFETY: set once, to a leaked tree that is never changed again.
    unsafe { &*t }
}

/// The root device's root, for a fresh name space.
pub(crate) fn attach_root() -> Result<Chan> {
    ROOT.attach("")
}

/// True if the boot archive holds `path` (`boot/tmem`).
pub(crate) fn has(path: &str) -> bool {
    let t = tree();
    let mut at = 0;
    for e in path.split('/') {
        match t.nodes[at].children.iter().find(|&&i| t.nodes[i].name == e) {
            Some(&i) => at = i,
            None => return false,
        }
    }
    true
}

pub(crate) struct Root;

pub(crate) static ROOT: Root = Root;

fn node(c: &Chan) -> usize {
    usize::try_from(c.qid.path).expect("a node index")
}

fn dirgen(c: &Chan, i: usize) -> Option<Entry> {
    let t = tree();
    t.nodes[node(c)].children.get(i).map(|&n| t.entry(n))
}

impl Dev for Root {
    fn dc(&self) -> char {
        '/'
    }

    fn name(&self) -> &'static str {
        "root"
    }

    fn attach(&self, spec: &str) -> Result<Chan> {
        if !spec.is_empty() {
            return Err(EBADSPEC.into());
        }
        Ok(Chan::new(&ROOT, 0, tree().qid(0), String::from("#/")))
    }

    fn walk(&self, c: &Chan, names: &[&str]) -> Result<Walkqid> {
        dev::walk_gen(c, names, &dirgen, &|c| tree().qid(tree().nodes[node(c)].parent))
    }

    fn stat(&self, c: &Chan) -> Result<Vec<u8>> {
        Ok(tree().entry(node(c)).encode(self, c.devno))
    }

    fn open(&self, c: Chan, mode: OpenMode) -> Result<Arc<Chan>> {
        let perm = tree().nodes[node(&c)].perm;
        dev::open_checked(c, mode, perm)
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        if c.qid.is_dir() {
            return dev::dirread_gen(c, buf, offset, &dirgen);
        }
        Ok(dev::read_bytes(tree().nodes[node(c)].data, buf, offset))
    }

    fn write(&self, _: &Chan, _: &[u8], _: u64) -> Result<usize> {
        Err(EPERM.into())
    }

    fn mapped(&self, c: &Chan) -> Option<&'static [u8]> {
        (!c.qid.is_dir()).then(|| tree().nodes[node(c)].data)
    }
}
