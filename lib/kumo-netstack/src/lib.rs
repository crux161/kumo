#![no_std]
//j813

//! The network domain's shared, allocation-free core (PLAN/020, DESIGN/026): the budgets every
//! netstack instance is sized by, and a bounded in-memory loopback `Device` for smoltcp. The
//! frame and socket wire codecs join this crate in NET2a-2; the process around it is NET2b.
//!
//! The crate has no `alloc`: nothing in it can grow after start.

pub mod budget;
pub mod loopback;

pub use loopback::{InjectError, Loopback};
pub use smoltcp;
