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
pub use initrd::{find_file, InitrdError, InitrdFile, FAT32_IMG_PATH, SORA_INIT_PATH};
pub use object::{Handle, KoId, ObjectKind, Rights, Signals, INVALID_HANDLE};
pub use sys::{Errno, Status, Syscall};

pub const ABI_VERSION: u32 = 1;
