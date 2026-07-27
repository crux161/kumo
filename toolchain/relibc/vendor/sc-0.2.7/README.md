# Raw system calls for Rust

This library allows Rust code to invoke system calls directly.

⚠️ NOTE: Only these architectures have been ported to the stable (as of 1.59) `asm!` macro

- aarch64
- riscv64
- x86_64

All the other architectures use the deprecated `llvm_asm!` macro which has already been removed.
To use this crate with those architectures you'll need to use an older nightly like
`nightly-2022-01-14`

See the [list of supported platforms](https://github.com/japaric/syscall.rs/tree/master/src/platform).  Additions are very welcome!

We do not intend to provide wrapper functions like `read(2)` etc. because there are many subtly different ways to define them in Rust.
