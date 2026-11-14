#![no_std]
//j828
//j829
//j830
//j831
//j886

//! Pure xHCI data structures and ring state machines.
//!
//! This crate deliberately contains no MMIO, allocation, physical-address discovery, or cache
//! maintenance. Callers provide device-visible IOVAs that must already be mapped in their
//! `DeviceCtx`; the future controller driver owns the unsafe register and DMA synchronization edge.

mod context;
mod control;
mod controller;
mod custody;
mod dma;
mod dwc3;
mod fdt;
mod hub;
mod noop;
mod probe;
mod quiesce;
mod registers;
mod ring;
mod stash;
mod trb;

pub use context::{
    endpoint_context_index, ContextSize, EndpointContext, EndpointType, InputControlContext,
    SlotContext, SlotSnapshot, SlotState, UsbSpeed,
};
pub use control::{ControlData, ControlError, ControlPipe, ControlProgress, CONTROL_DATA_MAX};
pub use controller::{ControllerStatus, NoOpRegisterConfig, RegisterIo, RegisterLayout};
pub use custody::{
    CustodyError, DmaCustodyLedger, DmaMemory, DMA_ACK_LEN, DMA_ALLOCATION_MAX, DMA_CUSTODY_LEN,
    DMA_CUSTODY_MAX, DMA_TOTAL_MAX,
};
pub use dma::{
    boundary_window, DmaBuf, DmaQwords, DmaTrbs, QwordSink, TrbSink, TrbSource, TRANSFER_BOUNDARY,
};
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
pub use quiesce::{
    EndpointQuiesce, EndpointSnapshot, EndpointState, QuiesceError, QuiesceReport, StopVerdict,
    STOP_RETRIES,
};
pub use registers::{
    needs_bus_reset, port_clear_reset_change_value, port_power_value, port_reset_value,
    port_state_to_neutral, port_warm_reset_value, portsc_offset, CapabilityRegisters, PortStatus,
    RootPortScan,
};
pub use ring::{
    CommandRing, EventRing, EventRingSegmentTableEntry, RingRestart, RingSegment, RingToken,
    TransferRing,
};
pub use stash::{
    CompletionKey, CompletionRouter, CompletionStats, RouteError, RoutedTransfer, TransferTicket,
    COMPLETION_LANES, COMPLETION_TRBS,
};
pub use trb::{
    is_stop_completion, Event, NormalTransfer, SetupTransferType, Trb, TrbType,
    COMPLETION_CONTEXT_STATE_ERROR, COMPLETION_SHORT_PACKET, COMPLETION_STOPPED,
    COMPLETION_STOPPED_LENGTH_INVALID, COMPLETION_STOPPED_SHORT_PACKET, COMPLETION_SUCCESS,
};

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
    /// A ring published or consumed TRBs after the restart position was taken.
    RingPositionChanged,
}
