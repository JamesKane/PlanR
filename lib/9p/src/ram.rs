//! A file system held in memory, the one `ramfs` serves: a [`Tree`]
//! whose files' contents are byte vectors.

use alloc::vec::Vec;

use abi::{DMAPPEND, Dir};

use crate::srv::{Fs, Result, Stat};
use crate::tree::{Node, Tree};
use crate::{OTRUNC, Qid};

pub const ETOOBIG: &str = "file too large";

/// See the module.
#[derive(Debug)]
pub struct RamFs {
    pub tree: Tree<Vec<u8>>,
    /// The longest a file may grow.
    pub max_file: u64,
}

impl RamFs {
    /// An empty file system, its root owned by `uid` and open to everyone
    /// (`0777`), as Plan 9's `ramfs` is; see [`Tree::new`] for `clock`.
    #[must_use]
    pub fn new(uid: &str, clock: fn() -> u32) -> RamFs {
        RamFs {
            tree: Tree::new(uid, 0o777, clock),
            max_file: 256 << 20,
        }
    }

    fn fits(&self, len: u64) -> Result<usize> {
        if len > self.max_file {
            return Err(ETOOBIG.into());
        }
        usize::try_from(len).map_err(|_| crate::srv::Ename::Borrowed(ETOOBIG))
    }
}

fn set_len(data: &mut Vec<u8>, stat: &mut Stat, len: usize) {
    data.resize(len, 0);
    stat.length = len as u64;
}

impl Fs for RamFs {
    type File = Node<Vec<u8>>;

    fn attach(&mut self, _uname: &str, _aname: &str) -> Result<Self::File> {
        Ok(self.tree.root())
    }

    fn qid(&self, f: &Self::File) -> Qid {
        f.borrow().stat.qid
    }

    fn walk(&mut self, dir: &Self::File, name: &str, uname: &str) -> Result<Self::File> {
        self.tree.walk(dir, name, uname)
    }

    fn open(&mut self, f: &Self::File, mode: u8, uname: &str) -> Result<()> {
        self.tree.open(f, mode, uname)?;
        if mode & OTRUNC != 0 && f.borrow().stat.length != 0 {
            {
                let mut e = f.borrow_mut();
                let e = &mut *e;
                set_len(&mut e.data, &mut e.stat, 0);
            }
            self.tree.modified(f, uname);
        }
        Ok(())
    }

    fn create(&mut self, dir: &Self::File, name: &str, perm: u32, _mode: u8, uname: &str) -> Result<Self::File> {
        self.tree.create(dir, name, perm, uname)
    }

    fn read(&mut self, f: &Self::File, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let n = {
            let e = f.borrow();
            let start = usize::try_from(offset).unwrap_or(usize::MAX).min(e.data.len());
            let n = buf.len().min(e.data.len() - start);
            buf[..n].copy_from_slice(&e.data[start..start + n]);
            n
        };
        self.tree.accessed(f);
        Ok(n)
    }

    fn dirent(&mut self, dir: &Self::File, index: usize) -> Option<Stat> {
        self.tree.dirent(dir, index)
    }

    fn write(&mut self, f: &Self::File, offset: u64, data: &[u8], uname: &str) -> Result<usize> {
        {
            let mut e = f.borrow_mut();
            let e = &mut *e;
            let offset = if e.stat.mode & DMAPPEND != 0 {
                e.stat.length
            } else {
                offset
            };
            let end = self.fits(offset.saturating_add(data.len() as u64))?;
            let start = end - data.len();
            if end > e.data.len() {
                set_len(&mut e.data, &mut e.stat, end);
            }
            e.data[start..end].copy_from_slice(data);
        }
        self.tree.modified(f, uname);
        Ok(data.len())
    }

    fn stat(&mut self, f: &Self::File) -> Result<Stat> {
        Ok(f.borrow().stat.clone())
    }

    fn wstat(&mut self, f: &Self::File, d: &Dir<'_>, uname: &str) -> Result<()> {
        if d.length != u64::MAX {
            self.fits(d.length)?;
        }
        self.tree.wstat(f, d, uname, |e, len| {
            #[allow(clippy::cast_possible_truncation)] // checked by `fits`
            set_len(&mut e.data, &mut e.stat, len as usize);
        })
    }

    fn remove(&mut self, f: &Self::File, uname: &str) -> Result<()> {
        self.tree.remove(f, uname)
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::{String, ToString};
    use alloc::vec;

    use abi::{DMDIR, DMEXEC, QTAPPEND, QTDIR};

    use super::*;
    use crate::srv::{Srv, Transport};
    use crate::{Elems, NOFID, ORCLOSE, ORDWR, OREAD, OWRITE, Rmsg, Tmsg};

    fn clock() -> u32 {
        1_000_000
    }

    const MSIZE: u32 = 8192 + 24;

    type R<T> = core::result::Result<T, String>;

    /// A client of a `RamFs` server, calling `handle` directly.
    struct C {
        srv: Srv<RamFs>,
        out: Vec<u8>,
        tag: u16,
    }

    impl C {
        fn new() -> C {
            let mut c = C {
                srv: Srv::new(RamFs::new("eve", clock), MSIZE),
                out: vec![0; MSIZE as usize],
                tag: 0,
            };
            match c.rpc(&Tmsg::Version {
                msize: 1 << 20,
                version: "9P2000",
            }) {
                Ok(Rmsg::Version { msize, version }) => assert_eq!((msize, version), (MSIZE, "9P2000")),
                r => panic!("{r:?}"),
            }
            c.attach(0, "eve");
            c
        }

        fn rpc(&mut self, t: &Tmsg<'_>) -> R<Rmsg<'_>> {
            self.tag = self.tag.wrapping_add(1);
            let mut msg = vec![0; t.size()];
            t.encode(self.tag, &mut msg).unwrap();
            let n = self.srv.handle(&msg, &mut self.out).unwrap();
            let (tag, r) = Rmsg::decode(&self.out[..n]).unwrap();
            assert_eq!(tag, self.tag);
            match r {
                Rmsg::Error { ename } => Err(ename.to_string()),
                r => Ok(r),
            }
        }

        fn attach(&mut self, fid: u32, uname: &str) -> Qid {
            match self.rpc(&Tmsg::Attach {
                fid,
                afid: NOFID,
                uname,
                aname: "",
            }) {
                Ok(Rmsg::Attach { qid }) => qid,
                r => panic!("{r:?}"),
            }
        }

        fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> R<Vec<Qid>> {
            let wnames = Elems::new(names).unwrap();
            match self.rpc(&Tmsg::Walk { fid, newfid, wnames })? {
                Rmsg::Walk { wqids } => Ok(wqids.as_slice().to_vec()),
                r => panic!("{r:?}"),
            }
        }

        fn open(&mut self, fid: u32, mode: u8) -> R<Qid> {
            match self.rpc(&Tmsg::Open { fid, mode })? {
                Rmsg::Open { qid, .. } => Ok(qid),
                r => panic!("{r:?}"),
            }
        }

        fn create(&mut self, fid: u32, name: &str, perm: u32, mode: u8) -> R<Qid> {
            match self.rpc(&Tmsg::Create { fid, name, perm, mode })? {
                Rmsg::Create { qid, .. } => Ok(qid),
                r => panic!("{r:?}"),
            }
        }

        /// Walks `newfid` to `name` from the root and creates `leaf` there.
        fn make(&mut self, newfid: u32, dir: &[&str], leaf: &str, perm: u32, mode: u8) -> R<Qid> {
            self.walk(0, newfid, dir)?;
            self.create(newfid, leaf, perm, mode)
        }

        fn read(&mut self, fid: u32, offset: u64, count: u32) -> R<Vec<u8>> {
            match self.rpc(&Tmsg::Read { fid, offset, count })? {
                Rmsg::Read { data } => Ok(data.to_vec()),
                r => panic!("{r:?}"),
            }
        }

        fn write(&mut self, fid: u32, offset: u64, data: &[u8]) -> R<u32> {
            match self.rpc(&Tmsg::Write { fid, offset, data })? {
                Rmsg::Write { count } => Ok(count),
                r => panic!("{r:?}"),
            }
        }

        fn clunk(&mut self, fid: u32) -> R<()> {
            self.rpc(&Tmsg::Clunk { fid }).map(drop)
        }

        fn remove(&mut self, fid: u32) -> R<()> {
            self.rpc(&Tmsg::Remove { fid }).map(drop)
        }

        fn stat(&mut self, fid: u32) -> R<Stat> {
            match self.rpc(&Tmsg::Stat { fid })? {
                Rmsg::Stat { stat } => Ok(owned(&Dir::decode(stat).unwrap().0)),
                r => panic!("{r:?}"),
            }
        }

        fn wstat(&mut self, fid: u32, d: &Dir<'_>) -> R<()> {
            let mut b = vec![0; d.size()];
            d.encode(&mut b).unwrap();
            self.rpc(&Tmsg::Wstat { fid, stat: &b }).map(drop)
        }

        /// Every name in directory `fid`, opened here, read `count` bytes
        /// at a time.
        fn names(&mut self, fid: u32, count: u32) -> R<Vec<String>> {
            self.open(fid, OREAD)?;
            let (mut names, mut offset) = (Vec::new(), 0);
            loop {
                let b = self.read(fid, offset, count)?;
                if b.is_empty() {
                    return Ok(names);
                }
                offset += b.len() as u64;
                names.extend(abi::dirs(&b).map(|d| d.name.to_string()));
            }
        }
    }

    fn owned(d: &Dir<'_>) -> Stat {
        Stat {
            qid: d.qid,
            mode: d.mode,
            atime: d.atime,
            mtime: d.mtime,
            length: d.length,
            name: d.name.into(),
            uid: d.uid.into(),
            gid: d.gid.into(),
            muid: d.muid.into(),
        }
    }

    fn err<T: core::fmt::Debug>(r: R<T>) -> String {
        r.unwrap_err()
    }

    #[test]
    fn version_and_attach() {
        let mut c = C::new();
        match c.rpc(&Tmsg::Version {
            msize: 4096,
            version: "9P1999",
        }) {
            Ok(Rmsg::Version { msize, version }) => assert_eq!((msize, version), (4096, "unknown")),
            r => panic!("{r:?}"),
        }
        assert_eq!(c.srv.fids(), 0, "a version clunks every fid");
        assert_eq!(
            err(c.rpc(&Tmsg::Version {
                msize: 100,
                version: "9P2000"
            })),
            "version: message size too small"
        );
        let qid = c.attach(1, "eve");
        assert!(qid.ty == QTDIR && qid.path == 0);
        assert_eq!(
            err(c.rpc(&Tmsg::Attach {
                fid: 1,
                afid: NOFID,
                uname: "eve",
                aname: ""
            })),
            "duplicate fid"
        );
        assert_eq!(
            err(c.rpc(&Tmsg::Auth {
                afid: 9,
                uname: "eve",
                aname: ""
            })),
            "authentication not required"
        );
        assert!(matches!(c.rpc(&Tmsg::Flush { oldtag: 3 }), Ok(Rmsg::Flush)));
        assert_eq!(err(c.stat(7)), "unknown fid");
        let s = c.stat(1).unwrap();
        assert_eq!((s.name.as_str(), s.uid.as_str(), s.mode), ("/", "eve", DMDIR | 0o777));
    }

    #[test]
    fn files() {
        let mut c = C::new();
        let q = c.make(1, &[], "a", 0o664, ORDWR).unwrap();
        assert!(q.ty == 0 && q.path != 0);
        assert_eq!(c.write(1, 0, b"hello, world").unwrap(), 12);
        assert_eq!(c.write(1, 7, b"there").unwrap(), 5);
        assert_eq!(c.read(1, 0, 100).unwrap(), b"hello, there");
        assert_eq!(c.read(1, 7, 3).unwrap(), b"the");
        assert_eq!(c.read(1, 100, 10).unwrap(), b"");
        // A write past the end leaves a hole of zeros.
        c.write(1, 14, b"!").unwrap();
        assert_eq!(c.read(1, 10, 10).unwrap(), b"re\0\0!");
        let s = c.stat(1).unwrap();
        assert_eq!((s.length, s.qid.vers, s.mode), (15, 3, 0o664));
        assert_eq!((s.uid.as_str(), s.gid.as_str(), s.muid.as_str()), ("eve", "eve", "eve"));
        assert_eq!(err(c.walk(1, 2, &[])), "cannot clone open fid");
        c.clunk(1).unwrap();
        assert_eq!(err(c.clunk(1)), "unknown fid");

        // OTRUNC; a read is limited by the message size.
        c.walk(0, 1, &["a"]).unwrap();
        c.open(1, OWRITE | crate::OTRUNC).unwrap();
        assert_eq!(c.stat(1).unwrap().length, 0);
        let big = vec![7u8; 20_000];
        assert_eq!(c.write(1, 0, &big[..8192]).unwrap(), 8192);
        assert_eq!(err(c.read(1, 0, 10)), "permission denied");
        c.clunk(1).unwrap();
        c.walk(0, 1, &["a"]).unwrap();
        c.open(1, OREAD).unwrap();
        assert_eq!(c.read(1, 0, 100_000).unwrap().len(), 8192);
        assert_eq!(err(c.write(1, 0, b"x")), "permission denied");
        assert_eq!(err(c.open(1, OREAD)), "fid already open");
        c.clunk(1).unwrap();
        assert_eq!(err(c.make(1, &[], "a", 0o664, OREAD)), "file already exists");
        c.clunk(1).unwrap();
        assert_eq!(err(c.make(1, &[], "..", 0o664, OREAD)), "illegal file name");
        c.clunk(1).unwrap();
        assert_eq!(c.srv.fids(), 1);
    }

    #[test]
    fn append_and_limits() {
        let mut c = C::new();
        let q = c.make(1, &[], "log", DMAPPEND | 0o666, OWRITE).unwrap();
        assert_eq!(q.ty, QTAPPEND);
        c.write(1, 0, b"one ").unwrap();
        c.write(1, 0, b"two").unwrap();
        c.clunk(1).unwrap();
        c.walk(0, 1, &["log"]).unwrap();
        c.open(1, OREAD).unwrap();
        assert_eq!(c.read(1, 0, 100).unwrap(), b"one two");
        c.clunk(1).unwrap();

        c.srv.fs.max_file = 100;
        c.make(1, &[], "small", 0o666, OWRITE).unwrap();
        assert_eq!(err(c.write(1, 95, b"123456")), "file too large");
        c.write(1, 94, b"123456").unwrap();
        let mut d = Dir::DONT_TOUCH;
        d.length = 101;
        assert_eq!(err(c.wstat(1, &d)), "file too large");
    }

    #[test]
    fn directories() {
        let mut c = C::new();
        let q = c.make(1, &[], "d", DMDIR | 0o775, OREAD).unwrap();
        assert!(q.is_dir());
        c.clunk(1).unwrap();
        assert_eq!(err(c.make(1, &[], "e", DMDIR | 0o777, OWRITE)), "file is a directory");
        c.clunk(1).unwrap();
        for i in 0..100 {
            let name = alloc::format!("file{i:03}");
            c.make(1, &["d"], &name, 0o644, OWRITE).unwrap();
            c.clunk(1).unwrap();
        }
        c.walk(0, 1, &["d"]).unwrap();
        assert_eq!(err(c.open(1, OWRITE)), "file is a directory");
        // Small reads: each holds whole entries only.
        let names = c.names(1, 200).unwrap();
        assert_eq!(names.len(), 100);
        assert!(names[0] == "file000" && names[99] == "file099");
        assert_eq!(err(c.read(1, 7, 200)), "bad offset in directory read");
        let again = c.read(1, 0, 200).unwrap();
        assert_eq!(
            abi::dirs(&again).next().unwrap().name,
            "file000",
            "a read from 0 starts again"
        );
        c.clunk(1).unwrap();

        // Walks: `..`, a partial walk, a walk through a file.
        assert_eq!(c.walk(0, 1, &["d", "file005", ".."]).unwrap().len(), 2);
        let qids = c.walk(0, 1, &["d", "..", "d", "file005"]).unwrap();
        assert_eq!(qids.len(), 4);
        assert_eq!(qids[1].path, 0);
        let partial = c.walk(0, 2, &["d", "nonesuch", "x"]).unwrap();
        assert_eq!(partial.len(), 1);
        assert_eq!(err(c.stat(2)), "unknown fid", "a partial walk makes no fid");
        assert_eq!(err(c.walk(0, 2, &["nonesuch"])), "file does not exist");
        assert_eq!(c.walk(0, 0, &[]).unwrap().len(), 0);
        assert_eq!(err(c.walk(0, 1, &[])), "duplicate fid");
        assert_eq!(err(c.walk(1, 2, &["x"])), "walk in non-directory");
        assert_eq!(err(c.create(1, "x", 0o666, OREAD)), "create in non-directory");
        // The root's parent is itself.
        assert_eq!(c.walk(0, 2, &[".."]).unwrap()[0].path, 0);

        // Removing: a full directory refuses, an empty one goes.
        c.walk(0, 3, &["d"]).unwrap();
        assert_eq!(err(c.remove(3)), "directory not empty");
        assert_eq!(err(c.stat(3)), "unknown fid", "remove clunks even when it fails");
        for i in 0..100 {
            let name = alloc::format!("file{i:03}");
            c.walk(0, 3, &["d", &name]).unwrap();
            c.remove(3).unwrap();
        }
        c.walk(0, 3, &["d"]).unwrap();
        c.remove(3).unwrap();
        assert_eq!(err(c.walk(0, 3, &["d"])), "file does not exist");
        assert_eq!(err(c.remove(0)), "permission denied", "the root stays");
    }

    #[test]
    fn removed_files() {
        let mut c = C::new();
        c.make(1, &[], "gone", 0o666, ORDWR).unwrap();
        c.write(1, 0, b"still here").unwrap();
        c.walk(0, 2, &["gone"]).unwrap();
        c.remove(2).unwrap();
        // The open fid still has it.
        assert_eq!(c.read(1, 0, 100).unwrap(), b"still here");
        c.write(1, 0, b"STILL").unwrap();
        assert_eq!(c.read(1, 0, 5).unwrap(), b"STILL");
        assert_eq!(err(c.walk(0, 2, &["gone"])), "file does not exist");
        assert_eq!(err(c.remove(1)), "file does not exist");

        // ORCLOSE: gone at clunk.
        c.make(1, &[], "tmp", 0o666, OWRITE | ORCLOSE).unwrap();
        c.walk(0, 2, &["tmp"]).unwrap();
        c.clunk(2).unwrap();
        c.clunk(1).unwrap();
        assert_eq!(err(c.walk(0, 2, &["tmp"])), "file does not exist");
    }

    #[test]
    fn permissions() {
        let mut c = C::new();
        c.attach(9, "glenda");
        // eve's private directory and file.
        c.make(1, &[], "priv", DMDIR | 0o700, OREAD).unwrap();
        c.clunk(1).unwrap();
        c.make(1, &["priv"], "f", 0o600, OWRITE).unwrap();
        c.clunk(1).unwrap();
        // Walking into it needs only the root's permission; on from it
        // needs its own.
        assert_eq!(c.walk(9, 10, &["priv", "f"]).unwrap().len(), 1);
        c.walk(9, 10, &["priv"]).unwrap();
        assert_eq!(err(c.walk(10, 11, &["f"])), "permission denied");
        c.clunk(10).unwrap();
        // A file in the open root that glenda may only read.
        c.make(1, &[], "ro", 0o644, OWRITE).unwrap();
        c.clunk(1).unwrap();
        c.walk(9, 10, &["ro"]).unwrap();
        assert_eq!(err(c.open(10, OWRITE)), "permission denied");
        assert_eq!(err(c.open(10, OREAD | crate::OTRUNC)), "permission denied");
        c.open(10, OREAD).unwrap();
        c.clunk(10).unwrap();
        // glenda's own file, in eve's group (the directory's).
        c.walk(9, 10, &[]).unwrap();
        c.create(10, "mine", 0o777, OWRITE).unwrap();
        c.clunk(10).unwrap();
        let s = c.stat_of(&["mine"]);
        assert_eq!((s.uid.as_str(), s.gid.as_str(), s.mode), ("glenda", "eve", 0o777));
        // The directory's permissions mask a new file's.
        c.make(1, &["priv"], "g", 0o666, OWRITE).unwrap();
        assert_eq!(c.stat(1).unwrap().mode, 0o600);
        c.clunk(1).unwrap();
        c.make(1, &["priv"], "sub", DMDIR | 0o777, OREAD).unwrap();
        assert_eq!(c.stat(1).unwrap().mode, DMDIR | 0o700);
        c.clunk(1).unwrap();
        // Nothing may be opened that its owner may not.
        c.walk(0, 1, &["priv", "f"]).unwrap();
        let mut d = Dir::DONT_TOUCH;
        d.mode = 0;
        c.wstat(1, &d).unwrap();
        assert_eq!(err(c.open(1, OREAD)), "permission denied");
        d.mode = DMEXEC;
        c.wstat(1, &d).unwrap();
        c.open(1, crate::OEXEC).unwrap();
        c.clunk(1).unwrap();
        // ORCLOSE needs write permission on the directory.
        c.walk(0, 1, &["priv"]).unwrap();
        d.mode = DMDIR | 0o555;
        c.wstat(1, &d).unwrap();
        c.clunk(1).unwrap();
        c.walk(0, 1, &["priv", "g"]).unwrap();
        assert_eq!(err(c.open(1, OWRITE | ORCLOSE)), "permission denied");
        assert_eq!(err(c.remove(1)), "permission denied");
    }

    #[test]
    fn wstat() {
        let mut c = C::new();
        c.attach(9, "glenda");
        c.make(1, &[], "a", 0o666, OWRITE).unwrap();
        c.write(1, 0, b"0123456789").unwrap();
        c.clunk(1).unwrap();
        c.make(1, &[], "b", 0o666, OWRITE).unwrap();
        c.clunk(1).unwrap();
        c.walk(0, 1, &["a"]).unwrap();
        // A rename to a name in use, or a bad one, does nothing.
        let mut d = Dir::DONT_TOUCH;
        d.name = "b";
        assert_eq!(err(c.wstat(1, &d)), "file already exists");
        d.name = "x/y";
        assert_eq!(err(c.wstat(1, &d)), "illegal file name");
        // All or nothing: the rename is refused with the owner change.
        d.name = "c";
        d.uid = "glenda";
        assert_eq!(err(c.wstat(1, &d)), "permission denied");
        assert_eq!(c.stat(1).unwrap().name, "a");
        d.uid = "";
        d.length = 4;
        c.wstat(1, &d).unwrap();
        let s = c.stat(1).unwrap();
        assert_eq!((s.name.as_str(), s.length), ("c", 4));
        assert_eq!(err(c.walk(0, 2, &["a"])), "file does not exist");
        c.walk(0, 2, &["c"]).unwrap();
        c.open(2, OREAD).unwrap();
        assert_eq!(c.read(2, 0, 100).unwrap(), b"0123");
        c.clunk(2).unwrap();
        // Growing fills with zeros.
        d = Dir::DONT_TOUCH;
        d.length = 6;
        c.wstat(1, &d).unwrap();
        c.walk(0, 2, &["c"]).unwrap();
        c.open(2, OREAD).unwrap();
        assert_eq!(c.read(2, 0, 100).unwrap(), b"0123\0\0");
        c.clunk(2).unwrap();
        // A file stays a file; the mode, time and group are the owner's.
        d = Dir::DONT_TOUCH;
        d.mode = DMDIR | 0o777;
        assert_eq!(err(c.wstat(1, &d)), "wstat can't convert between files and directories");
        d.mode = 0o640;
        d.mtime = 42;
        d.gid = "sys";
        c.walk(9, 10, &["c"]).unwrap();
        assert_eq!(err(c.wstat(10, &d)), "permission denied");
        c.wstat(1, &d).unwrap();
        let s = c.stat(1).unwrap();
        assert_eq!((s.mode, s.mtime, s.gid.as_str()), (0o640, 42, "sys"));
        // Changing nothing is allowed to anyone.
        c.wstat(10, &Dir::DONT_TOUCH).unwrap();
        // A directory's length cannot change; the root cannot be renamed.
        d = Dir::DONT_TOUCH;
        d.length = 0;
        c.wstat(0, &d).unwrap();
        d.length = 5;
        assert_eq!(err(c.wstat(0, &d)), "wstat can't change a directory's length");
        d = Dir::DONT_TOUCH;
        d.name = "root";
        assert_eq!(err(c.wstat(0, &d)), "permission denied");
        assert_eq!(
            err(c.rpc(&Tmsg::Wstat { fid: 1, stat: b"short" })),
            "malformed stat buffer"
        );
    }

    impl C {
        fn stat_of(&mut self, names: &[&str]) -> Stat {
            self.walk(0, 99, names).unwrap();
            let s = self.stat(99).unwrap();
            self.clunk(99).unwrap();
            s
        }
    }

    /// A transport that hands out its requests a few bytes at a time and
    /// keeps the replies.
    struct Pieces {
        input: Vec<u8>,
        at: usize,
        output: Vec<u8>,
    }

    impl Transport for Pieces {
        fn read(&mut self, buf: &mut [u8]) -> crate::srv::Result<usize> {
            let n = buf.len().min(3).min(self.input.len() - self.at);
            buf[..n].copy_from_slice(&self.input[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }

        fn write(&mut self, buf: &[u8]) -> crate::srv::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }
    }

    #[test]
    fn serve() {
        let mut input = Vec::new();
        let reqs = [
            Tmsg::Version {
                msize: 8192,
                version: "9P2000",
            },
            Tmsg::Attach {
                fid: 0,
                afid: NOFID,
                uname: "eve",
                aname: "",
            },
            Tmsg::Create {
                fid: 0,
                name: "f",
                perm: 0o666,
                mode: OWRITE,
            },
            Tmsg::Write {
                fid: 0,
                offset: 0,
                data: b"data",
            },
            Tmsg::Clunk { fid: 7 },
        ];
        for (tag, t) in (1..).zip(&reqs) {
            let mut b = vec![0; t.size()];
            t.encode(tag, &mut b).unwrap();
            input.extend_from_slice(&b);
        }
        let mut t = Pieces {
            input,
            at: 0,
            output: Vec::new(),
        };
        let mut srv = Srv::new(RamFs::new("eve", clock), MSIZE);
        srv.serve(&mut t).unwrap();
        assert_eq!(srv.fids(), 0, "end of file clunks everything");
        let mut out = &t.output[..];
        let mut replies = Vec::new();
        while !out.is_empty() {
            let n = crate::msg_size(out).unwrap();
            let (tag, r) = Rmsg::decode(&out[..n]).unwrap();
            replies.push((tag, alloc::format!("{r:?}")));
            out = &out[n..];
        }
        assert_eq!(replies.len(), 5);
        assert!(replies[3].1.contains("count: 4"));
        assert_eq!(replies[4], (5, "Error { ename: \"unknown fid\" }".to_string()));

        // Not a request: the server stops.
        let mut t = Pieces {
            input: vec![7, 0, 0, 0, 101, 0, 0],
            at: 0,
            output: Vec::new(),
        };
        assert!(srv.serve(&mut t).is_err());
    }
}
