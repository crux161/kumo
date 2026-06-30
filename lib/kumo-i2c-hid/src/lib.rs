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

pub use fdt::{
    discover_i2c_hid_bus, discover_keyboard, GicInterrupt, GpioInterrupt, HidDeviceKind,
    HidDeviceTopology, I2cHidBusTopology, KeyboardTopology, MAX_I2C_HID_DEVICES,
};
pub use geni::{register, Controller, GeniError, RegisterIo, SourceClock};
pub use protocol::{
    boot_keyboard_report, boot_mouse_report, BootMouseReport, Command, HidDescriptor, InputFrame,
    MouseButtons, PowerState, ProtocolError, BOOT_MOUSE_REPORT_BYTES,
};
pub use report_descriptor::{
    find_boot_keyboard, find_boot_mouse, find_led_output_report_id, KeyboardReport, MouseReport,
    ReportDescriptorError,
};
