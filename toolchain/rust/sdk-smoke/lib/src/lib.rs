//j857
#![feature(restricted_std)]
//! SH0: the second crate of the consumer proof. It exists so the sdk-smoke image is
//! built from more than one Rust crate against the prebuilt sysroot.
//! — TIMBERDOODLE

pub fn label() -> &'static str {
    "sdk-smoke-lib/0.1.0"
}
