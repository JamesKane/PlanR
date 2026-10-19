//! `exec`: refuses a missing file and bad pointers, keeping the program
//! running, then replaces it with `targs`.

#![no_std]
#![no_main]

use r::{Sys, exec};

r::main!(run);

fn run(_: r::Args) -> Result<(), &'static str> {
    let e = exec(c"/bin/nonexistent", &[c"nonexistent"]);
    if e.as_bytes() != b"'/bin/nonexistent' file does not exist" {
        return Err("exec of a missing file: wrong error");
    }
    // SAFETY: the kernel must refuse both without touching anything.
    unsafe {
        if r::syscall(Sys::Exec, [16, 0, 0, 0, 0, 0]).is_ok() {
            return Err("exec of a bad name pointer returned success");
        }
        if r::syscall(Sys::Exec, [c"/bin/targs".as_ptr() as u64, 16, 0, 0, 0, 0]).is_ok() {
            return Err("exec with a bad argv returned success");
        }
    }
    if r::Error::last().as_bytes() != b"bad address in system call" {
        return Err("exec with a bad argv: wrong error");
    }
    let _ = exec(c"/bin/targs", &[c"targs", c"from", c"exec"]);
    Err("exec of targs failed")
}
