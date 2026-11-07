#![no_std]
//j550
//j803
//j809
//j828
//j831

//! J803: the per-request volume validator moved to `drv-blk` as the shared block codec, so the
//! NVMe service enforces the identical contract. The USB names stay as re-exports.

pub use drv_blk::{volume_request as storage_request, VolumeRequest as StorageRequest};

pub mod ax88;

pub mod completion;
pub mod nic;
