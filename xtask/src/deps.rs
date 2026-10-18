//! Enforces the dependency rule (ADR 0001, ADR 0006).
//!
//! 1. Every package in the build graph is an in-tree path crate: no
//!    registry, git, or out-of-tree path dependencies anywhere, host zone
//!    included.
//! 2. Every crate that ships in Plan R is `no_std` (it may opt into `std`
//!    only under `cfg(test)` for host unit tests), and does not depend on
//!    host-zone crates.

use std::path::Path;
use std::process::Command;

use crate::json::{self, Value};

/// Host-zone packages: run on the development machine, never ship.
const HOST_ZONE: &[&str] = &["xtask"];

pub(crate) fn check(root: &Path) -> Result<(), String> {
    let out = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .current_dir(root)
        .args(["metadata", "--format-version", "1", "--locked", "--offline"])
        .output()
        .map_err(|e| format!("cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let meta = json::parse(&String::from_utf8_lossy(&out.stdout))?;
    let problems = violations(&meta, root);
    if problems.is_empty() {
        let n = meta.get("packages").map_or(0, |p| p.as_array().len());
        println!("xtask: dependency rule holds ({n} packages, all in-tree)");
        Ok(())
    } else {
        Err(format!("dependency rule violated:\n  {}", problems.join("\n  ")))
    }
}

fn violations(meta: &Value, root: &Path) -> Vec<String> {
    let mut bad = Vec::new();
    let packages = meta.get("packages").map_or(&[][..], Value::as_array);
    for p in packages {
        let name = p.get("name").and_then(Value::as_str).unwrap_or("?");
        if let Some(src) = p.get("source").and_then(Value::as_str) {
            bad.push(format!("{name}: external source {src}"));
        }
        let manifest = p.get("manifest_path").and_then(Value::as_str).unwrap_or("");
        if !Path::new(manifest).starts_with(root) {
            bad.push(format!("{name}: lives outside the tree ({manifest})"));
        }
        if HOST_ZONE.contains(&name) {
            continue;
        }
        for d in p.get("dependencies").map_or(&[][..], Value::as_array) {
            let dep = d.get("name").and_then(Value::as_str).unwrap_or("?");
            if HOST_ZONE.contains(&dep) {
                bad.push(format!("{name}: ships but depends on host-zone crate {dep}"));
            }
        }
        for t in p.get("targets").map_or(&[][..], Value::as_array) {
            let kinds = t.get("kind").map_or(&[][..], Value::as_array);
            if !kinds.iter().any(|k| matches!(k.as_str(), Some("lib" | "bin"))) {
                continue;
            }
            let src = t.get("src_path").and_then(Value::as_str).unwrap_or("");
            match std::fs::read_to_string(src) {
                Ok(text) if declares_no_std(&text) => {}
                Ok(_) => bad.push(format!("{name}: {src} is not #![no_std]")),
                Err(e) => bad.push(format!("{name}: {src}: {e}")),
            }
        }
    }
    bad
}

/// True if the crate root has `#![no_std]`, possibly as
/// `#![cfg_attr(not(test), no_std)]`.
fn declares_no_std(src: &str) -> bool {
    src.lines().map(str::trim).any(|l| {
        let l: String = l.chars().filter(|c| !c.is_whitespace()).collect();
        l == "#![no_std]" || l == "#![cfg_attr(not(test),no_std)]"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(pkgs: &str) -> Value {
        json::parse(&format!(r#"{{"packages": [{pkgs}]}}"#)).unwrap()
    }

    #[test]
    fn flags_registry_and_out_of_tree() {
        let m = meta(
            r#"{"name":"serde","source":"registry+https://github.com/rust-lang/crates.io-index",
                "manifest_path":"/home/x/.cargo/registry/serde/Cargo.toml","dependencies":[],"targets":[]}"#,
        );
        let v = violations(&m, Path::new("/src/planr"));
        assert_eq!(v.len(), 2, "{v:?}");
    }

    #[test]
    fn flags_shipping_crate_on_host_zone() {
        let m = meta(
            r#"{"name":"k","source":null,"manifest_path":"/src/planr/k/Cargo.toml",
                "dependencies":[{"name":"xtask"}],"targets":[]}"#,
        );
        assert_eq!(violations(&m, Path::new("/src/planr")).len(), 1);
    }

    #[test]
    fn no_std_detection() {
        assert!(declares_no_std("//! doc\n#![no_std]\n"));
        assert!(declares_no_std("#![cfg_attr(not(test), no_std)]"));
        assert!(!declares_no_std("// #![no_std] is just a comment here\nfn main() {}"));
    }
}
