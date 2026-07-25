#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
#![deny(unsafe_op_in_unsafe_fn)]
//j434
//j460
//j467

//! Nijigumo's UEFI front-end (`BOOTAA64.EFI`).
//!
//! This app discovers the machine over UEFI Boot Services (GOP framebuffer, the
//! staged device tree, the firmware memory map), loads the KUMO kernel ELF (and an
//! optional initrd) from the EFI System Partition, assembles and validates the
//! `kumo_abi::BootInfo` handoff, then **exits boot services and jumps to Ziwei**
//! (the kernel `_start`) with `x0` holding the `BootInfo` pointer.
//!
//! Boot-log honesty rule (Guidance 002 §1): every reported line carries real
//! values; an absent device prints `absent`; nothing is faked. All UEFI-console
//! output happens *before* ExitBootServices, so it is visible on a board whose
//! only console is the firmware's (e.g. the X13s); after the jump the kernel's own
//! Stage-A console takes over.
//!
//! The EFI bindings are hand-rolled (no external `uefi` crate) to keep the
//! bootloader dependency-light and auditable; only the protocol members Nijigumo
//! calls are typed, the rest of each service table is opaque pointers so the field
//! offsets stay correct.

use core::ffi::c_void;
use core::fmt;
use core::mem::{size_of, MaybeUninit};
#[cfg(not(test))]
use core::panic::PanicInfo;
use core::ptr;

use kumo_abi::{BootInfo, Framebuffer, FramebufferFormat, MemRegion, Range, RawSlice};
use niji_loader::elf::{parse_elf64, EM_AARCH64};
use niji_loader::{summarize_platform, validate_boot_info};
use niji_uefi::{
    build_boot_info, efi_memory_type, efi_memory_type_is_firmware_owned, efi_pixel_format,
    fdt_total_size, framebuffer_from_gop, is_fdt_magic, is_plausible_firmware_pointer,
    mem_region_kind_from_efi, parse_pl011_console, BootConfig, UefiHandoffSeed,
};

pub type EfiHandle = *mut c_void;
pub type EfiStatus = usize;

const EFI_SUCCESS: EfiStatus = 0;
const EFI_FILE_MODE_READ: u64 = 0x0000_0000_0000_0001;
const EFI_PAGE_SIZE: u64 = 4096;
const EFI_ALLOCATE_ANY_PAGES: u32 = 0;
const MAX_DTB_BYTES: u64 = 16 * 1024 * 1024;

/// Staged asset paths on the ESP (backslash-separated, as UEFI expects).
/// One image stages at most one DTB; the loader probes each known board path in order and
/// takes the first that exists (PLAN_VII R1 added the RK3588 entry). — CORVUS
const DTB_ESP_PATHS: &[&str] = &[
    "\\EFI\\KUMO\\dtb\\qcom\\sc8280xp-lenovo-thinkpad-x13s.dtb",
    "\\EFI\\KUMO\\dtb\\rockchip\\rk3588-orangepi-5-plus.dtb",
];
const KERNEL_ESP_PATH: &str = "\\EFI\\KUMO\\kernel\\kumo-kernel.elf";
const INITRD_ESP_PATH: &str = "\\EFI\\KUMO\\initrd.img";
/// The staged boot manifest ([`BootConfig`]). The imager writes one per target image; the loader
/// consumes its fixed-size `board` and optional `console-uart` policy, but still resolves the
/// kernel/initrd/DTB from the constants above. Migrating those path reads to the manifest is a
/// later slice. — KESTREL 2026-07-17
const BOOT_CONFIG_ESP_PATH: &str = "\\EFI\\KUMO\\nijigumo.conf";

// === EFI GUIDs ===============================================================

#[repr(C)]
#[derive(PartialEq, Eq)]
struct EfiGuid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

const fn guid(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> EfiGuid {
    EfiGuid {
        data1,
        data2,
        data3,
        data4,
    }
}

const GRAPHICS_OUTPUT_GUID: EfiGuid = guid(
    0x9042_a9de,
    0x23dc,
    0x4a38,
    [0x96, 0xfb, 0x7a, 0xde, 0xd0, 0x80, 0x51, 0x6a],
);
const LOADED_IMAGE_GUID: EfiGuid = guid(
    0x5b1b_31a1,
    0x9562,
    0x11d2,
    [0x8e, 0x3f, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
);
const SIMPLE_FS_GUID: EfiGuid = guid(
    0x964e_5b22,
    0x6459,
    0x11d2,
    [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
);
/// `EFI_DTB_TABLE_GUID` — the firmware exposes the active flattened device tree as a
/// configuration-table entry under this GUID (the Raspberry Pi pftf/EDK2 firmware does this;
/// it is also what the Linux EFI stub consumes). Lets us recover a DTB on platforms that ship
/// none on the ESP (Pi 5, generic UEFI), while x13s keeps its bundled, newer ESP DTB.
const EFI_DTB_TABLE_GUID: EfiGuid = guid(
    0xb1b6_21d5,
    0xf19c,
    0x41a5,
    [0x83, 0x0b, 0xd9, 0x15, 0x2c, 0x69, 0xaa, 0xe0],
);
/// `EFI_PXE_BASE_CODE_PROTOCOL_GUID` — present on the device handle of an image the firmware
/// downloaded over PXE. It is how a netbooted loader reaches the TFTP server it came from, which
/// is the only way to fetch the remaining payloads: a PXE-booted image has no ESP filesystem.
const PXE_BASE_CODE_GUID: EfiGuid = guid(
    0x03c4_e603,
    0xac28,
    0x11d3,
    [0x9a, 0x2d, 0x00, 0x90, 0x27, 0x3f, 0xc1, 0x4d],
);
const FILE_INFO_GUID: EfiGuid = guid(
    0x0957_6e92,
    0x6d3f,
    0x11d2,
    [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
);

// === EFI tables & protocols ==================================================

#[repr(C)]
struct EfiTableHeader {
    signature: u64,
    revision: u32,
    header_size: u32,
    crc32: u32,
    reserved: u32,
}

#[repr(C)]
pub struct EfiSystemTable {
    header: EfiTableHeader,
    firmware_vendor: *mut u16,
    firmware_revision: u32,
    console_in_handle: EfiHandle,
    con_in: *mut EfiSimpleTextInputProtocol,
    console_out_handle: EfiHandle,
    con_out: *mut EfiSimpleTextOutputProtocol,
    standard_error_handle: EfiHandle,
    std_err: *mut EfiSimpleTextOutputProtocol,
    runtime_services: *mut c_void,
    boot_services: *mut EfiBootServices,
    number_of_table_entries: usize,
    configuration_table: *mut c_void,
}

/// One entry of the UEFI configuration table (`EfiSystemTable.configuration_table` is an
/// array of `number_of_table_entries` of these). Scanned for [`EFI_DTB_TABLE_GUID`].
#[repr(C)]
struct EfiConfigurationTable {
    vendor_guid: EfiGuid,
    vendor_table: *mut c_void,
}

#[repr(C)]
struct EfiSimpleTextOutputProtocol {
    reset: extern "efiapi" fn(*mut Self, bool) -> EfiStatus,
    output_string: extern "efiapi" fn(*mut Self, *const u16) -> EfiStatus,
}

#[repr(C)]
struct EfiSimpleTextInputProtocol {
    reset: extern "efiapi" fn(*mut Self, bool) -> EfiStatus,
    read_key_stroke: extern "efiapi" fn(*mut Self, *mut EfiInputKey) -> EfiStatus,
}

#[repr(C)]
struct EfiInputKey {
    scan_code: u16,
    unicode_char: u16,
}

// Boot Services function-pointer types Nijigumo invokes.
type EfiAllocatePages = extern "efiapi" fn(
    alloc_type: u32,
    memory_type: u32,
    pages: usize,
    memory: *mut u64,
) -> EfiStatus;
type EfiGetMemoryMap = extern "efiapi" fn(
    map_size: *mut usize,
    map: *mut c_void,
    map_key: *mut usize,
    desc_size: *mut usize,
    desc_version: *mut u32,
) -> EfiStatus;
type EfiAllocatePool =
    extern "efiapi" fn(pool_type: u32, size: usize, buffer: *mut *mut c_void) -> EfiStatus;
type EfiFreePool = extern "efiapi" fn(buffer: *mut c_void) -> EfiStatus;
type EfiHandleProtocol = extern "efiapi" fn(
    handle: EfiHandle,
    protocol: *const EfiGuid,
    interface: *mut *mut c_void,
) -> EfiStatus;
type EfiExitBootServices = extern "efiapi" fn(image_handle: EfiHandle, map_key: usize) -> EfiStatus;
type EfiStall = extern "efiapi" fn(microseconds: usize) -> EfiStatus;
type EfiLocateProtocol = extern "efiapi" fn(
    protocol: *const EfiGuid,
    registration: *mut c_void,
    interface: *mut *mut c_void,
) -> EfiStatus;

/// `EFI_BOOT_SERVICES`, laid out through `LocateProtocol` (the last member we
/// call). Unused slots are opaque pointers so every typed member keeps its
/// spec-defined offset.
#[repr(C)]
struct EfiBootServices {
    header: EfiTableHeader,
    raise_tpl: *const c_void,
    restore_tpl: *const c_void,
    allocate_pages: EfiAllocatePages,
    free_pages: *const c_void,
    get_memory_map: EfiGetMemoryMap,
    allocate_pool: EfiAllocatePool,
    free_pool: EfiFreePool,
    create_event: *const c_void,
    set_timer: *const c_void,
    wait_for_event: *const c_void,
    signal_event: *const c_void,
    close_event: *const c_void,
    check_event: *const c_void,
    install_protocol_interface: *const c_void,
    reinstall_protocol_interface: *const c_void,
    uninstall_protocol_interface: *const c_void,
    handle_protocol: EfiHandleProtocol,
    reserved: *const c_void,
    register_protocol_notify: *const c_void,
    locate_handle: *const c_void,
    locate_device_path: *const c_void,
    install_configuration_table: *const c_void,
    load_image: *const c_void,
    start_image: *const c_void,
    exit: *const c_void,
    unload_image: *const c_void,
    exit_boot_services: EfiExitBootServices,
    get_next_monotonic_count: *const c_void,
    stall: EfiStall,
    set_watchdog_timer: *const c_void,
    connect_controller: *const c_void,
    disconnect_controller: *const c_void,
    open_protocol: *const c_void,
    close_protocol: *const c_void,
    open_protocol_information: *const c_void,
    protocols_per_handle: *const c_void,
    locate_handle_buffer: *const c_void,
    locate_protocol: EfiLocateProtocol,
}

#[repr(C)]
struct EfiLoadedImageProtocol {
    revision: u32,
    parent_handle: EfiHandle,
    system_table: *const c_void,
    device_handle: EfiHandle,
    file_path: *const c_void,
    reserved: *const c_void,
    load_options_size: u32,
    load_options: *const c_void,
    image_base: *const c_void,
    image_size: u64,
    image_code_type: u32,
    image_data_type: u32,
    unload: *const c_void,
}

#[repr(C)]
struct EfiSimpleFileSystemProtocol {
    revision: u64,
    open_volume: extern "efiapi" fn(*mut Self, *mut *mut EfiFileProtocol) -> EfiStatus,
}

/// `EFI_PXE_BASE_CODE_MTFTP`. `server_ip` is an `EFI_IP_ADDRESS` (16-byte union) and `filename`
/// is NUL-terminated ASCII. Optional pointers may be null.
type EfiPxeBaseCodeMtftp = extern "efiapi" fn(
    this: *mut EfiPxeBaseCodeProtocol,
    operation: u32,
    buffer: *mut c_void,
    overwrite: bool,
    buffer_size: *mut u64,
    block_size: *mut usize,
    server_ip: *const [u8; 16],
    filename: *const u8,
    info: *const c_void,
    dont_use_buffer: bool,
) -> EfiStatus;

const EFI_PXE_TFTP_GET_FILE_SIZE: u32 = 1;
const EFI_PXE_TFTP_READ_FILE: u32 = 2;

/// `EFI_PXE_BASE_CODE_PROTOCOL`, laid out through `Mode` (the last member we read). Unused
/// entries stay opaque so every member we do touch keeps its spec-defined offset.
#[repr(C)]
struct EfiPxeBaseCodeProtocol {
    revision: u64,
    start: *const c_void,
    stop: *const c_void,
    dhcp: *const c_void,
    discover: *const c_void,
    mtftp: EfiPxeBaseCodeMtftp,
    udp_write: *const c_void,
    udp_read: *const c_void,
    set_ip_filter: *const c_void,
    arp: *const c_void,
    set_parameters: *const c_void,
    set_station_ip: *const c_void,
    set_packets: *const c_void,
    mode: *mut EfiPxeBaseCodeMode,
}

/// Prefix of `EFI_PXE_BASE_CODE_MODE` up to the DHCP packets we read the TFTP server address
/// from. The IP fields are `[u32; 4]` so the struct carries `EFI_IP_ADDRESS`'s 4-byte alignment
/// and the explicit pad reproduces the C layout (17 BOOLEANs + TTL + ToS = 19 bytes, padded to
/// 20). Trailing members are omitted: nothing after `proxy_offer` is read.
#[repr(C)]
struct EfiPxeBaseCodeMode {
    started: bool,
    ipv6_available: bool,
    ipv6_supported: bool,
    using_ipv6: bool,
    bis_supported: bool,
    bis_detected: bool,
    auto_arp: bool,
    send_guid: bool,
    dhcp_discover_valid: bool,
    dhcp_ack_received: bool,
    proxy_offer_received: bool,
    pxe_discover_valid: bool,
    pxe_reply_received: bool,
    pxe_bis_reply_received: bool,
    icmp_error_received: bool,
    tftp_error_received: bool,
    make_callbacks: bool,
    ttl: u8,
    tos: u8,
    _pad: u8,
    station_ip: [u32; 4],
    subnet_mask: [u32; 4],
    dhcp_discover: [u8; 1472],
    dhcp_ack: [u8; 1472],
    proxy_offer: [u8; 1472],
}

/// Byte offset of `BootpSiAddr` (the "next server" address) inside an
/// `EFI_PXE_BASE_CODE_DHCPV4_PACKET`: opcode/hwtype/hwaddrlen/gatehops (4) + ident (4) +
/// seconds/flags (4) + ciaddr (4) + yiaddr (4).
const DHCPV4_SIADDR_OFFSET: usize = 20;

#[repr(C)]
struct EfiFileProtocol {
    revision: u64,
    open:
        extern "efiapi" fn(*mut Self, *mut *mut EfiFileProtocol, *const u16, u64, u64) -> EfiStatus,
    close: extern "efiapi" fn(*mut Self) -> EfiStatus,
    delete: *const c_void,
    read: extern "efiapi" fn(*mut Self, *mut usize, *mut c_void) -> EfiStatus,
    write: *const c_void,
    get_position: *const c_void,
    set_position: *const c_void,
    get_info: extern "efiapi" fn(*mut Self, *const EfiGuid, *mut usize, *mut c_void) -> EfiStatus,
    set_info: *const c_void,
    flush: *const c_void,
}

#[repr(C)]
struct EfiGraphicsOutputProtocol {
    query_mode: *const c_void,
    set_mode: *const c_void,
    blt: *const c_void,
    mode: *mut EfiGraphicsOutputProtocolMode,
}

#[repr(C)]
struct EfiGraphicsOutputProtocolMode {
    max_mode: u32,
    mode: u32,
    info: *mut EfiGraphicsOutputModeInformation,
    size_of_info: usize,
    frame_buffer_base: u64,
    frame_buffer_size: usize,
}

#[repr(C)]
struct EfiGraphicsOutputModeInformation {
    version: u32,
    horizontal_resolution: u32,
    vertical_resolution: u32,
    pixel_format: u32,
    pixel_information: [u32; 4],
    pixels_per_scan_line: u32,
}

// === UTF-16 console ==========================================================

/// A fixed-capacity UTF-16 line buffer implementing [`fmt::Write`], so `kprint!`
/// formats directly into the words a UEFI `ConOut` consumes without any heap.
/// Overflow truncates rather than panics — the log must never fault.
struct Utf16Line {
    data: [u16; Self::CAP],
    len: usize,
}

impl Utf16Line {
    const CAP: usize = 256;

    fn new() -> Self {
        Self {
            data: [0; Self::CAP],
            len: 0,
        }
    }

    fn terminated(&mut self) -> &[u16] {
        let end = if self.len < Self::CAP {
            self.len
        } else {
            Self::CAP - 1
        };
        self.data[end] = 0;
        &self.data[..=end]
    }
}

impl fmt::Write for Utf16Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut units = [0u16; 2];
        for ch in s.chars() {
            for &unit in ch.encode_utf16(&mut units).iter() {
                if self.len < Self::CAP - 1 {
                    self.data[self.len] = unit;
                    self.len += 1;
                }
            }
        }
        Ok(())
    }
}

fn print_line(con_out: *mut EfiSimpleTextOutputProtocol, args: fmt::Arguments<'_>) {
    let mut line = Utf16Line::new();
    let _ = fmt::Write::write_fmt(&mut line, args);
    put_utf16(con_out, line.terminated());
}

macro_rules! kprint {
    ($con:expr, $($arg:tt)*) => {
        print_line($con, core::format_args!($($arg)*))
    };
}

fn put_utf16(con_out: *mut EfiSimpleTextOutputProtocol, text: &[u16]) {
    if con_out.is_null() {
        return;
    }
    unsafe {
        ((*con_out).output_string)(con_out, text.as_ptr());
    }
}

fn fb_format_name(format: FramebufferFormat) -> &'static str {
    match format {
        FramebufferFormat::Rgb => "RGB",
        FramebufferFormat::Bgr => "BGR",
        FramebufferFormat::Unknown => "unknown",
    }
}

/// Convert an ASCII string into a NUL-terminated UTF-16 stack array for UEFI
/// file paths. Non-ASCII input is not expected here (paths are fixed consts).
fn ascii_to_utf16<const N: usize>(s: &str) -> [u16; N] {
    let mut out = [0u16; N];
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && i + 1 < N {
        out[i] = bytes[i] as u16;
        i += 1;
    }
    out
}

// === Entry point =============================================================

#[no_mangle]
pub extern "efiapi" fn efi_main(
    image_handle: EfiHandle,
    system_table: *mut EfiSystemTable,
) -> EfiStatus {
    let con_out = unsafe { con_out_of(system_table) };

    kprint!(con_out, "\r\nNIJIGUMO Boot Loader v0.1.0\r\n");
    kprint!(con_out, "BOOTAA64.EFI loaded\r\n");
    // Report the Exception Level before handoff. EDK2-on-RPi enters the OS at EL2 while
    // the X13s firmware enters at EL1; the kernel's high-VA TTBR1 jump assumes EL1, so an
    // EL2 handoff faults at the `br` with no console up yet. Make the level visible here,
    // over the firmware serial that still works.
    #[cfg(target_arch = "aarch64")]
    kprint!(
        con_out,
        "exception lvl: EL{} (kernel handoff assumes EL1)\r\n",
        current_el()
    );

    // On success this never returns — it jumps to the kernel. It returns only when
    // the machine cannot be brought up far enough to hand off; then we keep the
    // first-light text readable and fall back to the firmware.
    unsafe { boot_and_jump(image_handle, system_table, con_out) };

    kprint!(con_out, "\r\nPress any key to return to firmware.\r\n");
    wait_for_key(system_table);

    EFI_SUCCESS
}

unsafe fn con_out_of(system_table: *mut EfiSystemTable) -> *mut EfiSimpleTextOutputProtocol {
    if system_table.is_null() {
        return ptr::null_mut();
    }
    unsafe { (*system_table).con_out }
}

/// Current Exception Level from `CurrentEL[3:2]` (readable at EL1/EL2/EL3). Used only to
/// report the firmware handoff level; the kernel's high-VA jump requires EL1.
#[cfg(target_arch = "aarch64")]
fn current_el() -> u64 {
    let el: u64;
    unsafe {
        core::arch::asm!(
            "mrs {el}, CurrentEL",
            el = out(reg) el,
            options(nomem, nostack, preserves_flags),
        );
    }
    (el >> 2) & 0x3
}

struct KernelLoad {
    entry_virt: u64,
    phys: Range,
    virt: Range,
    boot_ttbr1: u64,
}

/// Discover the platform, load the kernel + initrd + DTB, assemble & validate the
/// `BootInfo`, exit boot services, and jump to the kernel. Returns only on a
/// failure that leaves us unable to hand off (boot services still active).
unsafe fn boot_and_jump(
    image_handle: EfiHandle,
    system_table: *mut EfiSystemTable,
    con_out: *mut EfiSimpleTextOutputProtocol,
) {
    let boot_services = if system_table.is_null() {
        ptr::null_mut()
    } else {
        unsafe { (*system_table).boot_services }
    };
    if boot_services.is_null() {
        kprint!(con_out, "boot services: UNAVAILABLE - cannot probe\r\n");
        return;
    }

    kprint!(con_out, "\r\n--- platform discovery ---\r\n");

    let root = match unsafe { open_payload_source(image_handle, boot_services) } {
        Some(source) => {
            match source {
                PayloadSource::Esp(_) => kprint!(con_out, "payloads     : ESP filesystem\r\n"),
                PayloadSource::Tftp { server_ip, .. } => kprint!(
                    con_out,
                    "payloads     : TFTP from {}.{}.{}.{}\r\n",
                    server_ip[0],
                    server_ip[1],
                    server_ip[2],
                    server_ip[3]
                ),
            }
            source
        }
        None => {
            kprint!(
                con_out,
                "boot volume  : UNAVAILABLE - no ESP filesystem and no PXE TFTP source\r\n"
            );
            return;
        }
    };

    let framebuffer = unsafe { discover_framebuffer(boot_services, con_out) };
    let (dtb_addr, _dtb_len) = unsafe { load_dtb(boot_services, root, con_out, system_table) };
    let kernel = unsafe { load_kernel(boot_services, root, con_out) };
    let (initrd, _initrd_buf) = unsafe { load_initrd(boot_services, root, con_out) };

    let kernel = match kernel {
        Some(kernel) => kernel,
        None => {
            kprint!(
                con_out,
                "\r\nNijigumo cannot continue without a kernel; returning to firmware.\r\n"
            );
            return;
        }
    };

    let bufs = match unsafe { prepare_memory_buffers(boot_services) } {
        Some(bufs) => bufs,
        None => {
            kprint!(
                con_out,
                "memory map   : UNAVAILABLE; returning to firmware.\r\n"
            );
            return;
        }
    };

    let boot_ptr = match unsafe { alloc_bootinfo(boot_services) } {
        Some(ptr) => ptr,
        None => {
            kprint!(
                con_out,
                "bootinfo     : alloc failed; returning to firmware.\r\n"
            );
            return;
        }
    };

    let seed = UefiHandoffSeed {
        dtb: dtb_addr,
        framebuffer,
        kernel_phys: kernel.phys,
        kernel_virt: kernel.virt,
        initrd,
        ..UefiHandoffSeed::empty()
    };
    unsafe { boot_ptr.write(build_boot_info(seed)) };

    // Stamp fixed-size boot policy from the staged manifest: board identity plus an optional,
    // explicit PL011 route. The latter is how a Pi 5 image can follow firmware's current RP1
    // PCIe window without pretending that a movable BAR address is a stable BSP fact.
    unsafe { stamp_boot_config(boot_services, root, con_out, boot_ptr) };

    // Pre-exit summary + validation, using a provisional memory snapshot.
    if let Some((regions_ptr, regions_len, _key, _raw_map)) =
        unsafe { snapshot_memory(boot_services, &bufs) }
    {
        unsafe { (*boot_ptr).mem_regions = RawSlice::from_raw_parts(regions_ptr, regions_len) };
    }
    let discovery = unsafe { summarize_platform(&*boot_ptr) };
    kprint!(con_out, "\r\nBootInfo ABI : v{}\r\n", unsafe {
        (*boot_ptr).version
    });
    kprint!(
        con_out,
        "memory       : {} regions, {} MiB usable / {} MiB total\r\n",
        discovery.region_count,
        discovery.usable_bytes >> 20,
        discovery.total_bytes >> 20
    );
    kprint!(
        con_out,
        "kernel image : phys {:#x}, virt {:#x}, entry {:#x}\r\n",
        kernel.phys.start,
        kernel.virt.start,
        kernel.entry_virt
    );
    kprint!(
        con_out,
        "initrd       : {}\r\n",
        if discovery.has_initrd {
            "present"
        } else {
            "absent"
        }
    );
    kprint!(con_out, "handoff DTB  : {:#018x}\r\n", unsafe {
        (*boot_ptr).platform.dtb
    });
    kprint!(
        con_out,
        "handoff UART : {:#018x}{}\r\n",
        unsafe { (*boot_ptr).platform.pl011_console_base },
        if unsafe { (*boot_ptr).platform.pl011_console_base } == 0 {
            " (BSP default)"
        } else {
            " (PL011 override)"
        }
    );
    match validate_boot_info(unsafe { &*boot_ptr }) {
        Ok(_) => kprint!(con_out, "handoff      : VALID\r\n"),
        Err(err) => {
            kprint!(
                con_out,
                "handoff      : INVALID {:?} - returning to firmware\r\n",
                err
            );
            return;
        }
    }

    // Optional pre-handoff pause. Works on any board because it uses the firmware's
    // own console input (the X13s USB keyboard included), and times out so automated
    // boots are not blocked.
    unsafe { prompt_pause(system_table, boot_services, con_out) };

    kprint!(
        con_out,
        "\r\nLoading MUREX => {:#x}...\r\n",
        kernel.entry_virt
    );

    // ExitBootServices: refill mem_regions from the FINAL map, then exit on the key
    // that map produced. Retry if the map shifts under us.
    let mut exited = false;
    for _ in 0..8 {
        let (regions_ptr, regions_len, map_key, _raw_map) =
            match unsafe { snapshot_memory(boot_services, &bufs) } {
                Some(snap) => snap,
                None => break,
            };
        unsafe { (*boot_ptr).mem_regions = RawSlice::from_raw_parts(regions_ptr, regions_len) };
        let status = unsafe { ((*boot_services).exit_boot_services)(image_handle, map_key) };
        if status == EFI_SUCCESS {
            exited = true;
            break;
        }
    }
    if !exited {
        kprint!(
            con_out,
            "ExitBootServices failed; returning to firmware.\r\n"
        );
        return;
    }

    // Boot services are gone — no more UEFI console. The kernel's Stage-A console takes
    // over. Make the freshly-copied kernel code coherent for instruction fetch (clean
    // D-cache to PoC, invalidate I-cache), then branch to entry with x0 = BootInfo*.
    unsafe {
        jump_to_kernel(
            kernel.entry_virt,
            kernel.boot_ttbr1,
            boot_ptr,
            kernel.phys.start,
            kernel.phys.len,
        )
    }
}

/// Where Nijigumo reads its kernel, initrd, and DTB from. A disk boot exposes an ESP through
/// `SimpleFileSystem`; a PXE boot has no filesystem at all — its device handle carries
/// `PXE_BASE_CODE` instead, and the payloads come back over TFTP from the server that just sent
/// us. Both are addressed with the same `\EFI\KUMO\...` paths.
#[derive(Clone, Copy)]
enum PayloadSource {
    Esp(*mut EfiFileProtocol),
    Tftp {
        pxe: *mut EfiPxeBaseCodeProtocol,
        server_ip: [u8; 16],
    },
}

/// Open the boot device's volume root (`LoadedImage -> SimpleFileSystem`), or fall back to the
/// PXE Base Code protocol on the same device handle when the image was netbooted.
unsafe fn open_payload_source(
    image_handle: EfiHandle,
    boot_services: *mut EfiBootServices,
) -> Option<PayloadSource> {
    let mut loaded_image: *mut EfiLoadedImageProtocol = ptr::null_mut();
    let status = unsafe {
        ((*boot_services).handle_protocol)(
            image_handle,
            &LOADED_IMAGE_GUID,
            &mut loaded_image as *mut _ as *mut *mut c_void,
        )
    };
    if status != EFI_SUCCESS || loaded_image.is_null() {
        return None;
    }

    let device_handle = unsafe { (*loaded_image).device_handle };
    let mut fs: *mut EfiSimpleFileSystemProtocol = ptr::null_mut();
    let status = unsafe {
        ((*boot_services).handle_protocol)(
            device_handle,
            &SIMPLE_FS_GUID,
            &mut fs as *mut _ as *mut *mut c_void,
        )
    };
    if status == EFI_SUCCESS && !fs.is_null() {
        let mut root: *mut EfiFileProtocol = ptr::null_mut();
        let status = unsafe { ((*fs).open_volume)(fs, &mut root) };
        if status == EFI_SUCCESS && !root.is_null() {
            return Some(PayloadSource::Esp(root));
        }
    }

    unsafe { open_tftp_source(device_handle, boot_services) }
}

/// Build a TFTP payload source from the PXE Base Code protocol on `device_handle`.
///
/// The server address is taken from the DHCP exchange the firmware already completed: `DhcpAck`'s
/// `BootpSiAddr` names the boot server, and under proxyDHCP (the KUMO pxehost setup) it is the
/// `ProxyOffer` that carries it instead — so fall through to that when the ack's field is zero.
unsafe fn open_tftp_source(
    device_handle: EfiHandle,
    boot_services: *mut EfiBootServices,
) -> Option<PayloadSource> {
    let mut pxe: *mut EfiPxeBaseCodeProtocol = ptr::null_mut();
    let status = unsafe {
        ((*boot_services).handle_protocol)(
            device_handle,
            &PXE_BASE_CODE_GUID,
            &mut pxe as *mut _ as *mut *mut c_void,
        )
    };
    if status != EFI_SUCCESS || pxe.is_null() {
        return None;
    }
    let mode = unsafe { (*pxe).mode };
    if mode.is_null() {
        return None;
    }

    // Order matters under proxyDHCP, which is how the KUMO pxehost setup works: the network's
    // real DHCP server answers the lease (and puts ITS OWN address in the ack's `siaddr`), while a
    // separate proxy names the boot server. Observed on this board: ack said 192.168.0.1 (the
    // router) but the firmware booted from 192.168.0.108 (pxehost). So the proxy offer wins
    // whenever the firmware recorded one; the ack is only the fallback for a plain DHCP+TFTP
    // server that serves both roles.
    let mut server_ip = [0u8; 16];
    let mut found = false;
    let proxy_received = unsafe { (*mode).proxy_offer_received };
    let mut candidates: [*const u8; 2] = [ptr::null(), ptr::null()];
    if proxy_received {
        candidates[0] = unsafe { ptr::addr_of!((*mode).proxy_offer) } as *const u8;
        candidates[1] = unsafe { ptr::addr_of!((*mode).dhcp_ack) } as *const u8;
    } else {
        candidates[0] = unsafe { ptr::addr_of!((*mode).dhcp_ack) } as *const u8;
        candidates[1] = unsafe { ptr::addr_of!((*mode).proxy_offer) } as *const u8;
    }
    for bytes in candidates {
        if bytes.is_null() {
            continue;
        }
        let mut candidate = [0u8; 4];
        for (index, slot) in candidate.iter_mut().enumerate() {
            *slot = unsafe { *bytes.add(DHCPV4_SIADDR_OFFSET + index) };
        }
        if candidate != [0, 0, 0, 0] {
            server_ip[..4].copy_from_slice(&candidate);
            found = true;
            break;
        }
    }
    if !found {
        return None;
    }
    Some(PayloadSource::Tftp { pxe, server_ip })
}

/// Rewrite an ESP path (`\EFI\KUMO\initrd.img`) as the TFTP filename the server expects
/// (`EFI/KUMO/initrd.img`): drop the leading separator and use forward slashes. Returns a
/// NUL-terminated ASCII buffer; a path that does not fit is rejected rather than truncated.
fn tftp_filename<const N: usize>(path: &str) -> Option<[u8; N]> {
    let mut out = [0u8; N];
    let mut len = 0usize;
    for byte in path.bytes() {
        let byte = if byte == b'\\' { b'/' } else { byte };
        if len == 0 && byte == b'/' {
            continue;
        }
        if len + 1 >= N {
            return None;
        }
        out[len] = byte;
        len += 1;
    }
    (len > 0).then_some(out)
}

/// Read a whole file over TFTP into a caller-chosen allocation. `pages` selects `AllocatePages`
/// (page-aligned, for the initrd) over `AllocatePool`. Returns `(buffer, len)`.
unsafe fn read_tftp_file(
    boot_services: *mut EfiBootServices,
    pxe: *mut EfiPxeBaseCodeProtocol,
    server_ip: &[u8; 16],
    path: &str,
    pages: bool,
) -> Option<(*mut c_void, usize)> {
    let name = tftp_filename::<128>(path)?;

    // Ask the server for the size first: the read needs a buffer big enough for the whole file,
    // and a short buffer is a failed transfer rather than a partial one.
    let mut size: u64 = 0;
    let status = unsafe {
        ((*pxe).mtftp)(
            pxe,
            EFI_PXE_TFTP_GET_FILE_SIZE,
            ptr::null_mut(),
            false,
            &mut size,
            ptr::null_mut(),
            server_ip,
            name.as_ptr(),
            ptr::null(),
            false,
        )
    };
    if status != EFI_SUCCESS || size == 0 {
        return None;
    }

    let mut buffer: *mut c_void = ptr::null_mut();
    if pages {
        let mut address: u64 = 0;
        let page_count = size.div_ceil(EFI_PAGE_SIZE).max(1) as usize;
        let status = unsafe {
            ((*boot_services).allocate_pages)(
                EFI_ALLOCATE_ANY_PAGES,
                efi_memory_type::LOADER_DATA,
                page_count,
                &mut address,
            )
        };
        if status != EFI_SUCCESS || address == 0 {
            return None;
        }
        buffer = address as *mut c_void;
    } else {
        let status = unsafe {
            ((*boot_services).allocate_pool)(
                efi_memory_type::LOADER_DATA,
                size as usize,
                &mut buffer,
            )
        };
        if status != EFI_SUCCESS || buffer.is_null() {
            return None;
        }
    }

    let mut read_size = size;
    let status = unsafe {
        ((*pxe).mtftp)(
            pxe,
            EFI_PXE_TFTP_READ_FILE,
            buffer,
            false,
            &mut read_size,
            ptr::null_mut(),
            server_ip,
            name.as_ptr(),
            ptr::null(),
            false,
        )
    };
    if status != EFI_SUCCESS {
        if !pages {
            unsafe { ((*boot_services).free_pool)(buffer) };
        }
        return None;
    }
    Some((buffer, read_size as usize))
}

/// Open `path` on `root`, allocate a `LoaderData` pool, and read the whole file
/// into it. Returns `(buffer, len)`; the caller owns the pool allocation.
unsafe fn read_esp_file(
    boot_services: *mut EfiBootServices,
    root: PayloadSource,
    path: &str,
) -> Option<(*mut c_void, usize)> {
    let root = match root {
        PayloadSource::Esp(root) => root,
        PayloadSource::Tftp { pxe, server_ip } => {
            return unsafe { read_tftp_file(boot_services, pxe, &server_ip, path, false) };
        }
    };
    let path16 = ascii_to_utf16::<96>(path);
    let mut file: *mut EfiFileProtocol = ptr::null_mut();
    let status = unsafe { ((*root).open)(root, &mut file, path16.as_ptr(), EFI_FILE_MODE_READ, 0) };
    if status != EFI_SUCCESS || file.is_null() {
        return None;
    }

    let size = match unsafe { file_size_bytes(file) } {
        Some(size) => size,
        None => {
            unsafe { ((*file).close)(file) };
            return None;
        }
    };

    let mut buffer: *mut c_void = ptr::null_mut();
    let status = unsafe {
        ((*boot_services).allocate_pool)(efi_memory_type::LOADER_DATA, size as usize, &mut buffer)
    };
    if status != EFI_SUCCESS || buffer.is_null() {
        unsafe { ((*file).close)(file) };
        return None;
    }

    let mut read_size = size as usize;
    let status = unsafe { ((*file).read)(file, &mut read_size, buffer) };
    unsafe { ((*file).close)(file) };
    if status != EFI_SUCCESS {
        unsafe { ((*boot_services).free_pool)(buffer) };
        return None;
    }
    Some((buffer, read_size))
}

/// Like [`read_esp_file`], but the buffer comes from `AllocatePages` (`LoaderData`), so
/// it is **page-aligned** — required for handoffs the kernel republishes as physical
/// VMOs (the initrd). The pages stay allocated across `ExitBootServices`; the kernel's
/// memory plan excludes the range from the frame allocator.
unsafe fn read_esp_file_pages(
    boot_services: *mut EfiBootServices,
    root: PayloadSource,
    path: &str,
) -> Option<(*mut c_void, usize)> {
    let root = match root {
        PayloadSource::Esp(root) => root,
        PayloadSource::Tftp { pxe, server_ip } => {
            return unsafe { read_tftp_file(boot_services, pxe, &server_ip, path, true) };
        }
    };
    let path16 = ascii_to_utf16::<96>(path);
    let mut file: *mut EfiFileProtocol = ptr::null_mut();
    let status = unsafe { ((*root).open)(root, &mut file, path16.as_ptr(), EFI_FILE_MODE_READ, 0) };
    if status != EFI_SUCCESS || file.is_null() {
        return None;
    }

    let size = match unsafe { file_size_bytes(file) } {
        Some(size) => size,
        None => {
            unsafe { ((*file).close)(file) };
            return None;
        }
    };

    let pages = size.div_ceil(EFI_PAGE_SIZE).max(1) as usize;
    let mut base = 0u64;
    let status = unsafe {
        ((*boot_services).allocate_pages)(
            EFI_ALLOCATE_ANY_PAGES,
            efi_memory_type::LOADER_DATA,
            pages,
            &mut base,
        )
    };
    if status != EFI_SUCCESS || base == 0 {
        unsafe { ((*file).close)(file) };
        return None;
    }

    let mut read_size = size as usize;
    let status = unsafe { ((*file).read)(file, &mut read_size, base as *mut c_void) };
    unsafe { ((*file).close)(file) };
    if status != EFI_SUCCESS {
        // `free_pages` is untyped in our minimal bindings; a failed read here means the
        // boot is about to be declared degraded anyway, so the pages are left to the
        // firmware (reclaimed at ExitBootServices accounting like any LoaderData).
        return None;
    }
    Some((base as *mut c_void, read_size))
}

/// Read `EFI_FILE_INFO.FileSize` (at offset 8 of the structure).
unsafe fn file_size_bytes(file: *mut EfiFileProtocol) -> Option<u64> {
    let mut info = [0u8; 512];
    let mut info_size = info.len();
    let status = unsafe {
        ((*file).get_info)(
            file,
            &FILE_INFO_GUID,
            &mut info_size,
            info.as_mut_ptr() as *mut c_void,
        )
    };
    if status != EFI_SUCCESS || info_size < 16 {
        return None;
    }
    let mut file_size = [0u8; 8];
    file_size.copy_from_slice(&info[8..16]);
    Some(u64::from_le_bytes(file_size))
}

/// Read the staged boot manifest and stamp its fixed-size policy into `BootInfo`: the board id
/// and, when present, an explicit `console-uart = pl011@0x...` route. The image is built for one
/// board, but RP1's host address is a firmware-selected PCIe window rather than a stable board
/// constant, so that route must remain an explicit per-image override. — KESTREL 2026-07-17
///
/// Absent manifest, non-UTF-8 text, unparseable config, or no `board` key all leave the
/// field empty — the kernel reports "unstamped", which is the pre-existing behaviour. This
/// is deliberately not fatal: the board id is not yet load-bearing, and a boot that reaches
/// the kernel is worth more than one refused over a missing manifest.
unsafe fn stamp_boot_config(
    boot_services: *mut EfiBootServices,
    root: PayloadSource,
    con_out: *mut EfiSimpleTextOutputProtocol,
    boot_ptr: *mut BootInfo,
) {
    let Some((buffer, len)) = (unsafe { read_esp_file(boot_services, root, BOOT_CONFIG_ESP_PATH) })
    else {
        kprint!(con_out, "board        : unstamped (no manifest)\r\n");
        return;
    };
    let bytes = unsafe { core::slice::from_raw_parts(buffer as *const u8, len) };
    let config = core::str::from_utf8(bytes).ok().and_then(BootConfig::parse);
    let board = config.and_then(|cfg| cfg.board);
    match board {
        // `set_board_id` copies into BootInfo's fixed array, so the pool buffer need not
        // outlive this call.
        Some(board) => {
            unsafe { (*boot_ptr).set_board_id(board) };
            kprint!(con_out, "board        : {}\r\n", board);
        }
        None => kprint!(
            con_out,
            "board        : unstamped (manifest names no board)\r\n"
        ),
    }

    match config.and_then(|cfg| cfg.console_uart) {
        Some(value) => match parse_pl011_console(value) {
            Some(base) => {
                unsafe { (*boot_ptr).platform.pl011_console_base = base };
                kprint!(
                    con_out,
                    "console UART : PL011 override @ {:#018x}\r\n",
                    base
                );
            }
            None => kprint!(
                con_out,
                "console UART : rejected '{}' (using BSP default)\r\n",
                value
            ),
        },
        None => kprint!(con_out, "console UART : BSP default\r\n"),
    }
    unsafe { ((*boot_services).free_pool)(buffer) };
}

/// Resolve the DTB, returning `(phys_addr, len)` or `(0, 0)` if none is found.
///
/// Preference order: (1) a DTB staged on the ESP at one of [`DTB_ESP_PATHS`] — the X13s and
/// the Orange Pi 5 Plus ship their own, newer than the firmware's; (2) the firmware-provided
/// DTB from the UEFI configuration table ([`EFI_DTB_TABLE_GUID`]) — how the Raspberry Pi 5 and
/// other generic-UEFI boards supply their device tree, since they stage none on the ESP. The
/// kept pool buffer is handed to the kernel via BootInfo. A firmware-table blob is first copied
/// into `LoaderData`, so reclaiming firmware-owned pages after `ExitBootServices` cannot invalidate
/// the handoff before the kernel parses it. — KESTREL 2026-07-17
unsafe fn load_dtb(
    boot_services: *mut EfiBootServices,
    root: PayloadSource,
    con_out: *mut EfiSimpleTextOutputProtocol,
    system_table: *mut EfiSystemTable,
) -> (u64, u64) {
    // (1) A staged ESP DTB takes priority; an image stages at most one of these paths.
    for path in DTB_ESP_PATHS {
        let Some((buffer, len)) = (unsafe { read_esp_file(boot_services, root, path) }) else {
            continue;
        };
        let bytes = unsafe { core::slice::from_raw_parts(buffer as *const u8, len) };
        let declared_len = fdt_total_size(bytes).unwrap_or(0) as u64;
        if is_fdt_magic(bytes)
            && (40..=MAX_DTB_BYTES).contains(&declared_len)
            && declared_len <= len as u64
            && len as u64 <= MAX_DTB_BYTES
        {
            kprint!(
                con_out,
                "device tree  : {} @ {:#x} ({} declared / {} file bytes)\r\n",
                path,
                buffer as u64,
                declared_len,
                len
            );
            return (buffer as u64, declared_len);
        }
        kprint!(
            con_out,
            "device tree  : ESP FDT rejected (magic={} declared={} file={}) - trying next source\r\n",
            is_fdt_magic(bytes),
            declared_len,
            len
        );
        unsafe { ((*boot_services).free_pool)(buffer) };
    }

    // (2) Firmware configuration table (Pi 5 / generic UEFI).
    let probe_bufs = match unsafe { prepare_memory_buffers(boot_services) } {
        Some(bufs) => bufs,
        None => {
            kprint!(
                con_out,
                "device tree  : memory map unavailable; firmware DTB not dereferenced\r\n"
            );
            return (0, 0);
        }
    };
    let firmware_dtb = match unsafe { snapshot_memory(boot_services, &probe_bufs) } {
        Some((_regions_ptr, _regions_len, _, raw_map)) => unsafe {
            firmware_dtb(system_table, con_out, raw_map)
        },
        None => {
            kprint!(
                con_out,
                "device tree  : memory map snapshot failed; firmware DTB not dereferenced\r\n"
            );
            None
        }
    };
    if let Some((addr, len)) = firmware_dtb {
        let mut owned: *mut c_void = ptr::null_mut();
        let status = unsafe {
            ((*boot_services).allocate_pool)(efi_memory_type::LOADER_DATA, len as usize, &mut owned)
        };
        if status != EFI_SUCCESS || owned.is_null() {
            kprint!(
                con_out,
                "device tree  : firmware DTB copy failed; rejecting handoff\r\n"
            );
            unsafe { release_memory_buffers(boot_services, probe_bufs) };
            return (0, 0);
        }
        if !is_plausible_firmware_pointer(owned as u64, len, 8)
            || ranges_overlap(addr, len, owned as u64, len)
        {
            kprint!(
                con_out,
                "device tree  : firmware DTB copy destination rejected\r\n"
            );
            unsafe { ((*boot_services).free_pool)(owned) };
            unsafe { release_memory_buffers(boot_services, probe_bufs) };
            return (0, 0);
        }
        unsafe { ptr::copy_nonoverlapping(addr as *const u8, owned as *mut u8, len as usize) };
        unsafe { release_memory_buffers(boot_services, probe_bufs) };
        kprint!(
            con_out,
            "device tree  : copied firmware DTB {:#x} -> LoaderData {:#x} ({} bytes)\r\n",
            addr,
            owned as u64,
            len
        );
        return (owned as u64, len);
    }
    unsafe { release_memory_buffers(boot_services, probe_bufs) };

    kprint!(con_out, "device tree  : absent\r\n");
    (0, 0)
}

/// Scan the UEFI configuration table for [`EFI_DTB_TABLE_GUID`] and, if its blob carries valid
/// FDT magic, return `(phys_addr, totalsize)` read from the FDT header. The firmware owns this
/// memory; the caller must copy it into loader-owned storage before `ExitBootServices`.
unsafe fn firmware_dtb(
    system_table: *mut EfiSystemTable,
    con_out: *mut EfiSimpleTextOutputProtocol,
    memory_map: EfiMemoryMapView,
) -> Option<(u64, u64)> {
    const MAX_CONFIG_TABLES: usize = 1024;

    if system_table.is_null() {
        kprint!(con_out, "EFI tables   : system table is null\r\n");
        return None;
    }
    let count = unsafe { (*system_table).number_of_table_entries };
    let tables = unsafe { (*system_table).configuration_table } as *const EfiConfigurationTable;
    kprint!(
        con_out,
        "EFI tables   : count={} base={:#018x}\r\n",
        count,
        tables as u64
    );
    let table_bytes = count.checked_mul(size_of::<EfiConfigurationTable>());
    if count > MAX_CONFIG_TABLES
        || table_bytes.is_none()
        || !is_plausible_firmware_pointer(
            tables as u64,
            table_bytes.unwrap_or(0) as u64,
            core::mem::align_of::<EfiConfigurationTable>() as u64,
        )
        || !unsafe {
            memory_map.contains_firmware_owned_range(tables as u64, table_bytes.unwrap_or(0) as u64)
        }
    {
        kprint!(
            con_out,
            "EFI tables   : rejected count/base before scan\r\n"
        );
        return None;
    }
    for index in 0..count {
        let entry = unsafe { &*tables.add(index) };
        if entry.vendor_guid != EFI_DTB_TABLE_GUID {
            continue;
        }
        let ptr = entry.vendor_table as *const u8;
        kprint!(
            con_out,
            "EFI DTB match: index={} vendor_table={:#018x}\r\n",
            index,
            ptr as u64
        );
        if !is_plausible_firmware_pointer(ptr as u64, 8, 8)
            || !unsafe { memory_map.contains_firmware_owned_range(ptr as u64, 8) }
        {
            kprint!(
                con_out,
                "EFI DTB hdr : pointer rejected before dereference\r\n"
            );
            continue;
        }
        // Read the 8-byte FDT header (magic + totalsize) to validate and size the blob.
        let head = unsafe { core::slice::from_raw_parts(ptr, 8) };
        let magic = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
        let len = fdt_total_size(head).unwrap_or(0) as u64;
        kprint!(
            con_out,
            "EFI DTB hdr : {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x}  magic={:#010x} size={}\r\n",
            head[0],
            head[1],
            head[2],
            head[3],
            head[4],
            head[5],
            head[6],
            head[7],
            magic,
            len
        );
        if !is_fdt_magic(head) {
            kprint!(con_out, "EFI DTB hdr : bad FDT magic; rejected\r\n");
            continue;
        }
        if !(40..=MAX_DTB_BYTES).contains(&len) {
            kprint!(
                con_out,
                "EFI DTB hdr : implausible totalsize {}; rejected\r\n",
                len
            );
            continue;
        }
        if !is_plausible_firmware_pointer(ptr as u64, len, 8)
            || !unsafe { memory_map.contains_firmware_owned_range(ptr as u64, len) }
        {
            kprint!(
                con_out,
                "EFI DTB blob: complete range is outside readable memory; rejected\r\n"
            );
            continue;
        }
        kprint!(
            con_out,
            "device tree  : firmware table @ {:#x} ({} bytes)\r\n",
            ptr as u64,
            len
        );
        return Some((ptr as u64, len));
    }
    None
}

const PT_DESC_VALID: u64 = 1 << 0;
const PT_DESC_TABLE_OR_PAGE: u64 = 1 << 1;
const PT_DESC_AF: u64 = 1 << 10;
const PT_DESC_SH_INNER: u64 = 3 << 8;
const PT_DESC_UXN: u64 = 1 << 54;
const PT_ADDR_MASK: u64 = 0x0000_ffff_ffff_f000;
const BOOT_MAIR_WB_INDEX: u64 = 7;

struct BootPageTables {
    next: u64,
    end: u64,
}

impl BootPageTables {
    unsafe fn alloc(&mut self) -> Option<u64> {
        if self.next >= self.end {
            return None;
        }
        let page = self.next;
        self.next += EFI_PAGE_SIZE;
        unsafe { ptr::write_bytes(page as *mut u8, 0, EFI_PAGE_SIZE as usize) };
        Some(page)
    }
}

unsafe fn table_desc(table: u64, index: usize) -> u64 {
    unsafe { ((table + index as u64 * 8) as *const u64).read_volatile() }
}

unsafe fn write_table_desc(table: u64, index: usize, desc: u64) {
    unsafe { ((table + index as u64 * 8) as *mut u64).write_volatile(desc) };
}

unsafe fn ensure_boot_table(tables: &mut BootPageTables, parent: u64, index: usize) -> Option<u64> {
    let existing = unsafe { table_desc(parent, index) };
    if existing & PT_DESC_VALID != 0 {
        return Some(existing & PT_ADDR_MASK);
    }
    let child = unsafe { tables.alloc()? };
    unsafe { write_table_desc(parent, index, child | PT_DESC_VALID | PT_DESC_TABLE_OR_PAGE) };
    Some(child)
}

/// Construct the temporary TTBR1 tree used only for the first virtual jump into the
/// high-linked kernel. The kernel replaces this tree with its permanent map during M1.
unsafe fn build_boot_ttbr1(
    boot_services: *mut EfiBootServices,
    virt_base: u64,
    phys_base: u64,
    len: u64,
) -> Option<u64> {
    if len == 0 || virt_base & (EFI_PAGE_SIZE - 1) != 0 || phys_base & (EFI_PAGE_SIZE - 1) != 0 {
        return None;
    }

    let mapped_pages = len.div_ceil(EFI_PAGE_SIZE) as usize;
    // One root plus L1/L2 slack and one L3 table per 2 MiB of image.
    let table_pages = 4usize.saturating_add(mapped_pages.div_ceil(512));
    let mut table_base = 0u64;
    let status = unsafe {
        ((*boot_services).allocate_pages)(
            EFI_ALLOCATE_ANY_PAGES,
            efi_memory_type::LOADER_DATA,
            table_pages,
            &mut table_base,
        )
    };
    if status != EFI_SUCCESS {
        return None;
    }

    unsafe {
        ptr::write_bytes(
            table_base as *mut u8,
            0,
            table_pages * EFI_PAGE_SIZE as usize,
        )
    };
    let root = table_base;
    let mut tables = BootPageTables {
        next: table_base + EFI_PAGE_SIZE,
        end: table_base + table_pages as u64 * EFI_PAGE_SIZE,
    };

    for page in 0..mapped_pages {
        let va = virt_base + page as u64 * EFI_PAGE_SIZE;
        let pa = phys_base + page as u64 * EFI_PAGE_SIZE;
        let l1 = unsafe { ensure_boot_table(&mut tables, root, ((va >> 39) & 0x1ff) as usize)? };
        let l2 = unsafe { ensure_boot_table(&mut tables, l1, ((va >> 30) & 0x1ff) as usize)? };
        let l3 = unsafe { ensure_boot_table(&mut tables, l2, ((va >> 21) & 0x1ff) as usize)? };
        let desc = pa
            | PT_DESC_VALID
            | PT_DESC_TABLE_OR_PAGE
            | PT_DESC_AF
            | PT_DESC_SH_INNER
            | (BOOT_MAIR_WB_INDEX << 2)
            | PT_DESC_UXN;
        unsafe { write_table_desc(l3, ((va >> 12) & 0x1ff) as usize, desc) };
    }

    Some(root)
}

/// Load the high-linked kernel ELF into arbitrary physical pages, copy each `PT_LOAD`
/// segment (zeroing BSS), and construct the temporary TTBR1 map used for entry.
unsafe fn load_kernel(
    boot_services: *mut EfiBootServices,
    root: PayloadSource,
    con_out: *mut EfiSimpleTextOutputProtocol,
) -> Option<KernelLoad> {
    let (buffer, len) = match unsafe { read_esp_file(boot_services, root, KERNEL_ESP_PATH) } {
        Some(file) => file,
        None => {
            kprint!(
                con_out,
                "kernel       : absent (cannot read {})\r\n",
                KERNEL_ESP_PATH
            );
            return None;
        }
    };

    let bytes = unsafe { core::slice::from_raw_parts(buffer as *const u8, len) };
    let image = match parse_elf64(bytes) {
        Ok(image) => image,
        Err(err) => {
            kprint!(con_out, "kernel       : invalid ELF ({:?})\r\n", err);
            unsafe { ((*boot_services).free_pool)(buffer) };
            return None;
        }
    };
    if image.machine != EM_AARCH64 {
        kprint!(
            con_out,
            "kernel       : wrong machine {:#x}\r\n",
            image.machine
        );
        unsafe { ((*boot_services).free_pool)(buffer) };
        return None;
    }

    // Let the firmware pick a free physical region (boards differ wildly: QEMU virt
    // RAM is at 0x40000000, the X13s puts conventional memory up at 0xe0000000+).
    // Linked virtual addresses remain fixed; physical placement changes only the
    // backing pages installed into the temporary TTBR1 map.
    let pages = ((image.load_span() + EFI_PAGE_SIZE - 1) / EFI_PAGE_SIZE) as usize;
    let mut load_addr: u64 = 0;
    // Allocate the kernel into EfiLoaderCode, not EfiLoaderData: strict UEFI (Qualcomm's
    // SC8280XP firmware) maps loader *data* pages non-executable, so a `br` into the
    // copied kernel faults on instruction fetch and never runs its first instruction —
    // the X13s "red flash, no green" symptom. Lax firmware (the Pi's EDK2, QEMU/AAVMF)
    // maps data executable and masks the bug. EfiLoaderCode is what GRUB and the Linux
    // EFI stub use for loaded kernels.
    let status = unsafe {
        ((*boot_services).allocate_pages)(
            EFI_ALLOCATE_ANY_PAGES,
            efi_memory_type::LOADER_CODE,
            pages,
            &mut load_addr,
        )
    };
    if status != EFI_SUCCESS {
        kprint!(
            con_out,
            "kernel       : AllocatePages ({} pages) failed ({:#x})\r\n",
            pages,
            status
        );
        unsafe { ((*boot_services).free_pool)(buffer) };
        return None;
    }

    if image.load_span() != image.virt_span() {
        kprint!(
            con_out,
            "kernel       : physical/virtual spans differ ({:#x}/{:#x})\r\n",
            image.load_span(),
            image.virt_span()
        );
        unsafe { ((*boot_services).free_pool)(buffer) };
        return None;
    }

    let phys_delta = load_addr.wrapping_sub(image.load_base);

    for segment in image.segments() {
        let phys_offset = segment.phys_addr.wrapping_sub(image.load_base);
        let virt_offset = segment.virt_addr.wrapping_sub(image.virt_base);
        if phys_offset != virt_offset {
            kprint!(
                con_out,
                "kernel       : PT_LOAD physical/virtual layout mismatch\r\n"
            );
            unsafe { ((*boot_services).free_pool)(buffer) };
            return None;
        }
        let src = unsafe { (buffer as *const u8).add(segment.file_offset as usize) };
        let dst = segment.phys_addr.wrapping_add(phys_delta) as *mut u8;
        unsafe { ptr::copy_nonoverlapping(src, dst, segment.file_size as usize) };
        let zero = segment.mem_size.saturating_sub(segment.file_size) as usize;
        if zero > 0 {
            unsafe { ptr::write_bytes(dst.add(segment.file_size as usize), 0, zero) };
        }
    }

    unsafe { ((*boot_services).free_pool)(buffer) };

    let entry_offset = match image.virt_offset(image.entry) {
        Some(offset) => offset,
        None => {
            kprint!(
                con_out,
                "kernel       : entry lies outside PT_LOAD span\r\n"
            );
            return None;
        }
    };
    let entry_phys = load_addr.wrapping_add(entry_offset);
    let boot_ttbr1 = match unsafe {
        build_boot_ttbr1(boot_services, image.virt_base, load_addr, image.virt_span())
    } {
        Some(root) => root,
        None => {
            kprint!(con_out, "kernel       : TTBR1 bootstrap map failed\r\n");
            return None;
        }
    };
    kprint!(
        con_out,
        "kernel       : {} phys {:#x}..{:#x} -> virt {:#x} entry {:#x} (trampoline {:#x})\r\n",
        KERNEL_ESP_PATH,
        load_addr,
        load_addr + image.load_span(),
        image.virt_base,
        image.entry,
        entry_phys
    );
    Some(KernelLoad {
        entry_virt: image.entry,
        phys: Range::new(load_addr, image.load_span()),
        virt: Range::new(image.virt_base, image.virt_span()),
        boot_ttbr1,
    })
}

/// Load an optional initrd from the ESP into **page-aligned** memory. The kernel turns
/// `BootInfo.initrd` into the first physical VMO (PLAN §8.4), and physical VMOs require
/// a page-aligned base — a pool allocation's `+0x18` would be rejected. Absence is
/// honest, not fatal.
unsafe fn load_initrd(
    boot_services: *mut EfiBootServices,
    root: PayloadSource,
    con_out: *mut EfiSimpleTextOutputProtocol,
) -> (Range, *mut c_void) {
    match unsafe { read_esp_file_pages(boot_services, root, INITRD_ESP_PATH) } {
        Some((buffer, len)) => {
            kprint!(
                con_out,
                "initrd       : {} @ {:#x} ({} bytes, page-aligned)\r\n",
                INITRD_ESP_PATH,
                buffer as u64,
                len
            );
            (Range::new(buffer as u64, len as u64), buffer)
        }
        None => {
            kprint!(
                con_out,
                "initrd       : absent ({} not staged)\r\n",
                INITRD_ESP_PATH
            );
            (Range::empty(), ptr::null_mut())
        }
    }
}

/// A pool allocation for the `BootInfo` so it survives ExitBootServices.
unsafe fn alloc_bootinfo(boot_services: *mut EfiBootServices) -> Option<*mut BootInfo> {
    let mut ptr: *mut c_void = ptr::null_mut();
    let status = unsafe {
        ((*boot_services).allocate_pool)(
            efi_memory_type::LOADER_DATA,
            size_of::<BootInfo>(),
            &mut ptr,
        )
    };
    if status != EFI_SUCCESS || ptr.is_null() {
        None
    } else {
        Some(ptr as *mut BootInfo)
    }
}

struct MemBufs {
    desc: *mut c_void,
    desc_cap: usize,
    regions: *mut MemRegion,
    region_cap: usize,
}

#[derive(Clone, Copy)]
struct EfiMemoryMapView {
    descriptors: *const u8,
    count: usize,
    descriptor_size: usize,
}

impl EfiMemoryMapView {
    /// Check a range against raw EFI types before the lossy `MemRegionKind` conversion. This keeps
    /// Conventional and Unusable pages distinct and rejects both before any raw read.
    /// — KESTREL 2026-07-17
    ///
    /// # Safety
    /// The descriptor buffer must remain live and contain `count` entries of `descriptor_size`.
    unsafe fn contains_firmware_owned_range(self, address: u64, byte_len: u64) -> bool {
        let Some(end) = address.checked_add(byte_len) else {
            return false;
        };
        if address >= end || self.descriptor_size < 32 {
            return false;
        }
        for index in 0..self.count {
            let descriptor = unsafe { self.descriptors.add(index * self.descriptor_size) };
            let efi_type = unsafe { ptr::read_unaligned(descriptor as *const u32) };
            if !efi_memory_type_is_firmware_owned(efi_type) {
                continue;
            }
            let start = unsafe { ptr::read_unaligned(descriptor.add(8) as *const u64) };
            let pages = unsafe { ptr::read_unaligned(descriptor.add(24) as *const u64) };
            let Some(region_len) = pages.checked_mul(EFI_PAGE_SIZE) else {
                continue;
            };
            let Some(region_end) = start.checked_add(region_len) else {
                continue;
            };
            if start <= address && end <= region_end {
                return true;
            }
        }
        false
    }
}

const fn ranges_overlap(a_start: u64, a_len: u64, b_start: u64, b_len: u64) -> bool {
    let Some(a_end) = a_start.checked_add(a_len) else {
        return true;
    };
    let Some(b_end) = b_start.checked_add(b_len) else {
        return true;
    };
    a_start < b_end && b_start < a_end
}

/// Release a temporary memory-map snapshot before the final handoff buffers are prepared.
unsafe fn release_memory_buffers(boot_services: *mut EfiBootServices, bufs: MemBufs) {
    unsafe { ((*boot_services).free_pool)(bufs.regions as *mut c_void) };
    unsafe { ((*boot_services).free_pool)(bufs.desc) };
}

/// Pre-allocate the descriptor + `MemRegion` buffers used to snapshot the memory
/// map. Must run *before* ExitBootServices, since no allocation is possible after.
unsafe fn prepare_memory_buffers(boot_services: *mut EfiBootServices) -> Option<MemBufs> {
    let mut map_size: usize = 0;
    let mut map_key: usize = 0;
    let mut desc_size: usize = 0;
    let mut desc_version: u32 = 0;
    let _ = unsafe {
        ((*boot_services).get_memory_map)(
            &mut map_size,
            ptr::null_mut(),
            &mut map_key,
            &mut desc_size,
            &mut desc_version,
        )
    };
    if map_size == 0 || desc_size == 0 {
        return None;
    }

    // Slack for the descriptor + region allocations we are about to make.
    let cap = map_size + desc_size * 8;
    let mut desc: *mut c_void = ptr::null_mut();
    let status = unsafe {
        ((*boot_services).allocate_pool)(efi_memory_type::BOOT_SERVICES_DATA, cap, &mut desc)
    };
    if status != EFI_SUCCESS || desc.is_null() {
        return None;
    }

    let region_cap = cap / desc_size;
    let mut regions: *mut c_void = ptr::null_mut();
    let status = unsafe {
        ((*boot_services).allocate_pool)(
            efi_memory_type::LOADER_DATA,
            region_cap * size_of::<MemRegion>(),
            &mut regions,
        )
    };
    if status != EFI_SUCCESS || regions.is_null() {
        unsafe { ((*boot_services).free_pool)(desc) };
        return None;
    }

    Some(MemBufs {
        desc,
        desc_cap: cap,
        regions: regions as *mut MemRegion,
        region_cap,
    })
}

/// Snapshot the current memory map into the prepared buffers, converting each
/// descriptor into a `MemRegion`. Returns `(regions_ptr, count, map_key, raw_map)`; the `map_key`
/// is what ExitBootServices must be called with, while `raw_map` preserves EFI memory types for
/// pre-dereference ownership checks.
unsafe fn snapshot_memory(
    boot_services: *mut EfiBootServices,
    bufs: &MemBufs,
) -> Option<(u64, u64, usize, EfiMemoryMapView)> {
    let mut map_size = bufs.desc_cap;
    let mut map_key: usize = 0;
    let mut desc_size: usize = 0;
    let mut desc_version: u32 = 0;
    let status = unsafe {
        ((*boot_services).get_memory_map)(
            &mut map_size,
            bufs.desc,
            &mut map_key,
            &mut desc_size,
            &mut desc_version,
        )
    };
    if status != EFI_SUCCESS || desc_size == 0 {
        return None;
    }

    let count = map_size / desc_size;
    let mut written = 0usize;
    let mut i = 0usize;
    while i < count && written < bufs.region_cap {
        let base = unsafe { (bufs.desc as *const u8).add(i * desc_size) };
        let efi_type = unsafe { ptr::read_unaligned(base as *const u32) };
        let phys = unsafe { ptr::read_unaligned(base.add(8) as *const u64) };
        let pages = unsafe { ptr::read_unaligned(base.add(24) as *const u64) };
        let region = MemRegion {
            range: Range::new(phys, pages.saturating_mul(EFI_PAGE_SIZE)),
            kind: mem_region_kind_from_efi(efi_type),
            _reserved: 0,
        };
        unsafe { bufs.regions.add(written).write(region) };
        written += 1;
        i += 1;
    }

    Some((
        bufs.regions as u64,
        written as u64,
        map_key,
        EfiMemoryMapView {
            descriptors: bufs.desc as *const u8,
            count,
            descriptor_size: desc_size,
        },
    ))
}

/// Locate the active GOP and read its framebuffer. Only a usable *linear* buffer
/// (nonzero base/size, RGB/BGR) is accepted; a BltOnly GOP is reported absent.
unsafe fn discover_framebuffer(
    boot_services: *mut EfiBootServices,
    con_out: *mut EfiSimpleTextOutputProtocol,
) -> Option<Framebuffer> {
    let mut gop: *mut EfiGraphicsOutputProtocol = ptr::null_mut();
    let status = unsafe {
        ((*boot_services).locate_protocol)(
            &GRAPHICS_OUTPUT_GUID,
            ptr::null_mut(),
            &mut gop as *mut _ as *mut *mut c_void,
        )
    };
    if status != EFI_SUCCESS || gop.is_null() {
        kprint!(
            con_out,
            "framebuffer  : absent (GOP locate {:#x})\r\n",
            status
        );
        return None;
    }

    let mode = unsafe { (*gop).mode };
    if mode.is_null() {
        kprint!(con_out, "framebuffer  : absent (GOP mode null)\r\n");
        return None;
    }

    let base = unsafe { (*mode).frame_buffer_base };
    let len = unsafe { (*mode).frame_buffer_size } as u64;
    let info = unsafe { (*mode).info };
    let (width, height, stride, pixel_format) = if info.is_null() {
        (0, 0, 0, efi_pixel_format::BLT_ONLY)
    } else {
        unsafe {
            (
                (*info).horizontal_resolution,
                (*info).vertical_resolution,
                (*info).pixels_per_scan_line,
                (*info).pixel_format,
            )
        }
    };

    let fb = framebuffer_from_gop(base, len, width, height, stride, pixel_format);
    if base == 0 || len == 0 || fb.format == FramebufferFormat::Unknown {
        kprint!(
            con_out,
            "framebuffer  : GOP {}x{} present but no linear buffer - absent\r\n",
            width,
            height
        );
        return None;
    }

    kprint!(
        con_out,
        "framebuffer  : {}x{} {} @ {:#x} ({} KiB)\r\n",
        fb.width,
        fb.height,
        fb_format_name(fb.format),
        fb.phys,
        fb.len >> 10
    );
    Some(fb)
}

/// Make the freshly-copied kernel visible to instruction fetch, enable its temporary
/// TTBR1 map, then branch to the high virtual entry with `x0 = BootInfo*`. Never returns.
///
/// The loader wrote the kernel's `.text` (and patched relocations) with ordinary
/// stores, which sit **dirty in the D-cache**. Real ARM cores do not keep the I- and
/// D-caches coherent, so before fetching that code we must clean each written line —
/// to the Point of *Coherency* (`dc civac`), not just the PoU: on cores whose I-cache
/// fills below the PoU (some DSU/big.LITTLE configs, incl. the SC8280XP) a clean-to-PoU
/// leaves the code stale and the kernel never executes its first instruction. Then we
/// invalidate the I-cache (`ic iallu`). It all appears to work on QEMU, whose caches are
/// modelled coherently. `image_base..image_base+len` is the loaded image (`kernel.phys`).
#[cfg(target_arch = "aarch64")]
unsafe fn jump_to_kernel(
    entry: u64,
    boot_ttbr1: u64,
    boot: *const BootInfo,
    image_base: u64,
    image_len: u64,
) -> ! {
    unsafe {
        let line = dcache_line_size();
        let mut addr = image_base & !(line - 1);
        let end = image_base.wrapping_add(image_len);
        while addr < end {
            core::arch::asm!("dc civac, {a}", a = in(reg) addr, options(nostack, preserves_flags));
            addr = addr.wrapping_add(line);
        }

        // EDK2-on-RPi enters the OS at EL2; the X13s firmware enters at EL1. The kernel is
        // linked high and programs the EL1 translation regime, so at EL2 the `br` below
        // instruction-aborts at zeroth-level translation (the top half is untranslatable
        // under TTBR0_EL2, non-VHE). When we are at EL2, drop to EL1 first: reuse the
        // firmware's EL2 identity map as TTBR0_EL1, install the boot map as TTBR1_EL1,
        // mirror the EL2 T0/PS fields into TCR_EL1, enable the EL1 MMU, and `eret` to the
        // high entry. (E2H is assumed 0, as on RPi EDK2 / QEMU AAVMF; it is cleared below.)
        let current_el: u64;
        core::arch::asm!(
            "mrs {el}, CurrentEL",
            el = out(reg) current_el,
            options(nomem, nostack, preserves_flags),
        );
        if (current_el >> 2) & 0b11 == 2 {
            let tcr_el2: u64;
            let mair_el2: u64;
            let ttbr0_el2: u64;
            let sctlr_el1: u64;
            let hcr_el2: u64;
            core::arch::asm!(
                "mrs {tcr}, tcr_el2",
                "mrs {mair}, mair_el2",
                "mrs {ttbr0}, ttbr0_el2",
                "mrs {sctlr}, sctlr_el1",
                "mrs {hcr}, hcr_el2",
                tcr = out(reg) tcr_el2,
                mair = out(reg) mair_el2,
                ttbr0 = out(reg) ttbr0_el2,
                sctlr = out(reg) sctlr_el1,
                hcr = out(reg) hcr_el2,
                options(nomem, nostack, preserves_flags),
            );

            // TCR_EL1: keep the firmware's T0SZ/IRGN0/ORGN0/SH0/TG0 (bits 0..=15) so
            // TTBR0_EL1 walks the inherited identity map identically; relocate PS->IPS and
            // TBI->TBI0 (their bit positions differ from TCR_EL2); add the same
            // 48-bit / 4 KiB Normal-WB upper half the EL1 path programs for TTBR1.
            let tcr_t1 = (16u64 << 16) // T1SZ
                | (1u64 << 24) // IRGN1: WBWA
                | (1u64 << 26) // ORGN1: WBWA
                | (3u64 << 28) // SH1: inner-shareable
                | (2u64 << 30); // TG1: 4 KiB
            let tcr_el1 = (tcr_el2 & 0xffff)
                | tcr_t1
                | (((tcr_el2 >> 16) & 0x7) << 32) // PS -> IPS
                | (((tcr_el2 >> 20) & 0x1) << 37); // TBI -> TBI0
            let mair_el1 = (mair_el2 & !(0xffu64 << 56)) | (0xffu64 << 56); // slot 7 = Normal WB
                                                                            // The kernel never reprograms SCTLR_EL1, so hand EL1 a complete MMU+caches-on
                                                                            // value: start from its (reset) RES1 pattern and add M | C | I.
            let sctlr_el1 = sctlr_el1 | (1u64 << 0) | (1u64 << 2) | (1u64 << 12);
            // Hand EL1 a clean nVHE HCR_EL2: RW=1 (AArch64 EL1) and everything else 0 — in
            // particular IMO/FMO/AMO=0 so physical IRQ/FIQ/SError are taken to EL1 (not
            // trapped to EL2, where there is no handler after handoff), and E2H/TGE=0 so the
            // EL1 system registers we program are the ones the `eret` uses.
            let _ = hcr_el2;
            let hcr_el2 = 1u64 << 31;
            let spsr_el2 = 0x3c5u64; // EL1h, DAIF (D,A,I,F) masked

            // Hand EL1 the EL2-owned timer/GIC controls, or the kernel's GIC/timer bring-up
            // silently fails after the eret: CNTVOFF_EL2=0 aligns the virtual counter with
            // physical (set in the commit below), and on a PE that has the GICv3 system-
            // register interface (ID_AA64PFR0_EL1.GIC != 0) ICC_SRE_EL2.{Enable,SRE} lets EL1
            // use ICC_*_EL1. A GIC-400/GICv2 board (the Pi 5) has no such interface, so the
            // write is skipped there to avoid an UNDEF.
            let pfr0: u64;
            core::arch::asm!(
                "mrs {pfr0}, id_aa64pfr0_el1",
                pfr0 = out(reg) pfr0,
                options(nomem, nostack, preserves_flags),
            );
            if (pfr0 >> 24) & 0xf != 0 {
                core::arch::asm!(
                    "mrs {t}, icc_sre_el2",
                    "orr {t}, {t}, #1", // SRE (bit 0)
                    "orr {t}, {t}, #8", // Enable (bit 3)
                    "msr icc_sre_el2, {t}",
                    "isb",
                    t = out(reg) _,
                    options(nomem, nostack, preserves_flags),
                );
            }

            core::arch::asm!(
                "dsb sy",
                "ic  iallu",
                "dsb sy",
                "msr hcr_el2, {hcr}",
                "isb",
                "msr mair_el1, {mair}",
                "msr tcr_el1, {tcr}",
                "msr ttbr0_el1, {ttbr0}",
                "msr ttbr1_el1, {ttbr1}",
                "msr sctlr_el1, {sctlr}",
                "msr spsr_el2, {spsr}",
                "msr elr_el2, {elr}",
                "msr cntvoff_el2, xzr",
                "msr cnthctl_el2, {cnthctl}",
                "isb",
                "tlbi vmalle1",
                "dsb ish",
                "isb",
                "eret",
                hcr = in(reg) hcr_el2,
                mair = in(reg) mair_el1,
                tcr = in(reg) tcr_el1,
                ttbr0 = in(reg) ttbr0_el2,
                ttbr1 = in(reg) boot_ttbr1,
                sctlr = in(reg) sctlr_el1,
                spsr = in(reg) spsr_el2,
                elr = in(reg) entry,
                cnthctl = in(reg) 3u64,
                in("x0") boot,
                options(noreturn),
            );
        }

        // Preserve the firmware's TTBR0 geometry while defining a 48-bit, 4 KiB TTBR1
        // regime. MAIR slot 7 is reserved for this temporary Normal-WB mapping.
        let mut mair: u64;
        let mut tcr: u64;
        core::arch::asm!(
            "mrs {mair}, mair_el1",
            "mrs {tcr}, tcr_el1",
            mair = out(reg) mair,
            tcr = out(reg) tcr,
            options(nostack, nomem, preserves_flags),
        );
        mair = (mair & !(0xffu64 << 56)) | (0xffu64 << 56);
        let t1_fields = (0x3fu64 << 16)
            | (1u64 << 23)
            | (0x3u64 << 24)
            | (0x3u64 << 26)
            | (0x3u64 << 28)
            | (0x3u64 << 30);
        tcr = (tcr & !t1_fields)
            | (16u64 << 16) // T1SZ: 48-bit upper VA
            | (1u64 << 24) // IRGN1: WBWA
            | (1u64 << 26) // ORGN1: WBWA
            | (3u64 << 28) // SH1: inner-shareable
            | (2u64 << 30); // TG1: 4 KiB

        core::arch::asm!(
            "dsb sy",
            "ic  iallu",
            "dsb sy",
            "msr mair_el1, {mair}",
            "msr ttbr1_el1, {ttbr1}",
            "msr tcr_el1, {tcr}",
            "isb",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            "br  {entry}",
            entry = in(reg) entry,
            ttbr1 = in(reg) boot_ttbr1,
            mair = in(reg) mair,
            tcr = in(reg) tcr,
            in("x0") boot,
            options(noreturn),
        )
    }
}

/// P10: x86_64 kernel handoff — exit UEFI boot services, preserve the firmware
/// identity map, and jump to the kernel entry with BootInfo in RDI.
#[cfg(target_arch = "x86_64")]
unsafe fn jump_to_kernel(
    entry: u64,
    _boot_ttbr1: u64,
    boot: *const BootInfo,
    _image_base: u64,
    _image_len: u64,
) -> ! {
    unsafe {
        core::arch::asm!(
            "jmp {entry}",
            entry = in(reg) entry,
            in("rdi") boot,
            options(noreturn),
        )
    }
}

/// D-cache line size in bytes, from `CTR_EL0.DminLine` (log2 of the line size in
/// 32-bit words). Used to stride `dc civac` over the loaded kernel image.
#[cfg(target_arch = "aarch64")]
unsafe fn dcache_line_size() -> u64 {
    let ctr: u64;
    unsafe {
        core::arch::asm!(
            "mrs {c}, ctr_el0",
            c = out(reg) ctr,
            options(nostack, nomem, preserves_flags),
        );
    }
    let dminline = (ctr >> 16) & 0xf;
    4u64 << dminline
}

/// Offer a brief pre-handoff pause via the firmware console. Drains any stale key,
/// then polls `ConIn` for ~3 seconds; if a key is pressed it holds until a second
/// key, otherwise it continues so unattended/automated boots are not blocked.
unsafe fn prompt_pause(
    system_table: *mut EfiSystemTable,
    boot_services: *mut EfiBootServices,
    con_out: *mut EfiSimpleTextOutputProtocol,
) {
    if system_table.is_null() {
        return;
    }
    let con_in = unsafe { (*system_table).con_in };
    if con_in.is_null() {
        return;
    }

    // Discard buffered input so a stale key does not auto-pause.
    unsafe { ((*con_in).reset)(con_in, false) };

    kprint!(
        con_out,
        "\r\nPress a key within 3s to pause before handoff...\r\n"
    );

    let mut key = MaybeUninit::<EfiInputKey>::uninit();
    let mut pressed = false;
    for _ in 0..30 {
        let status = unsafe { ((*con_in).read_key_stroke)(con_in, key.as_mut_ptr()) };
        if status == EFI_SUCCESS {
            pressed = true;
            break;
        }
        unsafe { ((*boot_services).stall)(100_000) }; // 100 ms
    }

    if pressed {
        kprint!(con_out, "Paused. Press any key to continue handoff.\r\n");
        wait_for_key(system_table);
    } else {
        kprint!(con_out, "No key pressed; continuing.\r\n");
    }
}

fn wait_for_key(system_table: *mut EfiSystemTable) {
    if system_table.is_null() {
        spin_forever();
    }

    let con_in = unsafe { (*system_table).con_in };
    if con_in.is_null() {
        spin_forever();
    }

    let mut key = MaybeUninit::<EfiInputKey>::uninit();
    loop {
        let status = unsafe { ((*con_in).read_key_stroke)(con_in, key.as_mut_ptr()) };
        if status == EFI_SUCCESS {
            break;
        }
        core::hint::spin_loop();
    }
}

fn spin_forever() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    spin_forever();
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::fmt::Write;

    #[test]
    fn formats_decimal_and_hex_into_utf16() {
        let mut line = Utf16Line::new();
        write!(line, "n={} x={:#x}", 42u32, 0xabcu32).unwrap();
        let words = line.terminated();
        assert_eq!(*words.last().unwrap(), 0);
        let decoded = std::string::String::from_utf16(&words[..words.len() - 1]).unwrap();
        assert_eq!(decoded, "n=42 x=0xabc");
    }

    #[test]
    fn utf16_line_truncates_without_panicking() {
        let mut line = Utf16Line::new();
        for _ in 0..1000 {
            let _ = write!(line, "0123456789");
        }
        let words = line.terminated();
        assert!(words.len() <= Utf16Line::CAP);
        assert_eq!(*words.last().unwrap(), 0);
    }

    #[test]
    fn ascii_path_is_nul_terminated_utf16() {
        let path = ascii_to_utf16::<96>(KERNEL_ESP_PATH);
        let len = path.iter().position(|&w| w == 0).unwrap();
        let decoded = std::string::String::from_utf16(&path[..len]).unwrap();
        assert_eq!(decoded, KERNEL_ESP_PATH);
        assert_eq!(path[len], 0);
    }

    #[test]
    fn efi_configuration_table_layout_matches_the_uefi_abi() {
        assert_eq!(size_of::<EfiGuid>(), 16);
        assert_eq!(size_of::<EfiConfigurationTable>(), 24);
        assert_eq!(
            core::mem::offset_of!(EfiConfigurationTable, vendor_table),
            16
        );
        assert_eq!(
            core::mem::offset_of!(EfiSystemTable, number_of_table_entries),
            104
        );
        assert_eq!(
            core::mem::offset_of!(EfiSystemTable, configuration_table),
            112
        );
    }

    #[test]
    fn raw_efi_map_requires_owned_storage_for_the_complete_range() {
        let mut descriptor = [0u8; 40];
        descriptor[..4].copy_from_slice(&efi_memory_type::BOOT_SERVICES_DATA.to_le_bytes());
        descriptor[8..16].copy_from_slice(&0x4000u64.to_le_bytes());
        descriptor[24..32].copy_from_slice(&2u64.to_le_bytes());
        let view = EfiMemoryMapView {
            descriptors: descriptor.as_ptr(),
            count: 1,
            descriptor_size: descriptor.len(),
        };
        assert!(unsafe { view.contains_firmware_owned_range(0x4080, 0x1000) });
        assert!(!unsafe { view.contains_firmware_owned_range(0x5f00, 0x200) });

        descriptor[..4].copy_from_slice(&efi_memory_type::CONVENTIONAL.to_le_bytes());
        assert!(!unsafe { view.contains_firmware_owned_range(0x4080, 8) });
    }

    #[test]
    fn firmware_dtb_copy_ranges_must_be_disjoint() {
        assert!(!ranges_overlap(0x1000, 0x100, 0x2000, 0x100));
        assert!(ranges_overlap(0x1000, 0x100, 0x1080, 0x100));
        assert!(ranges_overlap(u64::MAX - 3, 8, 0x1000, 8));
    }
}
