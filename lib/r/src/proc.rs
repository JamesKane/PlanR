//! Processes: `wait` and its message, `postnote`, and the process's own
//! pid.

use alloc::vec::Vec;
use core::fmt::Write as _;

use crate::{Access, Buf, Error, OpenFlags, OpenMode};

/// How a child ended (Plan 9's `Waitmsg`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waitmsg {
    pub pid: u64,
    /// User, system and real time, in milliseconds.
    pub time: [u64; 3],
    /// Its exit status: empty for a normal exit, else `name pid: text`.
    pub msg: Vec<u8>,
}

impl Waitmsg {
    /// Decodes `await`'s text.
    #[must_use]
    pub fn parse(text: &[u8]) -> Option<Waitmsg> {
        let mut msg = alloc::vec![0u8; text.len()];
        let (pid, time, n) = abi::parse_waitmsg(text, &mut msg)?;
        msg.truncate(n);
        Some(Waitmsg { pid, time, msg })
    }

    /// The status after `name pid: `, the part the child gave `exits`.
    #[must_use]
    pub fn status(&self) -> &[u8] {
        let mut prefix = Buf::new();
        let _ = write!(prefix, " {}: ", self.pid);
        let p = prefix.as_bytes();
        self.msg
            .windows(p.len())
            .position(|w| w == p)
            .map_or(&self.msg[..], |at| &self.msg[at + p.len()..])
    }
}

/// Waits for a child to exit.
pub fn wait() -> Result<Waitmsg, Error> {
    let mut buf = [0u8; 512];
    let n = crate::await_child(&mut buf)?;
    Waitmsg::parse(&buf[..n]).ok_or_else(|| Error::from_text(b"bad wait message"))
}

/// Waits for a child to exit; returns its pid.
pub fn waitpid() -> Result<u64, Error> {
    wait().map(|w| w.pid)
}

/// Whom [`postnote`] posts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Whom {
    /// The process.
    Proc,
    /// Every process in its note group but the caller.
    Group,
}

/// Posts `note` to process `pid`, or to its note group, through `#p`
/// (so it works whatever is bound on `/proc`).
pub fn postnote(whom: Whom, pid: u64, note: &[u8]) -> Result<(), Error> {
    let mut name = Buf::new();
    let file = match whom {
        Whom::Proc => "note",
        Whom::Group => "notepg",
    };
    let _ = write!(name, "#p/{pid}/{file}\0");
    let name = core::ffi::CStr::from_bytes_with_nul(name.as_bytes()).map_err(|_| Error::from_text(b"bad pid"))?;
    let mode = OpenMode {
        access: Access::Write,
        flags: OpenFlags::empty(),
    };
    let fd = crate::open(name, mode)?;
    let r = crate::write(fd, note);
    let _ = crate::close(fd);
    match r {
        Ok(n) if n == note.len() => Ok(()),
        Ok(_) => Err(Error::from_text(b"short write")),
        Err(e) => Err(e),
    }
}

/// This process's pid, from `#c/pid`.
pub fn getpid() -> Result<u64, Error> {
    let v = crate::read_all(c"#c/pid")?;
    core::str::from_utf8(&v)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| Error::from_text(b"bad pid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_messages() {
        let mut text = [0u8; 128];
        let n = abi::format_waitmsg(42, [1, 2, 3], b"cat 42: no such file", &mut text);
        let w = Waitmsg::parse(&text[..n]).unwrap();
        assert_eq!((w.pid, w.time), (42, [1, 2, 3]));
        assert_eq!(w.msg, b"cat 42: no such file");
        assert_eq!(w.status(), b"no such file");
        let n = abi::format_waitmsg(7, [0; 3], b"", &mut text);
        let w = Waitmsg::parse(&text[..n]).unwrap();
        assert!(w.msg.is_empty() && w.status().is_empty());
    }
}
