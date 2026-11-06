#![no_std]
//j813
//j814

//! The network domain's shared, allocation-free core (PLAN/020, DESIGN/026): the budgets every
//! netstack instance is sized by, a bounded in-memory loopback `Device` for smoltcp, and the
//! KNF1 frame and KNS1/KNR1 socket wire codecs. The process around it is NET2b.
//!
//! The crate has no `alloc`: nothing in it can grow after start.

pub mod budget;
pub mod frame;
pub mod loopback;
pub mod socket;

pub use loopback::{InjectError, Loopback};
pub use smoltcp;
