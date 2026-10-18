//! `cargo xtask`: the Plan R host build tool.
//!
//! Host zone (ADR 0006): may use `std` and host programs (cargo, QEMU, the
//! firmware images), but no crates.io dependencies, and nothing here ships.

mod arch;
mod crc32;
mod deps;
mod fat;
mod gpt;
mod image;
mod json;
mod qemu;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use arch::Arch;

const USAGE: &str = "\
usage: cargo xtask <command> [options]

commands:
  build    build 9boot and the kernel for the target arch(es)
  image    build, then write the bootable GPT disk image
  qemu     build, write the image, and boot it interactively in QEMU
  test     host unit tests, then headless QEMU boot tests
  deps     check the dependency rule (ADR 0001)
  ci       everything CI runs: fmt, clippy, deps, test

options:
  --arch amd64|arm64|all   target architecture (default: all; qemu: amd64)
  --release                optimized build
  --gui                    qemu: show a display window
  --timeout SECS           test: per-boot timeout (default 120)
  --all-profiles           test: boot every QEMU profile (ci does this)
";

#[derive(Debug, Clone)]
struct Opts {
    arches: Vec<Arch>,
    release: bool,
    gui: bool,
    timeout: u64,
    /// Boot every QEMU profile, not just each arch's first.
    all_profiles: bool,
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
        "image" => images(&opts, false).map(drop),
        "qemu" => {
            let [arch] = opts.arches[..] else {
                return Err("qemu boots one arch at a time; pass --arch amd64 or --arch arm64".into());
            };
            let single = Opts {
                arches: vec![arch],
                ..opts.clone()
            };
            let img = images(&single, false)?.remove(0);
            qemu::run_interactive(arch, &img, opts.gui)
        }
        "test" => test(&opts),
        "deps" => deps::check(&root()),
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
#[derive(Debug)]
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
            Ok(Built {
                arch,
                loader: one("nineboot", arch.uefi_target(), "9boot.efi")?,
                kernel: one("kernel", arch.kernel_target(), "kernel")?,
            })
        })
        .collect()
}

/// Default boot configuration for interactive images.
const CONFIG: &str = "# Plan R boot configuration (see kernel/src/port/config.rs)\n";
/// Test images: the kernel runs its checks and powers off.
const TEST_CONFIG: &str = "# Plan R test image\ntest=1\n";

fn images(opts: &Opts, test: bool) -> Result<Vec<PathBuf>, String> {
    let (name, config) = if test {
        ("planr-test.img", TEST_CONFIG)
    } else {
        ("planr.img", CONFIG)
    };
    build(opts)?
        .into_iter()
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
const HOST_TESTED: &[&str] = &["acpi", "bootinfo", "elf", "uefi", "xtask"];

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
    let imgs = images(opts, true)?;
    let mut failed = Vec::new();
    for (&arch, img) in opts.arches.iter().zip(&imgs) {
        let profiles = if opts.all_profiles {
            arch.profiles()
        } else {
            &arch.profiles()[..1]
        };
        for profile in profiles {
            let name = format!("{arch}/{}", profile.name);
            match qemu::run_test(arch, profile, img, opts.timeout) {
                Ok(()) => println!("xtask: {name} boot test: PASS"),
                Err(e) => {
                    println!("xtask: {name} boot test: FAIL: {e}");
                    failed.push(name);
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
