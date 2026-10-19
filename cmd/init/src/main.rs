//! `/$cputype/init [-ctm] [cmd]`, as Plan 9's `init` (`cmd/init.c`): run
//! by boot, it sets its priority, puts `objtype`, `service` and
//! `timezone` in the environment, builds the name space from
//! `/lib/namespace` (`newns`), and runs rc on the console for good. A
//! terminal (`-t`) first runs `termrc` and the user's `lib/profile`
//! through `rc -c`; a CPU server (`-c`, the default) runs `cpurc` unless
//! `-m` (manual) says not to. With `cmd`, the first rc runs that instead.
//! Each time rc exits, init starts another a second later.

#![no_std]
#![no_main]
// libr's errors are their text, kept by value.
#![allow(clippy::result_large_err)]

extern crate alloc;

use alloc::ffi::CString;
use alloc::format;
use alloc::vec::Vec;
use core::ffi::CStr;
use core::sync::atomic::{AtomicBool, Ordering};

use r::{Access, NoteHow, OpenFlags, OpenMode, Rerr, RforkFlags as F, Text};

r::main!(run);

const fn mode(access: Access) -> OpenMode {
    OpenMode {
        access,
        flags: OpenFlags::empty(),
    }
}

/// What the rc to run should do.
struct Init {
    service: &'static [u8],
    cmd: Option<Vec<u8>>,
    manual: bool,
    iscpu: bool,
}

fn run(args: r::Args) -> Result<(), &'static str> {
    closefds();
    r::alarm(0);
    let mut o = r::Opts::new(args.iter());
    let mut init = Init {
        service: b"cpu",
        cmd: None,
        manual: false,
        iscpu: false,
    };
    while let Some(c) = o.next() {
        match c {
            'c' => init.service = b"cpu",
            'm' => init.manual = true,
            't' => init.service = b"terminal",
            _ => {}
        }
    }
    init.cmd = o.rest().first().map(|c| c.to_vec());

    let pid = r::getpid().unwrap_or(0);
    let ctl = format!("#p/{pid}/ctl");
    match r::cstr(ctl.as_bytes()).and_then(|c| r::open(&c, mode(Access::Write))) {
        Err(_) => r::println!("init: warning: can't open {ctl}: {Rerr}"),
        Ok(fd) => {
            if r::write(fd, b"pri 10") != Ok(6) {
                r::println!("init: warning: can't set priority: {Rerr}");
            }
            let _ = r::close(fd);
        }
    }

    let cpu = readenv(c"#e/cputype");
    setenv(c"#e/objtype", &cpu);
    setenv(c"#e/service", init.service);
    cpenv(c"/adm/timezone/local", c"#e/timezone");
    let user = readenv(c"#c/user");
    let _systemname = readenv(c"#c/sysname");

    let _ = r::newns(&user, None);
    init.iscpu = init.service == b"cpu";

    if init.iscpu && !init.manual {
        fexec(|| rc(&[c"rc", c"-c", c"/rc/bin/cpurc"]));
    }
    loop {
        r::println!("\ninit: starting /bin/rc");
        fexec(|| rcexec(&init));
        init.manual = true;
        init.cmd = None;
        let _ = r::sleep(1000);
    }
}

/// Execs rc with `argv`; returns only if that fails.
fn rc(argv: &[&CStr]) {
    let _ = r::exec(c"/bin/rc", argv);
}

fn rcexec(init: &Init) {
    if let Some(cmd) = &init.cmd {
        if let Ok(cmd) = CString::new(cmd.as_slice()) {
            rc(&[c"rc", c"-c", &cmd]);
        }
    } else if init.manual || init.iscpu {
        rc(&[c"rc"]);
    } else if init.service == b"terminal" {
        rc(&[c"rc", c"-c", c". /rc/bin/termrc; home=/usr/$user; cd; . lib/profile"]);
    } else {
        rc(&[c"rc"]);
    }
}

static GOTNOTE: AtomicBool = AtomicBool::new(false);

extern "C" fn pinhead(_: *mut r::Ureg, note: *const u8) {
    GOTNOTE.store(true, Ordering::Relaxed);
    // SAFETY: the kernel passes the note's NUL-terminated text.
    let m = unsafe { CStr::from_ptr(note.cast()) }.to_bytes();
    r::eprintln!("init got note '{}'", Text(m));
    let _ = r::noted(NoteHow::Cont);
}

/// Runs `execfn` in a child (Plan 9's `fork`: the name space and the
/// environment shared, the descriptors copied) with a note group of its
/// own, and waits for it, carrying on through notes.
fn fexec(execfn: impl FnOnce()) {
    let pid = match r::rfork(F::FDG | F::REND | F::PROC) {
        Ok(0) => {
            let _ = r::rfork(F::NOTEG);
            execfn();
            r::println!("init: exec error: {Rerr}");
            r::exits(Some(b"exec"))
        }
        Err(_) => {
            r::println!("init: fork error: {Rerr}");
            r::exits(Some(b"fork"))
        }
        Ok(pid) => pid,
    };
    let _ = r::notify(Some(pinhead));
    loop {
        GOTNOTE.store(false, Ordering::Relaxed);
        let w = match r::wait() {
            Ok(w) => w,
            Err(_) if GOTNOTE.load(Ordering::Relaxed) => continue,
            Err(_) => {
                r::println!("init: wait error: {Rerr}");
                return;
            }
        };
        if w.pid != pid {
            continue;
        }
        if w.msg.windows(10).any(|s| s == b"exec error") {
            r::println!("init: exit string {}", Text(&w.msg));
            r::println!("init: sleeping because exec failed");
            loop {
                let _ = r::sleep(1000);
            }
        }
        if !w.msg.is_empty() {
            r::println!("init: rc exit status: {}", Text(&w.msg));
        }
        return;
    }
}

/// The contents of `name`, which may be a device file whose length is 0;
/// `*unknown*` if it cannot be read.
fn readenv(name: &CStr) -> Vec<u8> {
    let unknown = || b"*unknown*".to_vec();
    let Ok(f) = r::open(name, mode(Access::Read)) else {
        r::println!("init: can't open {}: {Rerr}", Text(name.to_bytes()));
        return unknown();
    };
    let Ok(d) = r::dirfstat(f) else {
        r::println!("init: can't stat {}: {Rerr}", Text(name.to_bytes()));
        return unknown();
    };
    let len = d.as_dir().length;
    // Device files can be zero length but have contents.
    let len = if len == 0 {
        64
    } else {
        usize::try_from(len).unwrap_or(64)
    };
    let mut val = alloc::vec![0u8; len];
    let n = r::read(f, &mut val);
    let _ = r::close(f);
    let Ok(n) = n else {
        r::println!("init: can't read {}: {Rerr}", Text(name.to_bytes()));
        return unknown();
    };
    val.truncate(n);
    val
}

fn setenv(var: &CStr, val: &[u8]) {
    match r::create(var, mode(Access::Write), 0o644) {
        Err(_) => r::println!("init: can't open {}", Text(var.to_bytes())),
        Ok(fd) => {
            let _ = r::write(fd, val);
            let _ = r::close(fd);
        }
    }
}

fn cpenv(file: &CStr, var: &CStr) {
    let Ok(fd) = r::open(file, mode(Access::Read)) else {
        r::println!("init: can't open {}", Text(file.to_bytes()));
        return;
    };
    let mut buf = alloc::vec![0u8; 8192];
    match r::read(fd, &mut buf[..8191]) {
        Ok(i) if i > 0 => {
            let _ = r::close(fd);
            setenv(var, &buf[..i]);
        }
        _ => r::println!("init: can't read {}: {Rerr}", Text(file.to_bytes())),
    }
}

/// Cleans up after boot.
fn closefds() {
    for fd in 3..30 {
        let _ = r::close(fd);
    }
}
