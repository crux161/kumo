#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

//j774
//j798
//j815
//j822
//j824

pub mod actor;
pub mod argv;
pub mod boot;
pub mod call;
pub mod initrd;
pub mod object;
pub mod sys;

pub use argv::{pack_argv, unpack_argv};
pub use boot::{
    BootInfo, Framebuffer, FramebufferFormat, MemRegion, MemRegionKind, PlatformTable, Range,
    RawSlice,
};
pub use call::{pack_call_payload, unpack_call_payload, CALL_WORDS, MAX_CALL_BYTES};
pub use initrd::{
    entries, entry_paths, entry_table_bytes, find_entry, find_file, InitrdEntry, InitrdError,
    InitrdFile, ACTORCALL_PATH, ARGS_PATH, AUTOEXEC_PATH, BMP_VIEW_PATH, CAT_PATH, DRV_BLK_PATH,
    DRV_FB_PATH, DRV_GPU_PATH, DRV_I2C_HID_PATH, DRV_NVME_PATH, DRV_SERIAL_PATH, DRV_XHCI_PATH,
    FAT32_IMG_PATH, FATFS_PATH, GFX_CLIENT_PATH, GFX_HOLD_PATH, GFX_KEYS_PATH,
    GPU_CONTAIN_CLIENT_PATH, GPU_CONTAIN_SERVER_PATH, GPU_LANE_PATH, GROUP_PATH, HELLO_PATH,
    HOSTNAME_PATH, IDENTITYD_PATH, INITRD_HEADER_LEN, INITRD_MAX_ENTRIES, INITRD_TABLE_MAX,
    KILO_PATH, LS_PATH, LUA_REPL_PATH, NETPROBE_PATH, NETSTACK_PATH, PASSWD_PATH,
    PERSONA_LINUX_HELLO_PATH, RELIBC_SMOKE_PATH, SHADOW_PATH, SORA_INIT_PATH, SVC_HEALTH_PATH,
    THREADS_PATH, TTYD_PATH, WC_PATH,
};
pub use object::{Handle, KoId, ObjectKind, Rights, Signals, INVALID_HANDLE};
pub use sys::{
    decode_tlmm_gpio_irq, interrupt_authority_key, tlmm_gpio_irq, tlmm_gpio_irq_window_base,
    DevicePowerStatus, Errno, ProcessRunFlags, Status, Syscall, TlmmGpioIrq, VmarFlags,
    KERNEL_SHELL_COMMANDS,
};

// v2: BootInfo carries `board_id` (the baked board identity — DESIGN/017 §4 step 2).
// v3: PlatformTable carries an explicit loader-selected PL011 console override.
pub const ABI_VERSION: u32 = 3;
