//! Inject the kernel link script for the freestanding aarch64 image only.
//!
//! Host builds (the `std` test build) and the x86_64 backend are untouched: the
//! link arg is emitted solely when the target OS is `none`, and only for binary
//! targets, so `cargo test`/`cargo check` on the host keep working unchanged.

use std::env;

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    if target_os == "none" && target_arch == "aarch64" {
        let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
        println!("cargo:rustc-link-arg-bins=-T{manifest_dir}/kumo-kernel.ld");
        println!("cargo:rustc-link-arg-bins=-no-pie");
        // Retain relocation records so Nijigumo can rebase the (statically-linked)
        // kernel to whatever physical address a given board actually has free.
        println!("cargo:rustc-link-arg-bins=--emit-relocs");
        println!("cargo:rerun-if-changed=kumo-kernel.ld");
    }
}
