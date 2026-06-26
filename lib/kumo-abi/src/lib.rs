#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod argv;
pub mod boot;
pub mod initrd;
pub mod object;
pub mod sys;

pub use argv::{pack_argv, unpack_argv};
pub use boot::{
    BootInfo, Framebuffer, FramebufferFormat, MemRegion, MemRegionKind, PlatformTable, Range,
    RawSlice,
};
pub use initrd::{
    entries, entry_paths, entry_table_bytes, find_entry, find_file, InitrdEntry, InitrdError,
    InitrdFile, ARGS_PATH, AUTOEXEC_PATH, CAT_PATH, DRV_BLK_PATH, DRV_FB_PATH, DRV_I2C_HID_PATH,
    DRV_SERIAL_PATH, FAT32_IMG_PATH, HELLO_PATH, INITRD_HEADER_LEN, LS_PATH, LUA_REPL_PATH,
    PERSONA_LINUX_HELLO_PATH, SORA_INIT_PATH, SVC_HEALTH_PATH, TTYD_PATH, WC_PATH,
};
pub use object::{Handle, KoId, ObjectKind, Rights, Signals, INVALID_HANDLE};
pub use sys::{
    decode_tlmm_gpio_irq, interrupt_authority_key, tlmm_gpio_irq, tlmm_gpio_irq_window_base, Errno,
    ProcessRunFlags, Status, Syscall, TlmmGpioIrq, VmarFlags, PROCESS_LABEL_BYTES,
};

pub const ABI_VERSION: u32 = 1;
