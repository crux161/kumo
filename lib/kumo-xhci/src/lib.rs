#![no_std]
//j426
//j429
//j430
//j431
//j432

//! Pure xHCI data structures and ring state machines.
//!
//! This crate deliberately contains no MMIO, allocation, physical-address discovery, or cache
//! maintenance. Callers provide device-visible IOVAs that must already be mapped in their
//! `DeviceCtx`; the future controller driver owns the unsafe register and DMA synchronization edge.

mod context;
mod controller;
mod fdt;
mod noop;
mod probe;
mod registers;
mod ring;
mod trb;

pub use context::{
    endpoint_context_index, ContextSize, EndpointContext, EndpointType, InputControlContext,
    SlotContext, UsbSpeed,
};
pub use controller::{ControllerStatus, NoOpRegisterConfig, RegisterIo, RegisterLayout};
pub use fdt::{
    discover_x13s_usb0_xhci, GicInterrupt, XhciControllerTopology,
    X13S_USB0_XHCI_FIRST_LIGHT_MMIO_LEN, X13S_USB0_XHCI_MMIO_BASE, X13S_USB0_XHCI_MMIO_MIN_LEN,
    X13S_USB0_XHCI_REGISTER_MMIO_LEN, X13S_USB0_XHCI_STREAM_ID,
};
pub use noop::{prepare_noop_command_image, NoOpCommandImage, NoOpCommandIovas};
pub use probe::{XhciProbeConfig, XHCI_PROBE_CONFIG_LEN};
pub use registers::{portsc_offset, CapabilityRegisters, PortStatus};
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
    RegisterWindowTooSmall,
    ControllerNotHalted,
    ControllerNotReady,
}
