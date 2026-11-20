//j913
//! AMD64 builds link this program as a fixed-address native KUMO image (PLAN/026 U3a): the x86
//! user linker script and no PIE, as `hello` and Sora do. arm64 builds are untouched.
use std::{env, path::PathBuf};

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_os == "none" && target_arch == "x86_64" {
        let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
        let mut linker = manifest.clone();
        while !linker.join("kumo-user-x86.ld").exists() {
            assert!(
                linker.pop(),
                "kumo-user-x86.ld not found above {}",
                manifest.display()
            );
        }
        let linker = linker.join("kumo-user-x86.ld");
        println!("cargo:rustc-link-arg-bins=-T{}", linker.display());
        println!("cargo:rustc-link-arg-bins=-no-pie");
        println!("cargo:rerun-if-changed={}", linker.display());
    }
}
