//! `#P`, the architecture device. For now it holds `cputype`: the
//! architecture's name, the counter's frequency in MHz, and the number of
//! CPUs online. On amd64 the counter is the TSC, which runs at the
//! processor's nominal rate; on arm64 it is the generic timer, which does
//! not.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::{DMDIR, OpenMode, QTDIR, QTFILE, Qid};

use crate::arch;
use crate::port::chan::{Chan, Walkqid};
use crate::port::dev::{self, Dev, Entry};
use crate::port::error::{EBADSPEC, Result};
use crate::port::{clock, cpu};

pub(crate) struct Arch;

pub(crate) static ARCH: Arch = Arch;

const DIR: Qid = Qid {
    path: 0,
    vers: 0,
    ty: QTDIR,
};
const CPUTYPE: Qid = Qid {
    path: 1,
    vers: 0,
    ty: QTFILE,
};

fn dirgen(_: &Chan, i: usize) -> Option<Entry> {
    (i == 0).then(|| Entry::new("cputype", CPUTYPE, 0, 0o444))
}

impl Dev for Arch {
    fn dc(&self) -> char {
        'P'
    }

    fn name(&self) -> &'static str {
        "arch"
    }

    fn attach(&self, spec: &str) -> Result<Chan> {
        if !spec.is_empty() {
            return Err(EBADSPEC.into());
        }
        Ok(Chan::new(&ARCH, 0, DIR, String::from("#P")))
    }

    fn walk(&self, c: &Chan, names: &[&str]) -> Result<Walkqid> {
        dev::walk_gen(c, names, &dirgen, &dev::flat(DIR))
    }

    fn stat(&self, c: &Chan) -> Result<Vec<u8>> {
        let e = if c.qid.is_dir() {
            Entry::new("#P", DIR, 0, DMDIR | 0o555)
        } else {
            Entry::new("cputype", CPUTYPE, 0, 0o444)
        };
        Ok(e.encode(self, c.devno))
    }

    fn open(&self, c: Chan, mode: OpenMode) -> Result<Arc<Chan>> {
        let perm = if c.qid.is_dir() { DMDIR | 0o555 } else { 0o444 };
        dev::open_checked(c, mode, perm)
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        if c.qid.is_dir() {
            return dev::dirread_gen(c, buf, offset, &dirgen);
        }
        let text = alloc::format!(
            "{} {} {}\n",
            arch::NAME,
            clock::counter_hz() / 1_000_000,
            cpu::online().count()
        );
        Ok(dev::read_bytes(text.as_bytes(), buf, offset))
    }
}
