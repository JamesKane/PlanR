//! The environment, `/env`: one file per variable. rc keeps a list as its
//! words each followed by a NUL; [`getenv`] gives the words joined by
//! spaces, as Plan 9's does, and [`getenv_list`] the words themselves.
//!
//! Also here, since it reads `$path`: [`exec_path`], which runs a command
//! found as rc would find it.

use alloc::ffi::CString;
use alloc::vec::Vec;
use core::ffi::CStr;

use crate::{Access, Error, OpenFlags, OpenMode};

/// The file holding `name`, if `name` can be a variable.
fn file(name: &[u8]) -> Option<CString> {
    if name.is_empty() || name.contains(&b'/') || name == b"." || name == b".." {
        return None;
    }
    let mut f = b"/env/".to_vec();
    f.extend_from_slice(name);
    CString::new(f).ok()
}

/// The contents of `/env/name`, as they are.
#[must_use]
pub fn getenv_raw(name: &[u8]) -> Option<Vec<u8>> {
    crate::read_all(&file(name)?).ok()
}

/// The value of `name`: NULs between words read as spaces, and a final
/// NUL is dropped.
#[must_use]
pub fn getenv(name: &[u8]) -> Option<Vec<u8>> {
    let mut v = getenv_raw(name)?;
    if v.last() == Some(&0) {
        v.pop();
    }
    for b in &mut v {
        if *b == 0 {
            *b = b' ';
        }
    }
    Some(v)
}

/// The words of list `name`, as rc wrote them; a value with no NUL is one
/// word.
#[must_use]
pub fn getenv_list(name: &[u8]) -> Option<Vec<Vec<u8>>> {
    let v = getenv_raw(name)?;
    let body = v.strip_suffix(&[0]).unwrap_or(&v);
    if v.is_empty() {
        return Some(Vec::new());
    }
    Some(body.split(|&b| b == 0).map(<[u8]>::to_vec).collect())
}

/// Sets `name` to `value`.
pub fn putenv(name: &[u8], value: &[u8]) -> Result<(), Error> {
    let f = file(name).ok_or_else(|| Error::from_text(b"bad environment variable name"))?;
    let mode = OpenMode {
        access: Access::Write,
        flags: OpenFlags::TRUNC,
    };
    let fd = crate::create(&f, mode, 0o664)?;
    let r = crate::io::Write::write_all(&mut crate::File(fd), value);
    let _ = crate::close(fd);
    r
}

/// Sets list `name` to `words`, as rc does: each followed by a NUL.
pub fn putenv_list(name: &[u8], words: &[&[u8]]) -> Result<(), Error> {
    let mut v = Vec::new();
    for w in words {
        v.extend_from_slice(w);
        v.push(0);
    }
    putenv(name, &v)
}

/// Whether `name` is run as it is, not looked for in `$path`: it starts
/// at the root, a device, `.` or `..`.
#[must_use]
pub fn is_explicit(name: &[u8]) -> bool {
    name.starts_with(b"/") || name.starts_with(b"#") || name.starts_with(b"./") || name.starts_with(b"../")
}

/// The directories `$path` names, else `.` and `/bin`.
#[must_use]
pub fn path() -> Vec<Vec<u8>> {
    getenv_list(b"path").unwrap_or_else(|| alloc::vec![b".".to_vec(), b"/bin".to_vec()])
}

/// Runs `name` on `argv` as [`crate::exec`] does, looking for it in each
/// `$path` directory unless [`is_explicit`]. Returns only if every try
/// failed, with the first failure that was not "file does not exist" (else
/// that one).
#[must_use]
pub fn exec_path(name: &[u8], argv: &[&CStr]) -> Error {
    let try_one = |full: Vec<u8>| match CString::new(full) {
        Ok(c) => crate::exec(&c, argv),
        Err(_) => Error::from_text(b"bad character in file name"),
    };
    if is_explicit(name) {
        return try_one(name.to_vec());
    }
    let mut first = None;
    for dir in path() {
        let mut full = dir;
        if !full.is_empty() && full.last() != Some(&b'/') {
            full.push(b'/');
        }
        full.extend_from_slice(name);
        let e = try_one(full);
        if e.as_bytes() != b"file does not exist" {
            return e;
        }
        first.get_or_insert(e);
    }
    first.unwrap_or_else(|| Error::from_text(b"file does not exist"))
}
