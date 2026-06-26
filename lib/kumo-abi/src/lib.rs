#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod argv;
pub mod boot;
pub mod i2c;
pub mod initrd;
pub mod object;
pub mod sys; // — OSPREY 2026-06-26 (d005)

pub use argv::{pack_argv, unpack_argv};
pub use boot::{
    BootInfo, Framebuffer, FramebufferFormat, MemRegion, MemRegionKind, PlatformTable, Range,
    RawSlice,
};
pub use i2c::{
    I2cFuncsResponse, I2cOpcode, I2cRequestHeader, I2cSmbusReadByteResponse,
    I2cSmbusWriteQuickResponse, I2cTransferRequest, I2cTransferResponse, I2C_FUNC_I2C,
    I2C_FUNC_I2C_TRANSFER, I2C_FUNC_SMBUS_QUICK, I2C_FUNC_SMBUS_READ_BYTE,
};
pub use initrd::{
    entries, entry_paths, entry_table_bytes, find_entry, find_file, InitrdEntry, InitrdError,
    InitrdFile, ARGS_PATH, AUTOEXEC_PATH, CAT_PATH, DRV_BLK_PATH, DRV_FB_PATH, DRV_GENI_I2C_PATH,
    DRV_I2C_HID_PATH, DRV_SERIAL_PATH, FAT32_IMG_PATH, HELLO_PATH, INITRD_ENTRY_LEN, INITRD_HEADER_LEN,
    INITRD_MAGIC, INITRD_PATH_MAX, INITRD_VERSION, LS_PATH, LUA_REPL_PATH,
    PERSONA_LINUX_HELLO_PATH, SORA_INIT_PATH, SVC_HEALTH_PATH, TTYD_PATH, WC_PATH,
};
pub use object::{Handle, KoId, ObjectKind, Rights, Signals, INVALID_HANDLE};
pub use sys::{
    decode_tlmm_gpio_irq, interrupt_authority_key, tlmm_gpio_irq, tlmm_gpio_irq_window_base, Errno,
    ProcessRunFlags, Status, Syscall, TlmmGpioIrq, VmarFlags, PROCESS_LABEL_BYTES,
}; // — OSPREY 2026-06-26 (d005)

pub const ABI_VERSION: u32 = 1;
