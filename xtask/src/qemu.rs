//! Running images under QEMU, interactively or as a headless boot test.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::arch::{Arch, Profile};

/// Must match the markers in `boot/9boot/src/main.rs` and `kernel/src/main.rs`.
const OK_MARKER: &str = "kernel: ok";
const FAIL_MARKERS: &[&str] = &["9boot: PANIC", "9boot: error", "kernel: PANIC"];
const BANNER: &str = "Plan R 9boot";

fn command(arch: Arch, profile: &Profile, img: &Path) -> Result<Command, String> {
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
        "4",
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
    c.args(["-device", "virtio-blk-pci,drive=disk", "-no-reboot"]);
    Ok(c)
}

pub(crate) fn run_interactive(arch: Arch, img: &Path, gui: bool) -> Result<(), String> {
    let mut c = command(arch, &arch.profiles()[0], img)?;
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

/// Boots `img` headless and passes if the loader prints the OK marker and
/// QEMU exits cleanly within `timeout_secs`.
pub(crate) fn run_test(arch: Arch, profile: &Profile, img: &Path, timeout_secs: u64) -> Result<(), String> {
    let mut c = command(arch, profile, img)?;
    c.args(["-display", "none", "-serial", "stdio", "-monitor", "none"]);
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::inherit());
    let mut child = c.spawn().map_err(|e| format!("{}: {e}", arch.qemu()))?;

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

    let log_path = crate::build_dir(arch).join(format!("serial-{}.log", profile.name));
    let mut log = std::fs::File::create(&log_path).map_err(|e| format!("{}: {e}", log_path.display()))?;
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let (mut ok, mut failed, mut echo) = (false, None, false);
    let outcome = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(raw) => {
                let line = strip_escapes(&raw);
                let _ = writeln!(log, "{line}");
                echo |= line.contains(BANNER);
                if echo && !line.trim().is_empty() {
                    println!("[{arch}] {line}");
                }
                ok |= line.contains(OK_MARKER);
                if failed.is_none() && FAIL_MARKERS.iter().any(|m| line.contains(m)) {
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
    match outcome {
        Some(why) => fail(why),
        None if failed.is_some() => fail(failed.unwrap_or_default()),
        None if !ok => fail(format!("QEMU exited ({status}) without {OK_MARKER:?}")),
        None if status.code() != Some(arch.success_status()) => {
            fail(format!("QEMU exited with {status}, expected {}", arch.success_status()))
        }
        None => Ok(()),
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
