#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod boot;
pub mod initrd;
pub mod object;
pub mod sys;

pub use boot::{
    BootInfo, Framebuffer, FramebufferFormat, MemRegion, MemRegionKind, PlatformTable, Range,
    RawSlice,
};
pub use initrd::{
    entry_paths, entry_table_bytes, find_file, InitrdError, InitrdFile, AUTOEXEC_PATH,
    DRV_BLK_PATH, DRV_FB_PATH, DRV_SERIAL_PATH, FAT32_IMG_PATH, HELLO_PATH, INITRD_HEADER_LEN,
    LS_PATH, PERSONA_LINUX_HELLO_PATH, SORA_INIT_PATH, SVC_HEALTH_PATH,
};
pub use object::{Handle, KoId, ObjectKind, Rights, Signals, INVALID_HANDLE};
pub use sys::{Errno, Status, Syscall, VmarFlags};

pub const ABI_VERSION: u32 = 1;
