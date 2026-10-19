//! Console input, typed by the test harness (`cargo xtask test`'s `cons`
//! boot): after each prompt line it prints, the harness types what the
//! step expects. Run by `init -i`, with `/dev/cons` as descriptors 0 and
//! 1 and a note group of its own.
//!
//! The steps: a line, with `\r` read as `\n`; erasing a character
//! (UTF-8 included) and the line, ^D with and without a line typed; raw
//! mode, where bytes come as typed; closing `consctl` ending raw mode; and
//! DEL interrupting a read.

#![no_std]
#![no_main]

use core::ffi::CStr;
use core::sync::atomic::{AtomicUsize, Ordering};

use r::{Access, NoteHow, OpenFlags, OpenMode};

r::main!(run);

type Check = Result<(), &'static str>;

fn ensure(ok: bool, why: &'static str) -> Check {
    if ok { Ok(()) } else { Err(why) }
}

const WRITE: OpenMode = OpenMode {
    access: Access::Write,
    flags: OpenFlags::empty(),
};

/// Prints a prompt line, for the harness to answer.
fn prompt(text: &[u8]) {
    let _ = r::write(1, text);
}

/// One read of the console.
fn read(buf: &mut [u8]) -> Result<&[u8], &'static str> {
    let n = r::read(0, buf).map_err(|_| "console read failed")?;
    Ok(&buf[..n])
}

/// Reads must give `want`, one block each.
fn reads(want: &[&[u8]], why: &'static str) -> Check {
    let mut buf = [0u8; 128];
    for w in want {
        ensure(read(&mut buf)? == *w, why)?;
    }
    Ok(())
}

/// Interrupt notes the handler has taken.
static INTERRUPTS: AtomicUsize = AtomicUsize::new(0);

extern "C" fn handler(_: *mut r::Ureg, note: *const u8) {
    // SAFETY: the kernel passes the note's NUL-terminated text.
    let text = unsafe { CStr::from_ptr(note.cast()) }.to_bytes();
    if text == b"interrupt" {
        INTERRUPTS.fetch_add(1, Ordering::AcqRel);
    }
    let _ = r::noted(NoteHow::Cont);
    r::exits(Some(b"noted failed"));
}

fn run(_: r::Args) -> Result<(), &'static str> {
    prompt(b"tcons: type a line\n");
    reads(&[b"hello, console\n"], "the line read back wrong")?;

    prompt(b"tcons: edit\n");
    // "abx" ^H "c" CR, "junk" ^U "ok" CR, "é" ^H "z" CR, "part" ^D, ^D.
    reads(
        &[b"abc\n", b"ok\n", b"z\n", b"part", b""],
        "an edited line read back wrong",
    )?;

    let ctl = r::open(c"/dev/consctl", WRITE).map_err(|_| "cannot open /dev/consctl")?;
    ensure(r::write(ctl, b"rawon") == Ok(5), "cannot write rawon")?;
    ensure(r::write(ctl, b"nonsense").is_err(), "consctl took nonsense")?;
    prompt(b"tcons: raw\n");
    // "r" DEL "q" CR, as typed.
    let mut got = r::Buf::new();
    let mut buf = [0u8; 16];
    while got.as_bytes().len() < 4 {
        got.put(read(&mut buf)?);
    }
    ensure(got.as_bytes() == b"r\x7fq\r", "raw input was changed")?;
    // Closing the last consctl ends raw mode.
    r::close(ctl).map_err(|_| "close failed")?;

    r::notify(Some(handler)).map_err(|_| "notify failed")?;
    prompt(b"tcons: interrupt\n");
    // DEL: the read is cut short, or, if the note came first, the alarm
    // ends it.
    r::alarm(5000);
    let r = r::read(0, &mut buf);
    r::alarm(0);
    ensure(r.is_err(), "the read was not interrupted")?;
    ensure(INTERRUPTS.load(Ordering::Acquire) == 1, "no interrupt note came")?;

    prompt(b"tcons: after\n");
    reads(&[b"still here\n"], "a line after the interrupt read back wrong")?;
    Ok(())
}
