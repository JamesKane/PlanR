//! Selects the per-architecture linker script. (Build scripts run on the
//! host; they are part of the build, not of Plan R.)

fn main() {
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "amd64",
        Ok("aarch64") => "arm64",
        _ => return,
    };
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo");
    println!("cargo:rustc-link-arg-bins=-T{dir}/src/arch/{arch}/link.ld");
    println!("cargo:rerun-if-changed=src/arch/{arch}/link.ld");
}
