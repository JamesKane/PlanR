//! libr: the Plan R user runtime. It will replace libc, lib9, bio and
//! thread. So far: the start-up code, the system calls, an exit on panic,
//! the heap ([`heap`]), formatted output ([`print`]), options ([`arg`]),
//! UTF-8 ([`utf`]), buffered I/O ([`bio`], on [`io`]), directory entries
//! ([`dir`]), the environment and `$path` ([`env`]), waiting and notes
//! ([`proc`]), time ([`time`]) and text utilities ([`text`]).
//!
//! The file calls take names as `&CStr` and give errors as [`Error`].
//!
//! A program is `#![no_std]` and `#![no_main]`, links with `user.ld`, and
//! names its main function with [`main!`]:
//!
//! ```ignore
//! r::main!(run);
//! fn run(args: r::Args) -> Result<(), &'static str> { … }
//! ```
//!
//! `Ok` exits normally; `Err` exits with its text as the status.

#![cfg_attr(not(test), no_std)]
// An error is its text, as Plan 9's errstr gives it: ERRMAX bytes, kept by
// value so a failed call needs no allocator.
#![allow(clippy::result_large_err)]

extern crate alloc;

pub mod arg;
pub mod bio;
pub mod dir;
pub mod env;
pub mod heap;
pub mod io;
pub mod print;
pub mod proc;
pub mod text;
pub mod time;
pub mod utf;

use core::arch::asm;
#[cfg(not(test))]
use core::arch::global_asm;
use core::ffi::CStr;
use core::fmt;

pub use arg::Opts;
pub use bio::{Bin, Bout};
pub use dir::{DirBuf, dirfstat, dirfwstat, dirread, dirreadall, dirstat, dirwstat};
pub use env::{exec_path, getenv, getenv_list, putenv, putenv_list};
pub use io::{File, Read, STDERR, STDIN, STDOUT, Write};
pub use print::{Quoted, RcQuoted, Rerr, Text};
pub use proc::{Waitmsg, Whom, getpid, postnote, wait, waitpid};
pub use text::{cleanname, getfields, gettokens, quote, rc_quote, tokenize, unquote};

pub use abi::{
    Access, BindFlags, Dir, ERRMAX, Failed, NoteHow, OpenFlags, OpenMode, Qid, RforkFlags, SegAttr, Sys, Ureg, Whence,
    dirs,
};

/// Makes `$f`, a `fn(Args) -> Result<(), &'static str>`, the program's
/// main function.
#[macro_export]
macro_rules! main {
    ($f:path) => {
        #[unsafe(no_mangle)]
        fn __libr_main(args: $crate::Args) -> Result<(), &'static str> {
            $f(args)
        }
    };
}

#[cfg(not(test))]
unsafe extern "Rust" {
    /// Defined by [`main!`].
    safe fn __libr_main(args: Args) -> Result<(), &'static str>;
}

// The entry point: hand the start-up block (ADR 0002) to `start`, with the
// stack aligned as a call leaves it and no frame record above.
#[cfg(all(not(test), target_arch = "x86_64"))]
global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    "_start:",
    "    xor ebp, ebp",
    "    mov rdi, rsp",
    "    call {start}",
    "    ud2",
    start = sym start,
);

#[cfg(all(not(test), target_arch = "aarch64"))]
global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    "_start:",
    "    mov x29, xzr",
    "    mov x30, xzr",
    "    mov x0, sp",
    "    bl {start}",
    "    brk #0",
    start = sym start,
);

/// Runs the program's main function on the arguments at `block`.
#[cfg(not(test))]
extern "C" fn start(block: *const u64) -> ! {
    // SAFETY: the kernel put argc and the argument pointers at `block`.
    let args = unsafe {
        Args {
            argc: usize::try_from(*block).unwrap_or(0),
            argv: block.add(1).cast(),
        }
    };
    match __libr_main(args) {
        Ok(()) => exits(None),
        Err(e) => exits(Some(e.as_bytes())),
    }
}

/// The program's arguments.
#[derive(Debug, Clone, Copy)]
pub struct Args {
    argc: usize,
    argv: *const *const u8,
}

impl Args {
    #[must_use]
    pub fn len(&self) -> usize {
        self.argc
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.argc == 0
    }

    /// Argument `i`, without its NUL.
    #[must_use]
    pub fn get(&self, i: usize) -> Option<&'static [u8]> {
        if i >= self.argc {
            return None;
        }
        // SAFETY: the kernel wrote `argc` pointers to NUL-terminated
        // strings, which stay put for the life of the program.
        Some(unsafe { CStr::from_ptr((*self.argv.add(i)).cast()) }.to_bytes())
    }

    pub fn iter(&self) -> impl Iterator<Item = &'static [u8]> + '_ {
        (0..self.argc).filter_map(|i| self.get(i))
    }
}

/// Makes system call `n` with `a`.
///
/// # Safety
///
/// The arguments must be what the call expects: pointers it writes through
/// must be valid, and so on.
pub unsafe fn syscall(n: Sys, a: [u64; abi::MAX_ARGS]) -> Result<u64, Failed> {
    // SAFETY: the caller's contract.
    unsafe { raw_syscall(n as u64, a) }
}

/// Makes the system call numbered `n`, which need not exist.
///
/// # Safety
///
/// As for [`syscall`].
#[cfg(target_arch = "x86_64")]
pub unsafe fn raw_syscall(n: u64, a: [u64; abi::MAX_ARGS]) -> Result<u64, Failed> {
    let (value, error);
    // SAFETY: the caller's contract; the kernel preserves every register
    // but the results, rcx and r11.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n => value,
            in("rdi") a[0],
            in("rsi") a[1],
            inlateout("rdx") a[2] => error,
            in("r10") a[3],
            in("r8") a[4],
            in("r9") a[5],
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    abi::decode([value, error])
}

/// Makes the system call numbered `n`, which need not exist.
///
/// # Safety
///
/// As for [`syscall`].
#[cfg(target_arch = "aarch64")]
pub unsafe fn raw_syscall(n: u64, a: [u64; abi::MAX_ARGS]) -> Result<u64, Failed> {
    let (value, error);
    // SAFETY: the caller's contract; the kernel preserves every register
    // but the results.
    unsafe {
        asm!(
            "svc #0",
            in("x8") n,
            inlateout("x0") a[0] => value,
            inlateout("x1") a[1] => error,
            in("x2") a[2],
            in("x3") a[3],
            in("x4") a[4],
            in("x5") a[5],
            options(nostack),
        );
    }
    abi::decode([value, error])
}

/// The text of a failed call, fetched with `errstr`.
#[derive(Clone, Copy)]
pub struct Error {
    text: [u8; ERRMAX],
}

impl Error {
    /// Takes the error text of the last failed call, leaving none.
    #[must_use]
    pub fn last() -> Error {
        let mut text = [0; ERRMAX];
        let _ = errstr(&mut text);
        Error { text }
    }

    /// The text, without its NUL.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        let n = self.text.iter().position(|&b| b == 0).unwrap_or(ERRMAX);
        &self.text[..n]
    }
}

impl PartialEq for Error {
    fn eq(&self, other: &Error) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl Eq for Error {}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Error(\"{}\")", self.as_bytes().escape_ascii())
    }
}

/// A fixed buffer for building text, cut at [`ERRMAX`] - 1 bytes.
#[derive(Clone, Copy)]
pub struct Buf {
    bytes: [u8; ERRMAX],
    len: usize,
}

impl Buf {
    #[must_use]
    pub const fn new() -> Buf {
        Buf {
            bytes: [0; ERRMAX],
            len: 0,
        }
    }

    /// Appends what fits of `b`.
    pub fn put(&mut self, b: &[u8]) {
        let n = b.len().min(ERRMAX - 1 - self.len);
        self.bytes[self.len..self.len + n].copy_from_slice(&b[..n]);
        self.len += n;
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl Default for Buf {
    fn default() -> Buf {
        Buf::new()
    }
}

impl fmt::Write for Buf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.put(s.as_bytes());
        Ok(())
    }
}

impl fmt::Debug for Buf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(core::str::from_utf8(self.as_bytes()).unwrap_or("?"))
    }
}

/// Fails with the last error's text.
fn failed<T>(_: Failed) -> Result<T, Error> {
    Err(Error::last())
}

/// Ends the program; `None` (or an empty message) is a normal exit.
pub fn exits(msg: Option<&[u8]>) -> ! {
    let mut buf = [0u8; ERRMAX];
    let ptr = match msg {
        Some(m) => {
            let n = m.len().min(ERRMAX - 1);
            buf[..n].copy_from_slice(&m[..n]);
            buf.as_ptr() as u64
        }
        None => 0,
    };
    // SAFETY: `buf` is NUL-terminated.
    let _ = unsafe { syscall(Sys::Exits, [ptr, 0, 0, 0, 0, 0]) };
    unreachable!("exits returned")
}

/// Swaps the process's error text with the NUL-terminated text in `buf`.
pub fn errstr(buf: &mut [u8]) -> Result<(), Failed> {
    // SAFETY: the kernel writes at most `buf.len()` bytes.
    unsafe { syscall(Sys::Errstr, [buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0, 0]) }.map(drop)
}

/// The process's error text, left as it was (Plan 9's `rerrstr`).
#[must_use]
pub fn rerrstr() -> Error {
    let mut text = [0; ERRMAX];
    let _ = errstr(&mut text);
    let mut back = text;
    let _ = errstr(&mut back);
    Error { text }
}

/// The most arguments [`exec`] passes.
pub const MAX_EXEC_ARGS: usize = 64;

/// Replaces the program with `name`, run on `argv`; returns only if that
/// fails.
#[must_use]
pub fn exec(name: &CStr, argv: &[&CStr]) -> Error {
    if argv.len() > MAX_EXEC_ARGS {
        return Error::from_text(b"too many arguments");
    }
    let mut ptrs = [core::ptr::null::<u8>(); MAX_EXEC_ARGS + 1];
    for (p, a) in ptrs.iter_mut().zip(argv) {
        *p = a.as_ptr().cast();
    }
    // SAFETY: `name` and every argument are NUL-terminated, and the
    // pointer list ends with a null.
    match unsafe { syscall(Sys::Exec, [name.as_ptr() as u64, ptrs.as_ptr() as u64, 0, 0, 0, 0]) } {
        Ok(_) => Error::from_text(b"exec returned"),
        Err(_) => Error::last(),
    }
}

impl Error {
    /// An error with text `t`, cut to fit.
    #[must_use]
    pub fn from_text(t: &[u8]) -> Error {
        let mut text = [0; ERRMAX];
        let n = t.len().min(ERRMAX - 1);
        text[..n].copy_from_slice(&t[..n]);
        Error { text }
    }
}

/// Moves the end of the bss segment to `addr`.
pub fn brk(addr: u64) -> Result<(), Error> {
    // SAFETY: changes only the caller's memory.
    unsafe { syscall(Sys::Brk, [addr, 0, 0, 0, 0, 0]) }
        .map(drop)
        .or_else(failed)
}

unsafe extern "C" {
    /// The end of the bss (`user.ld`).
    static end: u8;
}

/// The end of the bss as loaded, where [`brk`] starts.
#[must_use]
pub fn end_of_bss() -> u64 {
    (&raw const end) as u64
}

/// Sets the thread pointer.
pub fn settls(addr: u64) -> Result<(), Error> {
    // SAFETY: the thread pointer is the caller's own.
    unsafe { syscall(Sys::Settls, [addr, 0, 0, 0, 0, 0]) }
        .map(drop)
        .or_else(failed)
}

/// Sleeps for `ms` milliseconds; 0 yields. A note cuts it short.
pub fn sleep(ms: u64) -> Result<(), Error> {
    // SAFETY: no memory is involved.
    unsafe { syscall(Sys::Sleep, [ms, 0, 0, 0, 0, 0]) }
        .map(drop)
        .or_else(failed)
}

/// Waits for a child to exit and puts its wait message in `buf`; returns
/// the message's length.
pub fn await_child(buf: &mut [u8]) -> Result<usize, Error> {
    // SAFETY: the kernel writes at most `buf.len()` bytes.
    match unsafe { syscall(Sys::Await, [buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0, 0]) } {
        #[allow(clippy::cast_possible_truncation)] // at most buf.len()
        Ok(n) => Ok(n as usize),
        Err(f) => failed(f),
    }
}

/// Makes a new process, or changes the caller's, as `flags` say: the
/// child's pid in the parent, 0 in the child.
pub fn rfork(flags: RforkFlags) -> Result<u64, Error> {
    // SAFETY: the child returns here too, with a copy of the stack.
    unsafe { syscall(Sys::Rfork, [flags.bits(), 0, 0, 0, 0, 0]) }.or_else(failed)
}

/// Waits for another process of the rendezvous group to call with `tag`;
/// returns its `value`.
pub fn rendezvous(tag: u64, value: u64) -> Result<u64, Error> {
    // SAFETY: no memory is involved.
    unsafe { syscall(Sys::Rendezvous, [tag, value, 0, 0, 0, 0]) }.or_else(failed)
}

/// A note handler: called with the interrupted registers and the note's
/// NUL-terminated text, it must end with [`noted`].
pub type Handler = extern "C" fn(*mut Ureg, *const u8);

/// Sets the note handler; `None` lets notes end the process.
pub fn notify(f: Option<Handler>) -> Result<(), Error> {
    let addr = f.map_or(0, |f| f as usize as u64);
    // SAFETY: the kernel calls the handler as a `Handler`.
    unsafe { syscall(Sys::Notify, [addr, 0, 0, 0, 0, 0]) }
        .map(drop)
        .or_else(failed)
}

/// Ends a note handler as `how` says; returns only if that fails.
#[must_use]
pub fn noted(how: NoteHow) -> Error {
    // SAFETY: resumes the code the note interrupted, from its `Ureg`.
    let _ = unsafe { syscall(Sys::Noted, [how as u64, 0, 0, 0, 0, 0]) };
    Error::last()
}

/// Posts the note `alarm` in `ms` milliseconds, in place of any alarm set
/// before (0 sets none); returns the milliseconds that one had left.
#[allow(clippy::must_use_candidate)] // setting it is the point
pub fn alarm(ms: u64) -> u64 {
    // SAFETY: no memory is involved.
    unsafe { syscall(Sys::Alarm, [ms, 0, 0, 0, 0, 0]) }.unwrap_or(0)
}

/// Nanoseconds since 1970-01-01 UTC.
#[must_use]
pub fn nsec() -> u64 {
    // SAFETY: no memory is involved.
    unsafe { syscall(Sys::Nsec, [0; 6]) }.unwrap_or(0)
}

/// Takes one from the count at `addr` if it is positive; else, if
/// `block`, waits until it is. True if one was taken.
pub fn semacquire(addr: &core::sync::atomic::AtomicI64, block: bool) -> Result<bool, Error> {
    // SAFETY: the kernel changes the word only atomically.
    unsafe { syscall(Sys::Semacquire, [addr.as_ptr() as u64, u64::from(block), 0, 0, 0, 0]) }
        .map(|v| v != 0)
        .or_else(failed)
}

/// [`semacquire`], waiting at most `ms` milliseconds.
pub fn tsemacquire(addr: &core::sync::atomic::AtomicI64, ms: u64) -> Result<bool, Error> {
    // SAFETY: as for `semacquire`.
    unsafe { syscall(Sys::Tsemacquire, [addr.as_ptr() as u64, ms, 0, 0, 0, 0]) }
        .map(|v| v != 0)
        .or_else(failed)
}

/// Adds `count` to the count at `addr` and wakes its waiters; returns the
/// new count.
pub fn semrelease(addr: &core::sync::atomic::AtomicI64, count: u64) -> Result<i64, Error> {
    // SAFETY: as for `semacquire`.
    match unsafe { syscall(Sys::Semrelease, [addr.as_ptr() as u64, count, 0, 0, 0, 0]) } {
        #[allow(clippy::cast_possible_wrap)] // a signed count
        Ok(v) => Ok(v as i64),
        Err(f) => failed(f),
    }
}

/// Attaches a new segment of `class` (`shared` or `memory`), `len`
/// bytes, at `va` or, if 0, where the kernel chooses; returns its base.
pub fn segattach(attr: SegAttr, class: &CStr, va: u64, len: u64) -> Result<u64, Error> {
    // SAFETY: a new segment only adds memory, where nothing is.
    unsafe { syscall(Sys::Segattach, [attr.bits(), ptr(class), va, len, 0, 0]) }.or_else(failed)
}

/// Detaches the attached segment holding `va`.
///
/// # Safety
///
/// Nothing may use the segment's memory after.
pub unsafe fn segdetach(va: u64) -> Result<(), Error> {
    // SAFETY: the caller's contract.
    unsafe { syscall(Sys::Segdetach, [va, 0, 0, 0, 0, 0]) }
        .map(drop)
        .or_else(failed)
}

/// Lets go of the pages wholly inside `[va, va + len)`, which read as
/// zeros after.
///
/// # Safety
///
/// Nothing may count on what those pages held.
pub unsafe fn segfree(va: u64, len: u64) -> Result<(), Error> {
    // SAFETY: the caller's contract.
    unsafe { syscall(Sys::Segfree, [va, len, 0, 0, 0, 0]) }
        .map(drop)
        .or_else(failed)
}

/// Makes writes in `[va, va + len)` visible to instruction fetch.
pub fn segflush(va: u64, len: u64) -> Result<(), Error> {
    // SAFETY: changes nothing in memory.
    unsafe { syscall(Sys::Segflush, [va, len, 0, 0, 0, 0]) }
        .map(drop)
        .or_else(failed)
}

/// A file descriptor.
pub type Fd = usize;

/// A new pipe: what is written to one descriptor is read from the other.
pub fn pipe() -> Result<[Fd; 2], Error> {
    let mut fds = [0u64; 2];
    // SAFETY: the kernel writes the two words.
    unsafe { syscall(Sys::Pipe, [fds.as_mut_ptr() as u64, 0, 0, 0, 0, 0]) }.or_else(failed)?;
    #[allow(clippy::cast_possible_truncation)] // descriptors
    Ok(fds.map(|f| f as Fd))
}

/// Makes call `n` with `a` and gives its value as a count or descriptor.
///
/// # Safety
///
/// As for [`syscall`].
unsafe fn call_n(n: Sys, a: [u64; abi::MAX_ARGS]) -> Result<usize, Error> {
    // SAFETY: the caller's contract.
    match unsafe { syscall(n, a) } {
        #[allow(clippy::cast_possible_truncation)] // a count or descriptor
        Ok(v) => Ok(v as usize),
        Err(f) => failed(f),
    }
}

fn ptr(s: &CStr) -> u64 {
    s.as_ptr() as u64
}

/// Opens `name`.
pub fn open(name: &CStr, mode: OpenMode) -> Result<Fd, Error> {
    // SAFETY: `name` is NUL-terminated.
    unsafe { call_n(Sys::Open, [ptr(name), mode.raw(), 0, 0, 0, 0]) }
}

/// Creates `name` with permissions `perm` (`DMDIR` for a directory) and
/// opens it.
pub fn create(name: &CStr, mode: OpenMode, perm: u32) -> Result<Fd, Error> {
    // SAFETY: `name` is NUL-terminated.
    unsafe { call_n(Sys::Create, [ptr(name), mode.raw(), u64::from(perm), 0, 0, 0]) }
}

pub fn close(fd: Fd) -> Result<(), Error> {
    // SAFETY: no memory is involved.
    unsafe { call_n(Sys::Close, [fd as u64, 0, 0, 0, 0, 0]) }.map(drop)
}

/// Reads at the descriptor's offset.
pub fn read(fd: Fd, buf: &mut [u8]) -> Result<usize, Error> {
    // SAFETY: the kernel writes at most `buf.len()` bytes.
    unsafe {
        call_n(
            Sys::Read,
            [fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0],
        )
    }
}

/// Writes at the descriptor's offset.
pub fn write(fd: Fd, buf: &[u8]) -> Result<usize, Error> {
    // SAFETY: the kernel only reads `buf`.
    unsafe { call_n(Sys::Write, [fd as u64, buf.as_ptr() as u64, buf.len() as u64, 0, 0, 0]) }
}

pub fn pread(fd: Fd, buf: &mut [u8], offset: u64) -> Result<usize, Error> {
    // SAFETY: the kernel writes at most `buf.len()` bytes.
    unsafe {
        call_n(
            Sys::Pread,
            [fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64, offset, 0, 0],
        )
    }
}

pub fn pwrite(fd: Fd, buf: &[u8], offset: u64) -> Result<usize, Error> {
    // SAFETY: the kernel only reads `buf`.
    unsafe {
        call_n(
            Sys::Pwrite,
            [fd as u64, buf.as_ptr() as u64, buf.len() as u64, offset, 0, 0],
        )
    }
}

/// Moves the descriptor's offset; returns the new one.
pub fn seek(fd: Fd, offset: i64, whence: Whence) -> Result<u64, Error> {
    #[allow(clippy::cast_sign_loss)] // passed as the bits of an i64
    // SAFETY: no memory is involved.
    unsafe { syscall(Sys::Seek, [fd as u64, offset as u64, whence as u64, 0, 0, 0]) }.or_else(failed)
}

/// Puts `name`'s encoded [`Dir`] in `buf` and returns its length; if
/// `buf` is too small, only the size field fits, and 2 is returned.
pub fn stat(name: &CStr, buf: &mut [u8]) -> Result<usize, Error> {
    // SAFETY: the kernel writes at most `buf.len()` bytes.
    unsafe {
        call_n(
            Sys::Stat,
            [ptr(name), buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0],
        )
    }
}

/// As [`stat`], for an open file.
pub fn fstat(fd: Fd, buf: &mut [u8]) -> Result<usize, Error> {
    // SAFETY: the kernel writes at most `buf.len()` bytes.
    unsafe {
        call_n(
            Sys::Fstat,
            [fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0],
        )
    }
}

/// Changes what the encoded [`Dir`] in `buf` says to change.
pub fn wstat(name: &CStr, buf: &[u8]) -> Result<(), Error> {
    // SAFETY: the kernel only reads `buf`.
    unsafe { call_n(Sys::Wstat, [ptr(name), buf.as_ptr() as u64, buf.len() as u64, 0, 0, 0]) }.map(drop)
}

pub fn fwstat(fd: Fd, buf: &[u8]) -> Result<(), Error> {
    // SAFETY: the kernel only reads `buf`.
    unsafe { call_n(Sys::Fwstat, [fd as u64, buf.as_ptr() as u64, buf.len() as u64, 0, 0, 0]) }.map(drop)
}

pub fn remove(name: &CStr) -> Result<(), Error> {
    // SAFETY: `name` is NUL-terminated.
    unsafe { call_n(Sys::Remove, [ptr(name), 0, 0, 0, 0, 0]) }.map(drop)
}

/// Puts the name `fd` was opened by in `buf`, NUL-terminated, cut to fit;
/// returns its length.
pub fn fd2path(fd: Fd, buf: &mut [u8]) -> Result<usize, Error> {
    // SAFETY: the kernel writes at most `buf.len()` bytes.
    unsafe {
        call_n(
            Sys::Fd2path,
            [fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0],
        )
    }?;
    Ok(buf.iter().position(|&b| b == 0).unwrap_or(buf.len()))
}

/// Makes `new` (the lowest free descriptor if `None`) refer to what `old`
/// does.
pub fn dup(old: Fd, new: Option<Fd>) -> Result<Fd, Error> {
    let new = new.map_or(u64::MAX, |n| n as u64);
    // SAFETY: no memory is involved.
    unsafe { call_n(Sys::Dup, [old as u64, new, 0, 0, 0, 0]) }
}

pub fn chdir(name: &CStr) -> Result<(), Error> {
    // SAFETY: `name` is NUL-terminated.
    unsafe { call_n(Sys::Chdir, [ptr(name), 0, 0, 0, 0, 0]) }.map(drop)
}

/// Binds `name` onto `old`.
pub fn bind(name: &CStr, old: &CStr, flags: BindFlags) -> Result<(), Error> {
    // SAFETY: both names are NUL-terminated.
    unsafe { call_n(Sys::Bind, [ptr(name), ptr(old), flags.bits(), 0, 0, 0]) }.map(drop)
}

/// `b` as a C string; fails if it holds a NUL.
pub fn cstr(b: &[u8]) -> Result<alloc::ffi::CString, Error> {
    alloc::ffi::CString::new(b).map_err(|_| Error::from_text(b"bad character in file name"))
}

/// The whole of file `name`.
pub fn read_all(name: &CStr) -> Result<alloc::vec::Vec<u8>, Error> {
    let mode = OpenMode {
        access: Access::Read,
        flags: OpenFlags::empty(),
    };
    let fd = open(name, mode)?;
    let mut v = alloc::vec::Vec::new();
    let mut buf = [0u8; 8192];
    let r = loop {
        match read(fd, &mut buf) {
            Ok(0) => break Ok(v),
            Ok(n) => v.extend_from_slice(&buf[..n]),
            Err(e) => break Err(e),
        }
    };
    let _ = close(fd);
    r
}

/// Unmounts `name` from `old`, or everything there if `name` is `None`.
pub fn unmount(name: Option<&CStr>, old: &CStr) -> Result<(), Error> {
    // SAFETY: both names are NUL-terminated, or the first null.
    unsafe { call_n(Sys::Unmount, [name.map_or(0, ptr), ptr(old), 0, 0, 0, 0]) }.map(drop)
}

/// Attaches to the 9P server on `fd` and mounts its tree `aname` on
/// `old`; `afd` is from [`fauth`], if the server wants authentication.
pub fn mount(fd: Fd, afd: Option<Fd>, old: &CStr, flags: BindFlags, aname: &CStr) -> Result<(), Error> {
    let afd = afd.map_or(u64::MAX, |a| a as u64);
    // SAFETY: both names are NUL-terminated.
    unsafe { call_n(Sys::Mount, [fd as u64, afd, ptr(old), flags.bits(), ptr(aname), 0]) }.map(drop)
}

/// Sets up the 9P session on `fd`, with messages of at most `msize` bytes
/// (0 for the kernel's choice) in the version `buf` holds (empty for
/// 9P2000); the session's version is left in `buf`, and its length
/// returned.
pub fn fversion(fd: Fd, msize: u32, buf: &mut [u8]) -> Result<usize, Error> {
    // SAFETY: the kernel reads and writes at most `buf.len()` bytes.
    unsafe {
        call_n(
            Sys::Fversion,
            [
                fd as u64,
                u64::from(msize),
                buf.as_mut_ptr() as u64,
                buf.len() as u64,
                0,
                0,
            ],
        )
    }
}

/// A file for authenticating to the 9P server on `fd`, for `aname`.
pub fn fauth(fd: Fd, aname: &CStr) -> Result<Fd, Error> {
    // SAFETY: `aname` is NUL-terminated.
    unsafe { call_n(Sys::Fauth, [fd as u64, ptr(aname), 0, 0, 0, 0]) }
}

#[cfg(not(test))]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    let mut b = Buf::new();
    let _ = fmt::write(&mut b, format_args!("panic: {}", info.message()));
    if let Some(l) = info.location() {
        let _ = fmt::write(&mut b, format_args!(" at {}:{}", l.file(), l.line()));
    }
    exits(Some(b.as_bytes()))
}
