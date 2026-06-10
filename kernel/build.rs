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
        println!("cargo:rerun-if-changed=kumo-kernel.ld");
    }

    if target_os == "none" && target_arch == "x86_64" {
        // GRUB / any Multiboot loader loads the x86_64 kernel at the fixed 1 MiB link
        // address (no rebasing, so no --emit-relocs); the link script puts the Multiboot
        // header first and the entry at `_start` (the 32-bit long-mode trampoline).
        let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
        println!("cargo:rustc-link-arg-bins=-T{manifest_dir}/kumo-kernel-x86.ld");
        println!("cargo:rustc-link-arg-bins=-no-pie");
        println!("cargo:rerun-if-changed=kumo-kernel-x86.ld");
    }
}
