use std::{env, path::PathBuf};

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_os == "none" && target_arch == "x86_64" {
        let linker =
            PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../kumo-user-x86.ld");
        println!("cargo:rustc-link-arg-bins=-T{}", linker.display());
        println!("cargo:rustc-link-arg-bins=-no-pie");
        println!("cargo:rerun-if-changed={}", linker.display());
    }
}
