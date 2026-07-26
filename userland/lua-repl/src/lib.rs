//j485
#![no_std]

//! Build seam between KUMO's freestanding REPL process and the vendored Piccolo VM.
//!
//! The upstream 0.3.3 release still requires `std`. Keeping it behind an opt-in feature lets
//! normal target images retain the honest placeholder while host checks prove that the vendored
//! dependency closure is complete. The next slice can port that seam to `core` + `alloc` without
//! making network availability part of the work.

#[cfg(feature = "piccolo-vm")]
pub use piccolo as vm;
