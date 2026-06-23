#![no_std]

//! Pure xHCI data structures and ring state machines.
//!
//! This crate deliberately contains no MMIO, allocation, physical-address discovery, or cache
//! maintenance. Callers provide device-visible IOVAs that must already be mapped in their
//! `DeviceCtx`; the future controller driver owns the unsafe register and DMA synchronization edge.

mod context;
mod ring;
mod trb;

pub use context::{
    endpoint_context_index, ContextSize, EndpointContext, EndpointType, InputControlContext,
    SlotContext, UsbSpeed,
};
pub use ring::{
    CommandRing, EventRing, EventRingSegmentTableEntry, RingSegment, RingToken, TransferRing,
};
pub use trb::{Event, NormalTransfer, SetupTransferType, Trb, TrbType};

/// Validation or ring-state failure detected before hardware is touched.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidField,
    MisalignedIova,
    BufferCrosses64K,
    RingTooSmall,
    RingTooLarge,
    RingMemoryLength,
    RingFull,
    CompletionNotPending,
    WrongRingType,
}
