#![cfg_attr(feature = "bench", feature(test))]
#![cfg_attr(feature = "no_std", no_std)]

extern crate alloc;

pub mod compile;
pub mod ctype;
pub mod immut_vec;
pub mod matcher;
pub mod tree;

pub use compile::PosixRegexBuilder;
pub use matcher::PosixRegex;
