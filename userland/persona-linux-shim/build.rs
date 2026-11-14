//j885
// SH7: the shim shares its process with a foreign program that expects the low address
// space (static aarch64 Linux images link at 0x200000-0x400000, the same place lld puts a
// native KUMO image). The shim links high, clear of both. — TEAL
use std::env;

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_os == "none" && target_arch == "aarch64" {
        println!("cargo:rustc-link-arg-bins=--image-base=0x1c000000");
    }
}
