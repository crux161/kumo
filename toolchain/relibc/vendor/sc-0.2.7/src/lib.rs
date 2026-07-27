// Copyright 2014 The syscall.rs Project Developers. See the
// COPYRIGHT file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Raw system calls for Rust.
//!
//! NOTE: Only these architectures have been ported to the stable (as of 1.59) `asm!` macro
//!
//! - aarch64
//! - riscv64
//! - x86_64 
//! 
//! All the other architectures use the deprecated `llvm_asm!` macro which has already been removed.
//! To use this crate with those architectures you'll need to use an older nightly like
//! `nightly-2022-01-14`

// Reference http://man7.org/linux/man-pages/man2/syscall.2.html

#![allow(deprecated)] // llvm_asm!
#![deny(warnings)]
#![no_std]
#![cfg_attr(any(
    target_arch = "arm",
    target_arch = "mips",
    target_arch = "mips64",
    target_arch = "powerpc",
    target_arch = "powerpc64",
    target_arch = "sparc64",
    target_arch = "x86"
), feature(llvm_asm))]

#[cfg(test)]
extern crate std;

pub use platform::*;

pub mod macros;

#[cfg(all(any(target_os = "linux", target_os = "android"),
          target_arch = "aarch64"))]
#[path="platform/linux-aarch64/mod.rs"]
pub mod platform;

#[cfg(all(any(target_os = "linux", target_os = "android"),
          target_arch = "arm"))]
#[path="platform/linux-armeabi/mod.rs"]
pub mod platform;

#[cfg(all(any(target_os = "linux", target_os = "android"),
          target_arch = "mips"))]
#[path="platform/linux-mips/mod.rs"]
pub mod platform;

#[cfg(all(any(target_os = "linux", target_os = "android"),
          target_arch = "mips64"))]
#[path="platform/linux-mips64/mod.rs"]
pub mod platform;

#[cfg(all(any(target_os = "linux", target_os = "android"),
          target_arch = "powerpc"))]
#[path="platform/linux-powerpc/mod.rs"]
pub mod platform;

#[cfg(all(any(target_os = "linux", target_os = "android"),
          target_arch = "powerpc64"))]
#[path="platform/linux-powerpc64/mod.rs"]
pub mod platform;

#[cfg(all(any(target_os = "linux", target_os = "android"),
          target_arch = "riscv64"))]
#[path="platform/linux-riscv64/mod.rs"]
pub mod platform;

#[cfg(all(any(target_os = "linux", target_os = "android"),
          target_arch = "sparc64"))]
#[path="platform/linux-sparc64/mod.rs"]
pub mod platform;

#[cfg(all(any(target_os = "linux", target_os = "android"),
          target_arch = "x86"))]
#[path="platform/linux-x86/mod.rs"]
pub mod platform;

#[cfg(all(any(target_os = "linux", target_os = "android"),
          target_arch = "x86_64"))]
#[path="platform/linux-x86_64/mod.rs"]
pub mod platform;

#[cfg(all(target_os = "freebsd",
          target_arch = "x86_64"))]
#[path="platform/freebsd-x86_64/mod.rs"]
pub mod platform;

#[cfg(all(target_os = "macos",
          target_arch = "x86_64"))]
#[path="platform/macos-x86_64/mod.rs"]
pub mod platform;

#[cfg(all(target_os = "macos",
          target_arch = "aarch64"))]
#[path="platform/macos-aarch64/mod.rs"]
pub mod platform;
