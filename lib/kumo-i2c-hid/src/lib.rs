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
mod tlmm;

pub use fdt::{
    discover_i2c21_pinctrl, discover_i2c_hid_bus, discover_keyboard, GicInterrupt, GpioInterrupt,
    HidDeviceKind, HidDeviceTopology, I2c21PinctrlTopology, I2cHidBusTopology, KeyboardTopology,
    PinctrlRefs, TlmmFunction, TlmmOutput, TlmmPinctrlGroup, MAX_I2C_HID_DEVICES, MAX_PINCTRL_REFS,
    MAX_TLMM_PINCTRL_GROUPS, MAX_TLMM_PINS_PER_GROUP,
};
pub use geni::{register, Controller, GeniError, RegisterIo, SourceClock};
pub use protocol::{
    boot_keyboard_report, boot_mouse_report, BootMouseReport, Command, HidDescriptor, InputFrame,
    MouseButtons, OutputReportError, PowerState, ProtocolError, BOOT_MOUSE_REPORT_BYTES,
};
pub use report_descriptor::{
    find_boot_keyboard, find_boot_mouse, find_led_output_report, find_led_output_report_id,
    KeyboardReport, LedOutputReport, MouseReport, ReportDescriptorError,
};
pub use tlmm::{
    apply_tlmm_pinctrl_plan, sc8280xp_i2c21_tlmm_plan, TlmmPinctrlPlan, TlmmPinctrlPlanError,
    TlmmRegisterIo, TlmmRegisterUpdate, MAX_TLMM_PINCTRL_UPDATES, SC8280XP_TLMM_GPIO_CTL_OFFSET,
    SC8280XP_TLMM_GPIO_IO_OFFSET, SC8280XP_TLMM_GPIO_STRIDE,
};
