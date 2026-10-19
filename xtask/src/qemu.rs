//! Running images under QEMU, interactively or as a headless boot test.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::arch::{Arch, Profile};

/// Must match the markers in `boot/9boot/src/main.rs` and `kernel/src/main.rs`.
const OK_MARKER: &str = "kernel: ok";
/// What the kernel prints when `halt` is written to `/dev/reboot`.
const HALT_MARKER: &str = "kernel: halt";
const KERNEL_PANIC: &str = "kernel: PANIC";
const LOADER_FAIL_MARKERS: &[&str] = &["9boot: PANIC", "9boot: error"];

/// What a boot test must see.
#[derive(Debug)]
pub(crate) enum Expect {
    /// The OK marker, a line containing each of these strings, no failure
    /// marker, and a clean power-off.
    Ok(&'static [&'static str]),
    /// A kernel panic whose report contains every one of these strings,
    /// then the failure power-off.
    Panic(&'static [&'static str]),
    /// As `Ok`, but the machine is halted through `/dev/reboot`
    /// ([`HALT_MARKER`]) in place of the OK marker: a real boot, whose init
    /// never exits.
    Halt(&'static [&'static str]),
}
const BANNER: &str = "Plan R 9boot";

/// Input for a boot test: once a console line contains `after`, `send` is
/// typed on the serial line.
#[derive(Debug)]
pub(crate) struct Typed {
    pub(crate) after: &'static str,
    pub(crate) send: &'static [u8],
}

/// CPUs QEMU gives the machine unless a test says otherwise.
pub(crate) const CPUS: u32 = 4;

/// The virtual machine beyond the profile.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Machine {
    pub(crate) cpus: u32,
    /// Let the guest reset itself; otherwise a reset exits QEMU.
    pub(crate) reboot: bool,
}

fn command(arch: Arch, profile: &Profile, img: &Path, m: Machine) -> Result<Command, String> {
    let fw = arch.firmware()?;
    let dir = crate::build_dir(arch);
    let vars = dir.join("vars.fd");
    std::fs::copy(&fw.vars, &vars).map_err(|e| format!("copying {}: {e}", fw.vars.display()))?;

    let mut c = Command::new(arch.qemu());
    c.args([
        "-machine",
        profile.machine,
        "-cpu",
        profile.cpu,
        "-smp",
        &m.cpus.to_string(),
        "-m",
        "2G",
    ]);
    c.args(arch.device_args()).args(arch.accel_args());
    c.arg("-drive")
        .arg(format!("if=pflash,format=raw,readonly=on,file={}", fw.code.display()));
    c.arg("-drive")
        .arg(format!("if=pflash,format=raw,file={}", vars.display()));
    c.arg("-drive")
        .arg(format!("if=none,id=disk,format=raw,file={}", img.display()));
    c.args(["-device", "virtio-blk-pci,drive=disk"]);
    if !m.reboot {
        c.arg("-no-reboot");
    }
    Ok(c)
}

pub(crate) fn run_interactive(arch: Arch, img: &Path, gui: bool) -> Result<(), String> {
    let m = Machine {
        cpus: CPUS,
        reboot: true,
    };
    let mut c = command(arch, &arch.profiles()[0], img, m)?;
    // mon:stdio multiplexes the monitor onto the console: Ctrl-A X quits.
    c.args(["-serial", "mon:stdio"]);
    if !gui {
        c.args(["-display", "none"]);
    }
    crate::sh(c)
}

/// Strips ANSI/VT100 escape sequences and carriage returns from firmware
/// console output.
fn strip_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\x1b' => match it.next() {
                Some('[') => {
                    // CSI: parameters, then one final byte in @..~.
                    for c in it.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // Character-set designation: ESC ( B, ESC ) 0, …
                Some('(' | ')' | '*' | '+') => {
                    it.next();
                }
                _ => {} // other two-character escapes
            },
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// Where [`run_test`] keeps the serial log of `test` on `profile`.
pub(crate) fn log_path(arch: Arch, test: &str, profile: &Profile) -> PathBuf {
    crate::build_dir(arch).join(format!("serial-{test}-{}.log", profile.name))
}

/// Lines not echoed to the terminal: table dump bytes, and `toracle`'s
/// report (`xtask oracle` says how each script went).
const QUIET: &[&str] = &["acpidump: = ", "oracle: "];

/// The ways in to a test that hangs: `PLANR_MONITOR=path` puts QEMU's
/// monitor on a Unix socket (`info registers -a` shows every CPU without
/// stopping the guest), and `PLANR_GDB=port` starts a gdb stub.
fn debug_hooks(c: &mut Command) {
    match std::env::var("PLANR_MONITOR") {
        Ok(path) => c.args(["-monitor", &format!("unix:{path},server,nowait")]),
        Err(_) => c.args(["-monitor", "none"]),
    };
    if let Ok(port) = std::env::var("PLANR_GDB") {
        c.args(["-gdb", &format!("tcp:127.0.0.1:{port}")]);
    }
}

/// Boots `img` headless, types `input` as the console asks for it, and
/// checks the console and QEMU's exit status against `expect`, within
/// `timeout_secs`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_test(
    arch: Arch,
    profile: &Profile,
    test: &str,
    img: &Path,
    m: Machine,
    expect: &Expect,
    input: &[Typed],
    timeout_secs: u64,
) -> Result<(), String> {
    let mut c = command(arch, profile, img, m)?;
    c.args(["-display", "none", "-serial", "stdio"]);
    debug_hooks(&mut c);
    let stdin = if input.is_empty() {
        Stdio::null()
    } else {
        Stdio::piped()
    };
    c.stdin(stdin).stdout(Stdio::piped()).stderr(Stdio::inherit());
    let mut child = c.spawn().map_err(|e| format!("{}: {e}", arch.qemu()))?;
    let mut keyboard = child.stdin.take();
    let mut typed = 0;

    let stdout = child.stdout.take().expect("piped");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).split(b'\n') {
            let Ok(line) = line else { break };
            if tx.send(String::from_utf8_lossy(&line).into_owned()).is_err() {
                break;
            }
        }
    });

    let log_path = log_path(arch, test, profile);
    let mut log = std::fs::File::create(&log_path).map_err(|e| format!("{}: {e}", log_path.display()))?;
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let (mut ok, mut failed, mut panicked, mut echo) = (false, None, false, false);
    let needles: &[&str] = match expect {
        Expect::Ok(n) | Expect::Panic(n) | Expect::Halt(n) => n,
    };
    let mut seen = vec![false; needles.len()];
    let outcome = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(raw) => {
                let line = strip_escapes(&raw);
                let _ = writeln!(log, "{line}");
                echo |= line.contains(BANNER);
                if echo && !line.trim().is_empty() && !QUIET.iter().any(|q| line.contains(q)) {
                    println!("[{arch}] {line}");
                }
                ok |= line.contains(if matches!(expect, Expect::Halt(_)) {
                    HALT_MARKER
                } else {
                    OK_MARKER
                });
                panicked |= line.contains(KERNEL_PANIC);
                for (s, n) in seen.iter_mut().zip(needles) {
                    *s |= line.contains(n);
                }
                if let (Some(t), Some(k)) = (input.get(typed), keyboard.as_mut())
                    && line.contains(t.after)
                {
                    typed += 1;
                    if k.write_all(t.send).and_then(|()| k.flush()).is_err() {
                        break Some(format!("cannot type {:?}", t.after));
                    }
                }
                let fail_marker = LOADER_FAIL_MARKERS.iter().any(|m| line.contains(m))
                    || (!matches!(expect, Expect::Panic(_)) && line.contains(KERNEL_PANIC));
                if failed.is_none() && fail_marker {
                    failed = Some(line.trim().to_string());
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break None,
            Err(mpsc::RecvTimeoutError::Timeout) => break Some(format!("timed out after {timeout_secs}s")),
        }
    };
    if outcome.is_some() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let fail = |why: String| Err(format!("{why} (serial log: {})", log_path.display()));
    if let Some(why) = outcome.or(failed) {
        return fail(why);
    }
    if let Some(t) = input.get(typed) {
        return fail(format!("console never asked for input after {:?}", t.after));
    }
    let want_status = match expect {
        Expect::Ok(_) if !ok => return fail(format!("QEMU exited ({status}) without {OK_MARKER:?}")),
        Expect::Halt(_) if !ok => return fail(format!("QEMU exited ({status}) without {HALT_MARKER:?}")),
        Expect::Ok(_) | Expect::Halt(_) => {
            if let Some((n, _)) = needles.iter().zip(&seen).find(|(_, s)| !**s) {
                return fail(format!("console lacks {n:?}"));
            }
            arch.success_status()
        }
        Expect::Panic(_) if !panicked => return fail(format!("QEMU exited ({status}) without {KERNEL_PANIC:?}")),
        Expect::Panic(_) => {
            if let Some((n, _)) = needles.iter().zip(&seen).find(|(_, s)| !**s) {
                return fail(format!("panic report lacks {n:?}"));
            }
            arch.failure_status()
        }
    };
    if status.code() == Some(want_status) {
        Ok(())
    } else {
        fail(format!("QEMU exited with {status}, expected {want_status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::strip_escapes;

    #[test]
    fn strips_csi_and_cr() {
        assert_eq!(strip_escapes("\x1b[0m\x1b[2J\x1b[01;01HPlan R\r"), "Plan R");
        assert_eq!(strip_escapes("a\x1b(Bb"), "ab");
        assert_eq!(strip_escapes("plain"), "plain");
    }
}
