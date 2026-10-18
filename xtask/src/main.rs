//! `cargo xtask`: the Plan R host build tool.
//!
//! Host zone (ADR 0006): may use `std` and host programs (cargo, QEMU, the
//! firmware images), but no crates.io dependencies, and nothing here ships.

mod acpidump;
mod arch;
mod crc32;
mod demangle;
mod deps;
mod fat;
mod gpt;
mod image;
mod json;
mod ksyms;
mod qemu;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use arch::Arch;
use qemu::Expect;

const USAGE: &str = "\
usage: cargo xtask <command> [options]

commands:
  build    build 9boot and the kernel for the target arch(es)
  image    build, then write the bootable GPT disk image
  qemu     build, write the image, and boot it interactively in QEMU
  test     host unit tests, then headless QEMU boot tests
  deps     check the dependency rule (ADR 0001)
  acpidump boot QEMU and write its ACPI tables to oracle/acpi-corpus
  ci       everything CI runs: fmt, clippy, deps, test

options:
  --arch amd64|arm64|all   target architecture (default: all; qemu: amd64)
  --release                optimized build
  --gui                    qemu: show a display window
  --timeout SECS           test: per-boot timeout (default 120)
  --all-profiles           test: boot every QEMU profile (ci does this)
  --config FILE            image, qemu: use FILE as plan9.ini (e.g.
                           scripts/hwcheck.ini for the boards)
";

#[derive(Debug, Clone)]
struct Opts {
    arches: Vec<Arch>,
    release: bool,
    gui: bool,
    timeout: u64,
    /// Boot every QEMU profile, not just each arch's first.
    all_profiles: bool,
    /// image, qemu: the `plan9.ini` to put on the image instead of [`CONFIG`].
    config: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let Some((cmd, rest)) = args.split_first() else {
        print!("{USAGE}");
        return Ok(());
    };
    let opts = parse_opts(rest, cmd == "qemu")?;
    match cmd.as_str() {
        "build" => build(&opts).map(drop),
        "image" => images(&opts).map(drop),
        "qemu" => {
            let [arch] = opts.arches[..] else {
                return Err("qemu boots one arch at a time; pass --arch amd64 or --arch arm64".into());
            };
            let single = Opts {
                arches: vec![arch],
                ..opts.clone()
            };
            let img = images(&single)?.remove(0);
            qemu::run_interactive(arch, &img, opts.gui)
        }
        "test" => test(&opts),
        "deps" => deps::check(&root()),
        "acpidump" => acpidump::run(&opts),
        "ci" => ci(&opts),
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            Ok(())
        }
        _ => Err(format!("unknown command {cmd:?}\n{USAGE}")),
    }
}

fn parse_opts(args: &[String], single_default: bool) -> Result<Opts, String> {
    let mut opts = Opts {
        arches: if single_default {
            vec![Arch::Amd64]
        } else {
            Arch::ALL.to_vec()
        },
        release: false,
        gui: false,
        timeout: 120,
        all_profiles: false,
        config: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--arch" => {
                let v = it.next().ok_or("--arch needs a value")?;
                opts.arches = if v == "all" {
                    Arch::ALL.to_vec()
                } else {
                    vec![v.parse()?]
                };
            }
            "--release" => opts.release = true,
            "--gui" => opts.gui = true,
            "--all-profiles" => opts.all_profiles = true,
            "--config" => opts.config = Some(it.next().ok_or("--config needs a file")?.into()),
            "--timeout" => {
                let v = it.next().ok_or("--timeout needs a value")?;
                opts.timeout = v.parse().map_err(|_| format!("bad --timeout {v:?}"))?;
            }
            _ => return Err(format!("unknown option {a:?}")),
        }
    }
    Ok(opts)
}

/// The workspace root.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .to_path_buf()
}

/// Output directory for one architecture's artifacts.
fn build_dir(arch: Arch) -> PathBuf {
    root().join("build").join(arch.name())
}

fn cargo() -> Command {
    let mut c = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    c.current_dir(root());
    c
}

/// Runs a command, failing with its description if it does not succeed.
fn sh(mut cmd: Command) -> Result<(), String> {
    let desc = format!("{cmd:?}");
    let status = cmd.status().map_err(|e| format!("{desc}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{desc}: {status}"))
    }
}

/// Built artifacts for one architecture.
#[derive(Debug, Clone)]
struct Built {
    arch: Arch,
    loader: PathBuf,
    kernel: PathBuf,
}

fn build(opts: &Opts) -> Result<Vec<Built>, String> {
    let profile = if opts.release { "release" } else { "debug" };
    let one = |package: &str, target: &str, file: &str| -> Result<PathBuf, String> {
        let mut c = cargo();
        c.args(["build", "-p", package, "--target", target]);
        if opts.release {
            c.arg("--release");
        }
        sh(c)?;
        Ok(root().join("target").join(target).join(profile).join(file))
    };
    opts.arches
        .iter()
        .map(|&arch| {
            let loader = one("nineboot", arch.uefi_target(), "9boot.efi")?;
            let linked = one("kernel", arch.kernel_target(), "kernel")?;
            let kernel = build_dir(arch).join("kernel");
            fs::create_dir_all(build_dir(arch)).map_err(|e| e.to_string())?;
            ksyms::embed(arch, &linked, &kernel)?;
            Ok(Built { arch, loader, kernel })
        })
        .collect()
}

/// Default boot configuration for interactive images.
const CONFIG: &str = "# Plan R boot configuration (see kernel/src/port/config.rs)\n";

/// A headless QEMU boot test: an image with its own `plan9.ini`, and what
/// the console must show.
#[derive(Debug)]
struct BootTest {
    name: &'static str,
    config: &'static str,
    expect: Expect,
    /// Boot every profile under `--all-profiles`, not just the first.
    every_profile: bool,
    /// Only for this architecture (the test exercises arch-specific code).
    only: Option<Arch>,
    machine: qemu::Machine,
}

const FOUR_CPUS: qemu::Machine = qemu::Machine {
    cpus: qemu::CPUS,
    reboot: false,
};

const BOOT_TESTS: &[BootTest] = &[
    // The kernel runs its in-kernel tests and powers off.
    BootTest {
        name: "boot",
        config: "# Plan R test image\ntest=1\n",
        expect: Expect::Ok(&[]),
        every_profile: true,
        only: None,
        machine: FOUR_CPUS,
    },
    // One CPU: the SMP code must cope with having nothing to start.
    BootTest {
        name: "uniprocessor",
        config: "# Plan R test image\ntest=1\n",
        expect: Expect::Ok(&[]),
        every_profile: false,
        only: None,
        machine: qemu::Machine { cpus: 1, reboot: false },
    },
    // The amd64 fallbacks KVM would otherwise hide: xAPIC registers in
    // MMIO, and the one-shot count timer instead of TSC-deadline mode.
    BootTest {
        name: "legacy-apic",
        config: "# Plan R test image: xAPIC, one-shot timer\ntest=1\napic=xapic\napictimer=oneshot\n",
        expect: Expect::Ok(&[]),
        every_profile: false,
        only: Some(Arch::Amd64),
        machine: FOUR_CPUS,
    },
    // TSC calibration against the PIT, the fallback for machines with no
    // HPET (the in-kernel tests cross-check it against the HPET).
    BootTest {
        name: "pit-calibration",
        config: "# Plan R test image: TSC calibrated by the PIT\ntest=1\ntsc=pit\n",
        expect: Expect::Ok(&["clock: TSC", "(Pit)"]),
        every_profile: false,
        only: Some(Arch::Amd64),
        machine: FOUR_CPUS,
    },
    // A deliberate fatal page fault must produce a full trap report.
    BootTest {
        name: "trap",
        config: "# Plan R trap test image\ntest=trap\n",
        expect: Expect::Panic(&[
            "kernel: PANIC: trap:",
            "address 0xffff9000dead0000",
            "backtrace:",
            // Symbolized: #0 is the faulting read in boot_main.
            "kernel::boot_main+0x",
            "#1 0x",
        ]),
        every_profile: false,
        only: None,
        machine: FOUR_CPUS,
    },
    // Overflowing a kernel stack must produce a report, not a hang or a
    // reset: on an application processor, then on the boot CPU, whose
    // overflow stacks are found differently on arm64.
    BootTest {
        name: "overflow",
        config: "# Plan R stack overflow test image\ntest=overflow\n",
        expect: Expect::Panic(&[
            "ktest: overflowing CPU",
            "kernel stack overflow",
            "backtrace:",
            "overflow_stack::recurse+0x",
        ]),
        every_profile: false,
        only: None,
        machine: FOUR_CPUS,
    },
    BootTest {
        name: "overflow-boot-cpu",
        config: "# Plan R stack overflow test image\ntest=overflow\n",
        expect: Expect::Panic(&[
            "ktest: overflowing the boot CPU",
            "kernel stack overflow",
            "overflow_stack::recurse+0x",
        ]),
        every_profile: false,
        only: None,
        machine: qemu::Machine { cpus: 1, reboot: false },
    },
    // The table dump `cargo xtask acpidump` captures the corpus with.
    BootTest {
        name: "acpidump",
        config: "# Plan R ACPI dump test image\ntest=acpidump\n",
        expect: Expect::Ok(&["acpidump: table XSDT", "acpidump: table DSDT", "acpidump: end"]),
        every_profile: false,
        only: None,
        machine: FOUR_CPUS,
    },
    // The kernel resets warm; 9boot must find the log ring the first boot
    // wrote, save it to the boot volume, and show how it ended.
    BootTest {
        name: "log-ring",
        config: "# Plan R log ring test image\ntest=reset\n",
        expect: Expect::Ok(&[
            "9boot: BootNext = ",
            "9boot: saved boot 1's log",
            "9boot: it ends: ktest: warm reset to check the log ring",
            "log: ring at",
        ]),
        every_profile: false,
        only: None,
        machine: qemu::Machine { cpus: 1, reboot: true },
    },
];

fn images(opts: &Opts) -> Result<Vec<PathBuf>, String> {
    let config = match &opts.config {
        Some(p) => fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?,
        None => CONFIG.to_string(),
    };
    write_images(&build(opts)?, "planr.img", &config)
}

fn write_images(built: &[Built], name: &str, config: &str) -> Result<Vec<PathBuf>, String> {
    built
        .iter()
        .map(|b| {
            let out = build_dir(b.arch).join(name);
            let contents = image::Contents {
                loader: &b.loader,
                kernel: &b.kernel,
                config,
            };
            image::write(b.arch, &contents, &out)?;
            println!("xtask: wrote {}", out.display());
            Ok(out)
        })
        .collect()
}

/// Crates with host unit tests (everything except the bare-metal binaries).
const HOST_TESTED: &[&str] = &["acpi", "bootinfo", "elf", "fbtext", "ksyms", "logring", "uefi", "xtask"];

fn with_packages(c: &mut Command, packages: &[&str]) {
    for p in packages {
        c.args(["-p", p]);
    }
}

fn host_tests() -> Result<(), String> {
    let mut c = cargo();
    c.args(["test", "-q"]);
    with_packages(&mut c, HOST_TESTED);
    sh(c)
}

fn test(opts: &Opts) -> Result<(), String> {
    host_tests()?;
    let built = build(opts)?;
    let mut failed = Vec::new();
    for t in BOOT_TESTS {
        let built: Vec<Built> = built
            .iter()
            .filter(|b| t.only.is_none_or(|a| a == b.arch))
            .cloned()
            .collect();
        let imgs = write_images(&built, &format!("planr-{}.img", t.name), t.config)?;
        for (arch, img) in built.iter().map(|b| b.arch).zip(&imgs) {
            let profiles = if opts.all_profiles && t.every_profile {
                arch.profiles()
            } else {
                &arch.profiles()[..1]
            };
            for profile in profiles {
                let name = format!("{arch}/{} {}", profile.name, t.name);
                match qemu::run_test(arch, profile, t.name, img, t.machine, &t.expect, opts.timeout) {
                    Ok(()) => println!("xtask: {name} test: PASS"),
                    Err(e) => {
                        println!("xtask: {name} test: FAIL: {e}");
                        failed.push(name);
                    }
                }
            }
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("boot tests failed: {}", failed.join(", ")))
    }
}

fn ci(opts: &Opts) -> Result<(), String> {
    let mut fmt = cargo();
    fmt.args(["fmt", "--all", "--check"]);
    sh(fmt)?;

    let mut host = cargo();
    host.args(["clippy", "-q", "--all-targets"]);
    with_packages(&mut host, HOST_TESTED);
    host.args(["--", "-D", "warnings"]);
    sh(host)?;
    for arch in &opts.arches {
        for (package, target) in [("nineboot", arch.uefi_target()), ("kernel", arch.kernel_target())] {
            let mut c = cargo();
            c.args([
                "clippy", "-q", "-p", package, "--target", target, "--", "-D", "warnings",
            ]);
            sh(c)?;
        }
    }

    deps::check(&root())?;
    test(&Opts {
        all_profiles: true,
        ..opts.clone()
    })
}
