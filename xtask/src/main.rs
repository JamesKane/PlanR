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
  build    build 9boot (and later the kernel) for the target arch(es)
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
";

#[derive(Debug, Clone)]
struct Opts {
    arches: Vec<Arch>,
    release: bool,
    gui: bool,
    timeout: u64,
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
            let img = images(&Opts {
                arches: vec![arch],
                ..opts.clone()
            })?
            .remove(0);
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
}

fn build(opts: &Opts) -> Result<Vec<Built>, String> {
    opts.arches
        .iter()
        .map(|&arch| {
            let mut c = cargo();
            c.args(["build", "-p", "nineboot", "--target", arch.uefi_target()]);
            if opts.release {
                c.arg("--release");
            }
            sh(c)?;
            let profile = if opts.release { "release" } else { "debug" };
            let loader = root()
                .join("target")
                .join(arch.uefi_target())
                .join(profile)
                .join("9boot.efi");
            Ok(Built { arch, loader })
        })
        .collect()
}

fn images(opts: &Opts) -> Result<Vec<PathBuf>, String> {
    build(opts)?
        .into_iter()
        .map(|b| {
            let out = build_dir(b.arch).join("planr.img");
            image::write(b.arch, &b.loader, &out)?;
            println!("xtask: wrote {}", out.display());
            Ok(out)
        })
        .collect()
}

fn host_tests() -> Result<(), String> {
    let mut c = cargo();
    c.args(["test", "-q", "-p", "uefi", "-p", "xtask"]);
    sh(c)
}

fn test(opts: &Opts) -> Result<(), String> {
    host_tests()?;
    let imgs = images(opts)?;
    let mut failed = Vec::new();
    for (&arch, img) in opts.arches.iter().zip(&imgs) {
        match qemu::run_test(arch, img, opts.timeout) {
            Ok(()) => println!("xtask: {} boot test: PASS", arch.name()),
            Err(e) => {
                println!("xtask: {} boot test: FAIL: {e}", arch.name());
                failed.push(arch.name());
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
    host.args([
        "clippy",
        "-q",
        "-p",
        "uefi",
        "-p",
        "xtask",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ]);
    sh(host)?;
    for arch in &opts.arches {
        let mut c = cargo();
        c.args([
            "clippy",
            "-q",
            "-p",
            "nineboot",
            "--target",
            arch.uefi_target(),
            "--",
            "-D",
            "warnings",
        ]);
        sh(c)?;
    }

    deps::check(&root())?;
    test(opts)
}
