//! Errors: what a failed operation says. As in Plan 9 the text is the
//! error, and a system call that fails hands it to the process for
//! `errstr`. The common texts are Plan 9's (`port/error.h`), so programs
//! and scripts that match on them keep working.

use alloc::borrow::Cow;
use alloc::string::String;
use core::fmt;

use crate::port::uaccess::BadAddress;

/// Why an operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Error(Cow<'static, str>);

impl Error {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&'static str> for Error {
    fn from(s: &'static str) -> Error {
        Error(Cow::Borrowed(s))
    }
}

impl From<String> for Error {
    fn from(s: String) -> Error {
        Error(Cow::Owned(s))
    }
}

impl From<BadAddress> for Error {
    fn from(_: BadAddress) -> Error {
        Error::from(EBADADDR)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

pub(crate) type Result<T> = core::result::Result<T, Error>;

pub(crate) const EBADADDR: &str = "bad address in system call";
pub(crate) const EBADARG: &str = "bad arg in system call";
pub(crate) const ENONEXIST: &str = "file does not exist";
pub(crate) const EEXIST: &str = "file already exists";
pub(crate) const EPERM: &str = "permission denied";
pub(crate) const EISDIR: &str = "file is a directory";
pub(crate) const ENOTDIR: &str = "not a directory";
pub(crate) const EBADFD: &str = "fd out of range or not open";
pub(crate) const EBADUSEFD: &str = "inappropriate use of fd";
pub(crate) const ENOFD: &str = "no free file descriptors";
pub(crate) const ENOCREATE: &str = "mounted directory forbids creation";
pub(crate) const EMOUNT: &str = "inconsistent mount";
pub(crate) const EUNMOUNT: &str = "not mounted";
pub(crate) const ESHORTSTAT: &str = "stat buffer too small";
pub(crate) const EBADSTAT: &str = "malformed stat buffer";
pub(crate) const EBADSHARP: &str = "unknown device in # filename";
pub(crate) const EBADCHAR: &str = "bad character in file name";
pub(crate) const EBADSPEC: &str = "bad attach specifier";
pub(crate) const EBADOFFSET: &str = "bad offset in directory read";
pub(crate) const ENONS: &str = "no name space";
pub(crate) const ENOMEM: &str = "out of memory";
pub(crate) const EHUNGUP: &str = "i/o on hungup channel";
pub(crate) const ESHUTDOWN: &str = "device shut down";
pub(crate) const ENOATTACH: &str = "mount/attach disallowed";
pub(crate) const EINTR: &str = "interrupted";
pub(crate) const EPROCDIED: &str = "process exited";
pub(crate) const EBADCTL: &str = "bad process or channel control request";
