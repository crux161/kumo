#![no_std]
#![forbid(unsafe_code)]
//j826
//j831
//j832
//j833
//! Allocation-free ax88 (AX88179/AX88178A) wire codecs. No USB transport or hardware
//! access. A successful parse validates the entire aggregate before yielding any frame.
//! Wire facts: supplied resources/net/ax88179_178a.c; see docs/ax88-protocol.md.

pub mod control;
pub mod identity;
pub mod inventory;
pub mod link;
pub mod profile;
pub mod rx;
pub mod status;
pub mod tx;

/// Ethernet header through payload, without FCS; initial untagged MTU1500 policy.
pub const FRAME_MIN: usize = 14;
pub const FRAME_MAX: usize = 1514;
/// Transfer and validation-work budgets, not silicon capacity promises.
pub const AGGREGATE_MAX: usize = 32 * 1024;
pub const METADATA_MAX: usize = 1024;
