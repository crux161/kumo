#![no_std]
//j540
//j541
//j545
//j703
//j827

//! Pure xHCI data structures and ring state machines.
//!
//! This crate deliberately contains no MMIO, allocation, physical-address discovery, or cache
//! maintenance. Callers provide device-visible IOVAs that must already be mapped in their
//! `DeviceCtx`; the future controller driver owns the unsafe register and DMA synchronization edge.

mod context;
mod control;
mod controller;
mod dma;
mod dwc3;
mod fdt;
mod hub;
mod noop;
mod probe;
mod registers;
mod ring;
mod stash;
mod trb;

pub use context::{
    endpoint_context_index, ContextSize, EndpointContext, EndpointType, InputControlContext,
    SlotContext, UsbSpeed,
};
pub use control::{ControlData, ControlError, ControlPipe, ControlProgress, CONTROL_DATA_MAX};
pub use controller::{ControllerStatus, NoOpRegisterConfig, RegisterIo, RegisterLayout};
pub use dma::{DmaBuf, DmaQwords, DmaTrbs, QwordSink, TrbSink, TrbSource};
pub use dwc3::{HostInit, RegisterUpdate};
pub use fdt::{
    discover_primary_xhci, discover_rk3588_usb0_xhci, discover_x13s_usb0_xhci, GicInterrupt,
    XhciControllerTopology, RK3588_USB0_XHCI_IRQ, RK3588_USB0_XHCI_MMIO_BASE,
    RK3588_USB0_XHCI_MMIO_LEN, RK3588_USB0_XHCI_REGISTER_MMIO_LEN,
    X13S_USB0_XHCI_FIRST_LIGHT_MMIO_LEN, X13S_USB0_XHCI_MMIO_BASE, X13S_USB0_XHCI_MMIO_MIN_LEN,
    X13S_USB0_XHCI_REGISTER_MMIO_LEN, X13S_USB0_XHCI_STREAM_ID,
};
pub use hub::{
    HubConnectDebounce, HubConnectVerdict, HubDescriptor, HubPorts, HUB_CONNECT_WINDOW_NS,
    HUB_DEBOUNCE_STABLE_NS, HUB_DEBOUNCE_STEP_NS, HUB_DEBOUNCE_TIMEOUT_NS, MAX_HUB_PORTS,
};
pub use noop::{
    prepare_noop_command_image, NoOpCommandCompletion, NoOpCommandImage, NoOpCommandIovas,
};
pub use probe::{
    XhciProbeConfig, FLAG_DWC3, QUIRK_DIS_DEL_PHY_POWER_CHG, QUIRK_DIS_ENBLSLPM,
    QUIRK_DIS_RXDET_INP3, QUIRK_DIS_TX_IPGAP_LINECHECK, QUIRK_DIS_U1_ENTRY, QUIRK_DIS_U2_ENTRY,
    QUIRK_DIS_U2_FREECLK_EXISTS, XHCI_NO_STREAM_ID, XHCI_PROBE_CONFIG_LEN,
};
pub use registers::{
    port_power_value, port_reset_value, port_state_to_neutral, port_warm_reset_value,
    portsc_offset, CapabilityRegisters, PortStatus, RootPortScan,
};
pub use ring::{
    CommandRing, EventRing, EventRingSegmentTableEntry, RingSegment, RingToken, TransferRing,
};
pub use stash::{StashedTransfer, TransferEventStash, STASH_CAPACITY};
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
