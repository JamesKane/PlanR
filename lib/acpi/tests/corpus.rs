//! Parses every table set in `oracle/acpi-corpus` and compares what the
//! parser makes of it with the set's `summary.txt`. Run with
//! `UPDATE_CORPUS=1` to write the summaries after adding or changing a set
//! or the parser, then review the diff.
//!
//! A set captured by `cargo xtask acpidump` has a `tables.txt` giving each
//! table's physical address, and is replayed at those addresses. A set
//! copied from Linux's `/sys/firmware/acpi/tables` has only the tables, so
//! the test places them itself and builds an RSDP and XSDT listing them,
//! pointing the FADT at the DSDT and FACS.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use acpi::{Acpi, PhysMem, PpttNode, Signature};

/// Physical memory made of the captured tables.
struct Memory(Vec<(u64, Vec<u8>)>);

impl PhysMem for Memory {
    fn read(&self, phys: u64, len: usize) -> Option<&[u8]> {
        self.0.iter().find_map(|(base, bytes)| {
            let start = usize::try_from(phys.checked_sub(*base)?).ok()?;
            bytes.get(start..start.checked_add(len)?)
        })
    }
}

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../oracle/acpi-corpus")
}

fn sets() -> Vec<PathBuf> {
    let mut sets: Vec<_> = fs::read_dir(corpus())
        .expect("oracle/acpi-corpus")
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    sets.sort();
    sets
}

fn table(dir: &Path, name: &str) -> Vec<u8> {
    let path = dir.join(format!("{name}.dat"));
    fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The tables at their captured addresses, and the RSDP's.
fn replay(dir: &Path, index: &str) -> (Memory, u64) {
    let mut mem = Vec::new();
    let mut rsdp = None;
    for line in index.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
        let f: Vec<_> = line.split_whitespace().collect();
        let [name, addr, len] = f[..] else {
            panic!("{}: bad tables.txt line {line:?}", dir.display());
        };
        let addr = u64::from_str_radix(addr.trim_start_matches("0x"), 16).unwrap();
        let bytes = table(dir, name);
        assert_eq!(bytes.len().to_string(), len, "{name}.dat length");
        if name == "RSDP" {
            rsdp = Some(addr);
        }
        mem.push((addr, bytes));
    }
    (Memory(mem), rsdp.expect("tables.txt lists no RSDP"))
}

fn set_checksum(t: &mut [u8], at: usize) {
    t[at] = 0;
    let sum = t.iter().fold(0u8, |s, b| s.wrapping_add(*b));
    t[at] = sum.wrapping_neg();
}

/// The tables at addresses of our choosing, with an RSDP and XSDT built
/// to list them.
fn synthesize(dir: &Path) -> (Memory, u64) {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            (p.extension()? == "dat").then(|| p.file_stem().unwrap().to_string_lossy().into_owned())
        })
        .filter(|n| n != "RSDP" && n != "XSDT")
        .collect();
    names.sort();
    let addr = |i: usize| 0x10_0000 * (i as u64 + 1);
    let mut mem = Vec::new();
    let mut listed = Vec::new();
    let (mut dsdt, mut facs) = (None, None);
    for (i, name) in names.iter().enumerate() {
        match name.as_str() {
            "DSDT" => dsdt = Some(addr(i)),
            "FACS" => facs = Some(addr(i)),
            _ => listed.push(addr(i)),
        }
        mem.push((addr(i), table(dir, name)));
    }
    for (_, t) in &mut mem {
        if t.starts_with(b"FACP") {
            for (narrow, wide, target) in [(36, 132, facs), (40, 140, dsdt)] {
                t[narrow..narrow + 4].fill(0);
                if t.len() >= wide + 8 {
                    t[wide..wide + 8].copy_from_slice(&target.unwrap_or(0).to_le_bytes());
                } else if let Some(p) = target {
                    t[narrow..narrow + 4].copy_from_slice(&u32::try_from(p).unwrap().to_le_bytes());
                }
            }
            set_checksum(t, 9);
        }
    }
    let xsdt_at: u64 = 0x1000;
    let mut xsdt = b"XSDT".to_vec();
    xsdt.extend(u32::try_from(36 + 8 * listed.len()).unwrap().to_le_bytes());
    xsdt.extend([1, 0]);
    xsdt.extend(b"PLANR PLANRTST");
    xsdt.extend([0; 12]);
    for p in &listed {
        xsdt.extend(p.to_le_bytes());
    }
    set_checksum(&mut xsdt, 9);
    let mut rsdp = b"RSD PTR ".to_vec();
    rsdp.push(0);
    rsdp.extend(b"PLANR ");
    rsdp.push(2);
    rsdp.extend([0; 4]);
    rsdp.extend(36u32.to_le_bytes());
    rsdp.extend(xsdt_at.to_le_bytes());
    rsdp.extend([0; 4]);
    set_checksum(&mut rsdp[..20], 8);
    set_checksum(&mut rsdp, 32);
    mem.push((xsdt_at, xsdt));
    mem.push((0x800, rsdp));
    (Memory(mem), 0x800)
}

/// What the parser makes of a set, one fact per line.
fn summarize(acpi: &Acpi<'_, Memory>) -> String {
    let mut s = String::new();
    let r = &acpi.rsdp;
    writeln!(s, "rsdp: revision {}, OEM {:?}", r.revision, r.oem_id()).unwrap();
    for t in acpi.tables() {
        match t {
            Ok(t) => writeln!(
                s,
                "table: {} rev {} OEM {:?} {:?}",
                t.signature(),
                t.revision(),
                t.oem_id(),
                t.oem_table_id()
            ),
            Err(e) => writeln!(s, "table: error: {e}"),
        }
        .unwrap();
    }
    if let Some(madt) = acpi.madt() {
        writeln!(s, "madt: {} usable cpus", madt.cpu_count()).unwrap();
        for e in madt.entries() {
            writeln!(s, "madt: {e:?}").unwrap();
        }
    }
    if let Some(f) = acpi.fadt() {
        writeln!(s, "fadt: revision {}.{}", f.sdt().revision(), f.minor_version()).unwrap();
        writeln!(s, "fadt: dsdt {}, facs {}", present(f.dsdt()), present(f.facs())).unwrap();
        writeln!(s, "fadt: boot flags {:#x}, psci {:?}", f.iapc_boot_flags(), f.psci()).unwrap();
        writeln!(s, "fadt: reset {:?}", f.reset()).unwrap();
    }
    for (name, v) in [
        ("hpet", acpi.hpet().map(|v| format!("{v:?}"))),
        ("spcr", acpi.spcr().map(|v| format!("{v:?}"))),
        ("gtdt", acpi.gtdt().map(|v| format!("{v:?}"))),
    ] {
        if let Some(v) = v {
            writeln!(s, "{name}: {v}").unwrap();
        }
    }
    for d in acpi.dbg2() {
        writeln!(s, "dbg2: {d:?}").unwrap();
    }
    for e in acpi.mcfg() {
        writeln!(s, "mcfg: {e:?}").unwrap();
    }
    if let Some(p) = acpi.pptt() {
        for n in p.nodes() {
            writeln!(s, "pptt: {n:?}").unwrap();
            if let PpttNode::Processor(cpu) = n
                && cpu.flags.leaf()
            {
                let caches: Vec<_> = p.caches(cpu).map(|(level, c)| (level, c.kind)).collect();
                writeln!(s, "pptt:   caches {caches:?}").unwrap();
            }
        }
    }
    if let Some(t) = acpi.iort() {
        for n in t.nodes() {
            writeln!(
                s,
                "iort: node {} rev {} id {}: {:?}",
                n.offset, n.revision, n.identifier, n.kind
            )
            .unwrap();
            for m in n.mappings() {
                writeln!(s, "iort:   {m:?}").unwrap();
            }
        }
    }
    s
}

/// Whether a pointer is set: synthesized sets move the tables, so the
/// address itself is not a fact about the firmware.
fn present(p: Option<u64>) -> &'static str {
    if p.is_some() { "present" } else { "absent" }
}

#[test]
fn corpus_parses_as_recorded() {
    let update = std::env::var_os("UPDATE_CORPUS").is_some();
    let sets = sets();
    assert!(!sets.is_empty(), "the corpus is empty");
    let mut stale = Vec::new();
    for dir in sets {
        let (mem, rsdp) = match fs::read_to_string(dir.join("tables.txt")) {
            Ok(index) => replay(&dir, &index),
            Err(_) => synthesize(&dir),
        };
        let acpi = Acpi::new(&mem, rsdp).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        // A table the root lists but the set lacks would read as unmapped.
        for t in acpi.tables() {
            if let Err(e) = t {
                assert!(!matches!(e, acpi::Error::Unmapped(_)), "{}: {e}", dir.display());
            }
        }
        if acpi.fadt().is_some() {
            assert!(acpi.find(Signature::FADT).is_some());
        }
        let got = summarize(&acpi);
        let path = dir.join("summary.txt");
        if update {
            fs::write(&path, &got).unwrap();
        } else if fs::read_to_string(&path).ok().as_deref() != Some(got.as_str()) {
            stale.push(path.display().to_string());
        }
    }
    assert!(
        stale.is_empty(),
        "parse differs from the recorded summary (rerun with UPDATE_CORPUS=1 and review the diff): {}",
        stale.join(", ")
    );
}

/// The synthesized layout reads the same as the captured one, apart from
/// the RSDP and the root table's order, so sysfs copies are checked as
/// thoroughly as QEMU dumps.
#[test]
fn synthesized_layout_matches_captured() {
    let facts = |s: String| -> Vec<String> {
        s.lines()
            .filter(|l| !l.starts_with("rsdp:") && !l.starts_with("table:"))
            .map(str::to_string)
            .collect()
    };
    for dir in sets() {
        let Ok(index) = fs::read_to_string(dir.join("tables.txt")) else {
            continue;
        };
        let (captured, at) = replay(&dir, &index);
        let (synthesized, sat) = synthesize(&dir);
        let a = summarize(&Acpi::new(&captured, at).unwrap());
        let b = summarize(&Acpi::new(&synthesized, sat).unwrap());
        assert_eq!(facts(a), facts(b), "{}", dir.display());
    }
}
