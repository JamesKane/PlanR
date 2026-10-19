//! `cargo xtask acpidump`: boots each arch's first QEMU profile with
//! `test=acpidump` and writes the ACPI tables the kernel prints into
//! `oracle/acpi-corpus/qemu-<machine>/`.
//!
//! Each table becomes `<NAME>.dat`, raw. Names follow Linux's
//! `/sys/firmware/acpi/tables`: the signature, with a number from 1
//! appended when a signature repeats (`SSDT1`, `SSDT2`). So a board dump
//! copied from sysfs has the same layout. QEMU dumps add `RSDP.dat`,
//! `XSDT.dat` and a `tables.txt` giving each table's physical address,
//! so the corpus test can rebuild the exact memory image.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::arch::Arch;
use crate::qemu::{self, Expect};
use crate::{FOUR_CPUS, Opts};

const CONFIG: &str = "# Plan R ACPI dump image\ntest=acpidump\n";
const PREFIX: &str = "acpidump: ";

/// One table from the dump.
#[derive(Debug, PartialEq, Eq)]
struct Table {
    sig: String,
    phys: u64,
    bytes: Vec<u8>,
}

pub(crate) fn run(opts: &Opts) -> Result<(), String> {
    let built = crate::build(opts)?;
    let imgs = crate::write_images(&built, "planr-acpidump.img", CONFIG)?;
    for (b, img) in built.iter().zip(&imgs) {
        let profile = &b.arch.profiles()[0];
        let expect = Expect::Ok(&["acpidump: end"]);
        qemu::run_test(b.arch, profile, "acpidump", img, FOUR_CPUS, &expect, &[], opts.timeout)?;
        let log_path = qemu::log_path(b.arch, "acpidump", profile);
        let log = fs::read_to_string(&log_path).map_err(|e| format!("{}: {e}", log_path.display()))?;
        let tables = parse(&log)?;
        let machine = profile.machine.split(',').next().unwrap_or(profile.machine);
        let dir = crate::root().join("oracle/acpi-corpus").join(format!("qemu-{machine}"));
        let origin = format!(
            "{} -machine {} -cpu {} -smp {}, firmware {}",
            qemu_version(b.arch),
            profile.machine,
            profile.cpu,
            FOUR_CPUS.cpus,
            b.arch.firmware()?.code.display()
        );
        write(&dir, &tables, &origin)?;
        println!("xtask: wrote {} tables to {}", tables.len(), dir.display());
    }
    Ok(())
}

fn qemu_version(arch: Arch) -> String {
    Command::new(arch.qemu())
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).lines().next().map(str::to_string))
        .unwrap_or_else(|| arch.qemu().to_string())
}

/// The tables between the dump's `begin` and `end` lines.
fn parse(log: &str) -> Result<Vec<Table>, String> {
    let mut lines = log.lines().filter_map(|l| l.trim().strip_prefix(PREFIX));
    if !lines.any(|l| l == "begin") {
        return Err("no ACPI dump in the log".into());
    }
    let mut tables: Vec<(Table, usize)> = Vec::new();
    for l in lines {
        if l == "end" {
            return tables
                .into_iter()
                .map(|(t, len)| {
                    if t.bytes.len() == len {
                        Ok(t)
                    } else {
                        Err(format!("{}: {} bytes, expected {len}", t.sig, t.bytes.len()))
                    }
                })
                .collect();
        }
        if let Some(hex) = l.strip_prefix("= ") {
            let (t, _) = tables.last_mut().ok_or("hex before the first table")?;
            t.bytes.extend(decode_hex(hex)?);
        } else if let Some(header) = l.strip_prefix("table ") {
            let bad = || format!("bad table line {l:?}");
            let mut f = header.split(' ');
            let (Some(sig), Some(phys), Some(len), None) = (f.next(), f.next(), f.next(), f.next()) else {
                return Err(bad());
            };
            let phys = u64::from_str_radix(phys.trim_start_matches("0x"), 16).map_err(|_| bad())?;
            let len = len.parse().map_err(|_| bad())?;
            let table = Table {
                sig: sig.to_string(),
                phys,
                bytes: Vec::new(),
            };
            tables.push((table, len));
        } else {
            return Err(format!("the kernel could not dump a table: {l}"));
        }
    }
    Err("the ACPI dump has no end".into())
}

fn decode_hex(hex: &str) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err(format!("odd-length hex {hex:?}"));
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| format!("bad hex {hex:?}")))
        .collect()
}

/// Names each table as Linux's sysfs does, dropping repeats of one address
/// (a table both listed and pointed to by the FADT).
fn names(tables: &[Table]) -> Vec<(String, &Table)> {
    let mut seen = Vec::new();
    let unique: Vec<&Table> = tables
        .iter()
        .filter(|t| {
            let first = !seen.contains(&t.phys);
            seen.push(t.phys);
            first
        })
        .collect();
    let mut total: HashMap<&str, usize> = HashMap::new();
    for t in &unique {
        *total.entry(&t.sig).or_default() += 1;
    }
    let mut nth: HashMap<&str, usize> = HashMap::new();
    unique
        .into_iter()
        .map(|t| {
            let n = nth.entry(&t.sig).or_default();
            *n += 1;
            let name = if total[t.sig.as_str()] > 1 {
                format!("{}{n}", t.sig)
            } else {
                t.sig.clone()
            };
            (name, t)
        })
        .collect()
}

/// Replaces the `.dat` files and `tables.txt` in `dir`, leaving anything
/// else (a README) alone.
fn write(dir: &Path, tables: &[Table], origin: &str) -> Result<(), String> {
    let io = |e: std::io::Error| format!("{}: {e}", dir.display());
    fs::create_dir_all(dir).map_err(io)?;
    for e in fs::read_dir(dir).map_err(io)? {
        let p = e.map_err(io)?.path();
        if p.extension().is_some_and(|x| x == "dat") || p.file_name().is_some_and(|n| n == "tables.txt") {
            fs::remove_file(&p).map_err(io)?;
        }
    }
    let mut index = format!("# Captured by `cargo xtask acpidump` from {origin}.\n# name address length\n");
    for (name, t) in names(tables) {
        fs::write(dir.join(format!("{name}.dat")), &t.bytes).map_err(io)?;
        let _ = writeln!(index, "{name} {:#x} {}", t.phys, t.bytes.len());
    }
    fs::write(dir.join("tables.txt"), index).map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "\
noise
acpidump: begin
acpidump: table RSDP 0x7fbfa014 4
acpidump: = 52534420
acpidump: table SSDT 0x1000 2
acpidump: = abcd
acpidump: table APIC 0x2000 1
acpidump: = 01
  acpidump: table SSDT 0x3000 1
acpidump: = ff
acpidump: table APIC 0x2000 1
acpidump: = 01
acpidump: end
";

    #[test]
    fn parses_and_names_like_sysfs() {
        let tables = parse(LOG).unwrap();
        assert_eq!(tables.len(), 5);
        assert_eq!(tables[0].bytes, b"RSD ");
        assert_eq!(tables[1].phys, 0x1000);
        let names: Vec<_> = names(&tables).into_iter().map(|(n, t)| (n, t.phys)).collect();
        assert_eq!(
            names,
            [
                ("RSDP".to_string(), 0x7fbf_a014),
                ("SSDT1".to_string(), 0x1000),
                ("APIC".to_string(), 0x2000),
                ("SSDT2".to_string(), 0x3000),
            ]
        );
    }

    #[test]
    fn rejects_bad_dumps() {
        assert!(parse("acpidump: table X 0x0 1\n").is_err());
        assert!(parse("acpidump: begin\nacpidump: table APIC 0x0 2\nacpidump: = 01\nacpidump: end\n").is_err());
        assert!(parse("acpidump: begin\nacpidump: unreadable DSDT 0x0 9\nacpidump: end\n").is_err());
        assert!(parse("acpidump: begin\nacpidump: table APIC 0x0 1\nacpidump: = 0g\nacpidump: end\n").is_err());
        assert!(parse("acpidump: begin\nacpidump: table APIC 0x0 1\nacpidump: = 01\n").is_err());
    }
}
