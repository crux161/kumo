#![no_std]

//! Pure support for the ThinkPad X13s internal HID-over-I2C keyboard path.
//!
//! The hardware topology is discovered from the staged FDT, the HID-over-I2C wire structures are
//! decoded without allocation, and the Qualcomm GENI I2C engine is driven through an abstract
//! register interface. The future user-mode driver supplies the sole unsafe MMIO implementation.

mod fdt;
mod geni;
mod protocol;
mod report_descriptor;

pub use fdt::{discover_keyboard, GicInterrupt, GpioInterrupt, KeyboardTopology};
pub use geni::{register, Controller, GeniError, RegisterIo, SourceClock};
pub use protocol::{
    boot_keyboard_report, Command, HidDescriptor, InputFrame, PowerState, ProtocolError,
};
pub use report_descriptor::{
    find_boot_keyboard, inspect_report_descriptor, KeyboardReport, ReportDescriptorError,
    ReportDescriptorInfo,
};
