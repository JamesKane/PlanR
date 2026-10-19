//! `cargo xtask oracle`: the scripts in each corpus, `oracle/rc` (the
//! shell) and `oracle/cmd` (the commands), run on host plan9port and on
//! Plan R in QEMU, and what each wrote to standard output and error, and
//! whether it exited normally, must be the same (TESTING.md, "Userland
//! behaviour"). plan9port's rc gives only a Unix exit code, so the status
//! compared is whether it is empty; a script that cares about a status
//! string echoes it. A script is named by its corpus, `rc/words`.
//!
//! `oracle/expect` holds what plan9port cannot answer (the commands it
//! lacks, and Plan 9's own error strings): there, what each script should
//! write is written down beside it, in `NAME.out` and `NAME.err` (none if
//! missing), and `NAME.fail` is there if it should fail.
//!
//! Each script runs as `rc NAME.rc` in an empty directory of its own,
//! with standard input from `/dev/null`. On the host the environment is
//! only `PLAN9` and `PATH=.:$PLAN9/bin`, so that `$path` is like Plan 9's
//! `(. /bin)` and the commands are plan9port's; on Plan R, `toracle` runs
//! them (see `cmd/utest/src/bin/toracle.rs`, which also gives the
//! console format read here).
//!
//! Each corpus's `allow` lists its known differences, one to a line: the
//! script, the stream (`stdout`, `stderr` or `status`) and why. A listed
//! difference that has gone away is an error too, so the list stays true.
//! Both sides' results are kept in `build/oracle/`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::qemu::{self, Expect};
use crate::{Opts, root};

/// What one script did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Ran {
    out: Vec<u8>,
    err: Vec<u8>,
    /// Plan R's exit status after `rc pid: `, or plan9port's exit code;
    /// empty for a normal exit.
    status: String,
}

/// The streams compared.
const STREAMS: [&str; 3] = ["stdout", "stderr", "status"];

impl Ran {
    fn stream(&self, s: &str) -> &[u8] {
        match s {
            "stdout" => &self.out,
            "stderr" => &self.err,
            _ => {
                if self.status.is_empty() {
                    b"ok"
                } else {
                    b"failed"
                }
            }
        }
    }
}

/// The corpora: directories of `oracle/`.
const CORPORA: &[&str] = &["rc", "cmd", "expect"];

/// The corpus whose results are written down, not plan9port's.
const EXPECT: &str = "expect";

/// How long the oracle's boot may take at least, in seconds: the corpus
/// takes minutes on arm64 under emulation (the kernel allows init 600).
const ORACLE_TIMEOUT: u64 = 660;

/// How long a script may take on the host.
const HOST_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn run(opts: &Opts) -> Result<(), String> {
    let rc = plan9port()?;
    let dir = root().join("oracle");
    let mut scripts = Vec::new();
    let mut allow = Vec::new();
    for c in CORPORA {
        let these = corpus(&dir.join(c))?;
        let list = dir.join(c).join("allow");
        if list.exists() {
            allow.extend(parse_allow(&read(&list)?, c, &these)?);
        }
        scripts.extend(these.into_iter().map(|n| format!("{c}/{n}")));
    }
    let out = root().join("build/oracle");

    let mut host = BTreeMap::new();
    for name in &scripts {
        let ran = if name.starts_with(&format!("{EXPECT}/")) {
            expected(&dir, name)?
        } else {
            run_host(&rc, &dir, name, &out.join("host"))?
        };
        save(&out.join("host"), name, &ran)?;
        host.insert(name.clone(), ran);
    }

    let built = crate::build(opts)?;
    let imgs = crate::write_images(&built, "planr-oracle.img", "# Plan R oracle image\ntest=oracle\n")?;
    // Which allowed differences were seen, on any arch.
    let mut used = vec![false; allow.len()];
    let mut failed = Vec::new();
    for (arch, img) in built.iter().map(|b| b.arch).zip(&imgs) {
        let profile = &arch.profiles()[0];
        let expect = Expect::Ok(&["oracle: done", crate::INIT_OK]);
        qemu::run_test(
            arch,
            profile,
            "oracle",
            img,
            crate::FOUR_CPUS,
            &expect,
            &[],
            opts.timeout.max(ORACLE_TIMEOUT),
        )
        .map_err(|e| format!("{arch} oracle boot: {e}"))?;
        let log = read(&qemu::log_path(arch, "oracle", profile))?;
        let planr = parse_log(&log)?;
        for name in &scripts {
            let Some(got) = planr.get(name) else {
                return Err(format!("{arch}: Plan R did not run oracle/{name}.rc"));
            };
            save(&out.join(arch.name()), name, got)?;
            let want = &host[name];
            let mut differs = Vec::new();
            for s in STREAMS {
                if want.stream(s) == got.stream(s) {
                    continue;
                }
                match allow.iter().position(|a| a.script == *name && a.stream == s) {
                    Some(i) => used[i] = true,
                    None => differs.push(s),
                }
            }
            if differs.is_empty() {
                println!("xtask: {arch} oracle {name}: PASS");
            } else {
                println!("xtask: {arch} oracle {name}: FAIL: {} differ", differs.join(", "));
                print!("{}", show(want, got, &differs));
                failed.push(format!("{arch} {name}"));
            }
        }
    }
    for (a, _) in allow.iter().zip(&used).filter(|(_, u)| !**u) {
        let corpus = a.script.split('/').next().unwrap_or_default();
        println!(
            "xtask: oracle {}: {} no longer differs; take it out of oracle/{corpus}/allow",
            a.script, a.stream
        );
        failed.push(format!("allow {} {}", a.script, a.stream));
    }
    println!(
        "xtask: oracle: {} scripts, {} allowed differences; results in {}",
        scripts.len(),
        allow.len(),
        out.display()
    );
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("oracle failed: {}", failed.join(", ")))
    }
}

/// plan9port's rc: under `$PLAN9`, else `~/plan9port`.
fn plan9port() -> Result<PathBuf, String> {
    let dir = match std::env::var_os("PLAN9") {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?).join("plan9port"),
    };
    let rc = dir.join("bin/rc");
    if rc.is_file() {
        Ok(rc)
    } else {
        Err(format!(
            "no plan9port rc at {}; build it with scripts/install-plan9port.sh",
            rc.display()
        ))
    }
}

/// The scripts' names, without `.rc`, sorted.
fn corpus(dir: &Path) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for e in fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let name = e.map_err(|e| e.to_string())?.file_name();
        if let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".rc")) {
            names.push(stem.to_string());
        }
    }
    names.sort();
    Ok(names)
}

/// The scripts, for the boot archive: their names there
/// (`sys/lib/oracle/CORPUS/NAME.rc`) and contents.
pub(crate) fn archive_files() -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    for c in CORPORA {
        let dir = root().join("oracle").join(c);
        for n in corpus(&dir)? {
            let data = fs::read(dir.join(format!("{n}.rc"))).map_err(|e| format!("{c}/{n}.rc: {e}"))?;
            files.push((format!("sys/lib/oracle/{c}/{n}.rc"), data));
        }
    }
    Ok(files)
}

fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// What script `name` in `dir` should do, as written down beside it.
fn expected(dir: &Path, name: &str) -> Result<Ran, String> {
    let file = |ext: &str| {
        let p = dir.join(format!("{name}.{ext}"));
        if p.exists() {
            fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))
        } else {
            Ok(Vec::new())
        }
    };
    Ok(Ran {
        out: file("out")?,
        err: file("err")?,
        status: if dir.join(format!("{name}.fail")).exists() {
            "failed".into()
        } else {
            String::new()
        },
    })
}

/// Runs script `name` (`corpus/stem`) from `dir` under plan9port's `rc`,
/// in a fresh directory under `work`.
fn run_host(rc: &Path, dir: &Path, name: &str, work: &Path) -> Result<Ran, String> {
    let here = work.join(name);
    if here.exists() {
        fs::remove_dir_all(&here).map_err(|e| format!("{}: {e}", here.display()))?;
    }
    fs::create_dir_all(&here).map_err(|e| format!("{}: {e}", here.display()))?;
    let stem = name.rsplit('/').next().unwrap_or(name);
    let script = format!("{stem}.rc");
    fs::copy(dir.join(format!("{name}.rc")), here.join(&script)).map_err(|e| format!("{name}.rc: {e}"))?;
    let (out, err) = (work.join(format!("{name}.stdout")), work.join(format!("{name}.stderr")));
    let file = |p: &Path| fs::File::create(p).map_err(|e| format!("{}: {e}", p.display()));
    let plan9 = rc.parent().and_then(Path::parent).ok_or("odd plan9port path")?;
    let mut child = Command::new(rc)
        .arg0("rc")
        .arg(&script)
        .current_dir(&here)
        .env_clear()
        .env("PLAN9", plan9)
        .env("PATH", format!(".:{}", plan9.join("bin").display()))
        .stdin(Stdio::null())
        .stdout(file(&out)?)
        .stderr(file(&err)?)
        .spawn()
        .map_err(|e| format!("{}: {e}", rc.display()))?;
    let deadline = Instant::now() + HOST_TIMEOUT;
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("plan9port: {script} took more than {HOST_TIMEOUT:?}"));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let bytes = |p: &Path| fs::read(p).map_err(|e| format!("{}: {e}", p.display()));
    Ok(Ran {
        out: bytes(&out)?,
        err: bytes(&err)?,
        status: match status.code() {
            Some(0) => String::new(),
            Some(c) => c.to_string(),
            None => status.to_string(),
        },
    })
}

/// Keeps what script `name` did in `dir`.
fn save(dir: &Path, name: &str, ran: &Ran) -> Result<(), String> {
    let parent = dir.join(name);
    let parent = parent.parent().unwrap_or(dir);
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    for (ext, data) in [
        ("stdout", &ran.out[..]),
        ("stderr", &ran.err),
        ("status", ran.status.as_bytes()),
    ] {
        let p = dir.join(format!("{name}.{ext}"));
        fs::write(&p, data).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    Ok(())
}

/// Reads `toracle`'s report from a serial log.
fn parse_log(log: &str) -> Result<BTreeMap<String, Ran>, String> {
    let mut all = BTreeMap::new();
    let mut cur: Option<(String, Ran)> = None;
    let mut done = None;
    for line in log.lines() {
        let Some(rest) = line.trim_end().strip_prefix("oracle: ") else {
            continue;
        };
        let (what, arg) = rest.split_once(' ').unwrap_or((rest, ""));
        match (what, &mut cur) {
            ("begin", None) => cur = Some((arg.to_string(), Ran::default())),
            ("out", Some((_, r))) => r.out.extend(unhex(arg)?),
            ("err", Some((_, r))) => r.err.extend(unhex(arg)?),
            ("status", Some((_, r))) => r.status.push_str(&String::from_utf8_lossy(&unhex(arg)?)),
            ("end", Some((name, _))) if name == arg => {
                let (name, r) = cur.take().expect("matched");
                all.insert(name, r);
            }
            ("done", None) => done = arg.parse::<usize>().ok(),
            _ => return Err(format!("oracle: unexpected console line {line:?}")),
        }
    }
    match (done, cur) {
        (_, Some((name, _))) => Err(format!("oracle: {name}.rc never finished")),
        (Some(n), None) if n == all.len() => Ok(all),
        _ => Err("oracle: toracle's report is incomplete".into()),
    }
}

fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err(format!("oracle: odd hex {s:?}"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| format!("oracle: bad hex {s:?}")))
        .collect()
}

/// One line of the allow-file.
#[derive(Debug, PartialEq, Eq)]
struct Allow {
    script: String,
    stream: &'static str,
}

/// Reads corpus `corpus`'s allow-file, which names its `scripts` without
/// the corpus.
fn parse_allow(text: &str, corpus: &str, scripts: &[String]) -> Result<Vec<Allow>, String> {
    let mut allow = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = |why: &str| format!("oracle/{corpus}/allow:{}: {why}", n + 1);
        let mut f = line.split_whitespace();
        let (Some(script), Some(stream), Some(_why)) = (f.next(), f.next(), f.next()) else {
            return Err(bad("want a script, a stream and why"));
        };
        if !scripts.iter().any(|s| s == script) {
            return Err(bad(&format!("no script {script}.rc")));
        }
        let Some(stream) = STREAMS.into_iter().find(|&s| s == stream) else {
            return Err(bad(&format!("{stream} is not one of {}", STREAMS.join(", "))));
        };
        let script = format!("{corpus}/{script}");
        if allow.iter().any(|a: &Allow| a.script == script && a.stream == stream) {
            return Err(bad("listed twice"));
        }
        allow.push(Allow { script, stream });
    }
    Ok(allow)
}

/// How `got` (Plan R) differs from `want` (plan9port) in `streams`.
fn show(want: &Ran, got: &Ran, streams: &[&str]) -> String {
    let mut s = String::new();
    for &st in streams {
        if st == "status" {
            let _ = writeln!(s, "  status: wanted {:?}, Plan R {:?}", want.status, got.status);
            continue;
        }
        let _ = writeln!(s, "  {st}, wanted:");
        lines(&mut s, want.stream(st));
        let _ = writeln!(s, "  {st}, Plan R:");
        lines(&mut s, got.stream(st));
    }
    s
}

/// `data` a line at a time, escaped, so that missing newlines show.
fn lines(s: &mut String, data: &[u8]) {
    for l in data.split_inclusive(|&b| b == b'\n') {
        let _ = writeln!(s, "    {}", l.escape_ascii());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_report() {
        let log = "kernel: boot\noracle: begin a\noracle: out 6869\noracle: out 0a\noracle: end a\n\
                   oracle: begin b\noracle: err 78\r\noracle: status 666f6f\noracle: end b\noracle: done 2\n";
        let r = parse_log(log).unwrap();
        assert_eq!(r["a"].out, b"hi\n");
        assert_eq!(r["a"].status, "");
        assert_eq!(r["b"].err, b"x");
        assert_eq!(r["b"].status, "foo");
        assert!(parse_log("oracle: begin a\noracle: out 61\n").is_err());
        assert!(parse_log("oracle: begin a\noracle: end a\noracle: done 2\n").is_err());
        assert!(parse_log("oracle: begin a\noracle: out 6\noracle: end a\noracle: done 1\n").is_err());
    }

    #[test]
    fn reads_the_allow_file() {
        let scripts = ["a".to_string(), "b".to_string()];
        let a = parse_allow("# known\n\na stderr  messages differ\nb status x\n", "rc", &scripts).unwrap();
        assert_eq!(
            a,
            [
                Allow {
                    script: "rc/a".into(),
                    stream: "stderr"
                },
                Allow {
                    script: "rc/b".into(),
                    stream: "status"
                }
            ]
        );
        assert!(parse_allow("c stdout why\n", "rc", &scripts).is_err());
        assert!(parse_allow("a stdin why\n", "rc", &scripts).is_err());
        assert!(parse_allow("a stdout\n", "rc", &scripts).is_err());
        assert!(parse_allow("a stdout x\na stdout y\n", "rc", &scripts).is_err());
    }

    #[test]
    fn statuses_compare_as_ok_or_not() {
        let a = Ran {
            status: "1".into(),
            ..Ran::default()
        };
        let b = Ran {
            status: "no match".into(),
            ..Ran::default()
        };
        assert_eq!(a.stream("status"), b.stream("status"));
        assert_ne!(a.stream("status"), Ran::default().stream("status"));
    }
}
