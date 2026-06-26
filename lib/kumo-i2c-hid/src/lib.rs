#![no_std]

//! Pure protocol parsing for HID-over-I2C. // — OSPREY 2026-06-26 (d006)

mod fdt;
mod protocol;
mod report_descriptor;

pub use fdt::{discover_keyboard, GicInterrupt, GpioInterrupt, KeyboardTopology};
pub use protocol::{
    boot_keyboard_report, Command, HidDescriptor, InputFrame, PowerState, ProtocolError,
};
pub use report_descriptor::{
    find_boot_keyboard, inspect_report_descriptor, KeyboardReport, ReportDescriptorError,
    ReportDescriptorInfo,
};
