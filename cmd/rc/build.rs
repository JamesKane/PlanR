//! Links the programs with libr's script. (Build scripts run on the host;
//! they are part of the build, not of Plan R.)

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo");
    println!("cargo:rustc-link-arg-bins=-T{dir}/../../lib/r/user.ld");
    println!("cargo:rustc-link-arg-bins=-zmax-page-size=4096");
    println!("cargo:rerun-if-changed=../../lib/r/user.ld");
}
