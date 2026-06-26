#![no_std]

//! Qualcomm GENI I2C engine support. // — OSPREY 2026-06-26 (d006)

pub mod geni;

pub use geni::{register, Controller, GeniError, RegisterIo, SourceClock};
