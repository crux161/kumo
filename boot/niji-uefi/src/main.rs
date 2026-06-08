#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
#![deny(unsafe_op_in_unsafe_fn)]

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
use niji_loader::elf::{
    for_each_load_reloc, parse_elf64, EM_AARCH64, R_AARCH64_ABS32, R_AARCH64_ABS64,
};
use niji_loader::{summarize_platform, validate_boot_info};
use niji_uefi::{
    build_boot_info, efi_memory_type, efi_pixel_format, framebuffer_from_gop, is_fdt_magic,
    mem_region_kind_from_efi, UefiHandoffSeed,
};

pub type EfiHandle = *mut c_void;
pub type EfiStatus = usize;

const EFI_SUCCESS: EfiStatus = 0;
const EFI_FILE_MODE_READ: u64 = 0x0000_0000_0000_0001;
const EFI_PAGE_SIZE: u64 = 4096;
const EFI_ALLOCATE_ANY_PAGES: u32 = 0;

/// Staged asset paths on the ESP (backslash-separated, as UEFI expects).
const DTB_ESP_PATH: &str = "\\EFI\\KUMO\\dtb\\qcom\\sc8280xp-lenovo-thinkpad-x13s.dtb";
const KERNEL_ESP_PATH: &str = "\\EFI\\KUMO\\kernel\\kumo-kernel.elf";
const INITRD_ESP_PATH: &str = "\\EFI\\KUMO\\initrd.img";

// === EFI GUIDs ===============================================================

#[repr(C)]
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
    stall: *const c_void,
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

    kprint!(con_out, "\r\nNIJIGUMO X13S\r\n");
    kprint!(con_out, "BOOTAA64.EFI loaded\r\n");

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

struct KernelLoad {
    entry: u64,
    phys: Range,
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

    kprint!(con_out, "\r\n--- Nijigumo platform discovery ---\r\n");

    let root = match unsafe { open_boot_volume(image_handle, boot_services) } {
        Some(root) => root,
        None => {
            kprint!(
                con_out,
                "boot volume  : UNAVAILABLE - no ESP filesystem\r\n"
            );
            return;
        }
    };

    let framebuffer = unsafe { discover_framebuffer(boot_services, con_out) };
    let (dtb_addr, _dtb_len) = unsafe { load_dtb(boot_services, root, con_out) };
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
        // Identity mapping for now: the kernel runs in the firmware's page tables
        // after ExitBootServices. Higher-half remap is a later slice.
        kernel_virt: kernel.phys,
        initrd,
        ..UefiHandoffSeed::empty()
    };
    unsafe { boot_ptr.write(build_boot_info(seed)) };

    // Pre-exit summary + validation, using a provisional memory snapshot.
    if let Some((regions_ptr, regions_len, _key)) = unsafe { snapshot_memory(boot_services, &bufs) }
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
    kprint!(con_out, "kernel image : entry {:#x}\r\n", kernel.entry);
    kprint!(
        con_out,
        "initrd       : {}\r\n",
        if discovery.has_initrd {
            "present"
        } else {
            "absent"
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

    kprint!(
        con_out,
        "\r\nExiting boot services; jumping to Ziwei at {:#x}...\r\n",
        kernel.entry
    );

    // ExitBootServices: refill mem_regions from the FINAL map, then exit on the key
    // that map produced. Retry if the map shifts under us.
    let mut exited = false;
    for _ in 0..8 {
        let (regions_ptr, regions_len, map_key) =
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

    // Boot services are gone — no more UEFI console. The kernel's Stage-A console
    // takes over. Make the freshly-copied kernel code visible to the I-cache, then
    // branch to its entry with x0 = BootInfo*.
    unsafe { jump_to_kernel(kernel.entry, boot_ptr) }
}

/// Open the boot device's volume root (`LoadedImage -> SimpleFileSystem`).
unsafe fn open_boot_volume(
    image_handle: EfiHandle,
    boot_services: *mut EfiBootServices,
) -> Option<*mut EfiFileProtocol> {
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
    if status != EFI_SUCCESS || fs.is_null() {
        return None;
    }

    let mut root: *mut EfiFileProtocol = ptr::null_mut();
    let status = unsafe { ((*fs).open_volume)(fs, &mut root) };
    if status != EFI_SUCCESS || root.is_null() {
        return None;
    }
    Some(root)
}

/// Open `path` on `root`, allocate a `LoaderData` pool, and read the whole file
/// into it. Returns `(buffer, len)`; the caller owns the pool allocation.
unsafe fn read_esp_file(
    boot_services: *mut EfiBootServices,
    root: *mut EfiFileProtocol,
    path: &str,
) -> Option<(*mut c_void, usize)> {
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

/// Open the staged DTB, validate its FDT magic, and return `(phys_addr, len)`.
/// The pool buffer is intentionally kept (handed to the kernel via BootInfo).
unsafe fn load_dtb(
    boot_services: *mut EfiBootServices,
    root: *mut EfiFileProtocol,
    con_out: *mut EfiSimpleTextOutputProtocol,
) -> (u64, u64) {
    match unsafe { read_esp_file(boot_services, root, DTB_ESP_PATH) } {
        Some((buffer, len)) => {
            let bytes = unsafe { core::slice::from_raw_parts(buffer as *const u8, len) };
            if is_fdt_magic(bytes) {
                kprint!(
                    con_out,
                    "device tree  : {} @ {:#x} ({} bytes)\r\n",
                    DTB_ESP_PATH,
                    buffer as u64,
                    len
                );
                (buffer as u64, len as u64)
            } else {
                kprint!(con_out, "device tree  : FDT magic missing - ignored\r\n");
                unsafe { ((*boot_services).free_pool)(buffer) };
                (0, 0)
            }
        }
        None => {
            kprint!(con_out, "device tree  : absent\r\n");
            (0, 0)
        }
    }
}

/// Load the kernel ELF: read it, parse program headers, allocate its fixed load
/// region, copy each `PT_LOAD` segment (zeroing BSS), and return its entry + span.
unsafe fn load_kernel(
    boot_services: *mut EfiBootServices,
    root: *mut EfiFileProtocol,
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
    // The kernel is statically linked at a fixed base but carries relocation records
    // (`--emit-relocs`), so we rebase it to wherever we landed.
    let pages = ((image.load_span() + EFI_PAGE_SIZE - 1) / EFI_PAGE_SIZE) as usize;
    let mut load_addr: u64 = 0;
    let status = unsafe {
        ((*boot_services).allocate_pages)(
            EFI_ALLOCATE_ANY_PAGES,
            efi_memory_type::LOADER_DATA,
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

    let delta = load_addr.wrapping_sub(image.load_base);

    for segment in image.segments() {
        let src = unsafe { (buffer as *const u8).add(segment.file_offset as usize) };
        let dst = segment.phys_addr.wrapping_add(delta) as *mut u8;
        unsafe { ptr::copy_nonoverlapping(src, dst, segment.file_size as usize) };
        let zero = segment.mem_size.saturating_sub(segment.file_size) as usize;
        if zero > 0 {
            unsafe { ptr::write_bytes(dst.add(segment.file_size as usize), 0, zero) };
        }
    }

    // Rebase absolute pointers in the loaded image by `delta`. PC-relative
    // relocations move with the code and need no fixup.
    let reloc_result =
        for_each_load_reloc(bytes, image.load_base, image.load_end, |r_offset, ty| {
            let loc = r_offset.wrapping_add(delta);
            match ty {
                R_AARCH64_ABS64 => unsafe {
                    let p = loc as *mut u64;
                    p.write_unaligned(p.read_unaligned().wrapping_add(delta));
                },
                R_AARCH64_ABS32 => unsafe {
                    let p = loc as *mut u32;
                    p.write_unaligned(p.read_unaligned().wrapping_add(delta as u32));
                },
                _ => {}
            }
        });
    unsafe { ((*boot_services).free_pool)(buffer) };
    if let Err(err) = reloc_result {
        kprint!(con_out, "kernel       : relocation failed ({:?})\r\n", err);
        return None;
    }

    let entry = image.entry.wrapping_add(delta);
    kprint!(
        con_out,
        "kernel       : {} @ {:#x}..{:#x} entry {:#x}\r\n",
        KERNEL_ESP_PATH,
        load_addr,
        load_addr + image.load_span(),
        entry
    );
    Some(KernelLoad {
        entry,
        phys: Range::new(load_addr, image.load_span()),
    })
}

/// Load an optional initrd from the ESP. The pool buffer (if any) is handed to the
/// kernel via `BootInfo.initrd`. Absence is honest, not fatal.
unsafe fn load_initrd(
    boot_services: *mut EfiBootServices,
    root: *mut EfiFileProtocol,
    con_out: *mut EfiSimpleTextOutputProtocol,
) -> (Range, *mut c_void) {
    match unsafe { read_esp_file(boot_services, root, INITRD_ESP_PATH) } {
        Some((buffer, len)) => {
            kprint!(
                con_out,
                "initrd       : {} @ {:#x} ({} bytes)\r\n",
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
/// descriptor into a `MemRegion`. Returns `(regions_ptr, count, map_key)`; the
/// `map_key` is what ExitBootServices must be called with.
unsafe fn snapshot_memory(
    boot_services: *mut EfiBootServices,
    bufs: &MemBufs,
) -> Option<(u64, u64, usize)> {
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

    Some((bufs.regions as u64, written as u64, map_key))
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

/// Make the freshly-copied kernel visible to instruction fetch, then branch to its
/// entry with `x0 = BootInfo*`. Never returns.
unsafe fn jump_to_kernel(entry: u64, boot: *const BootInfo) -> ! {
    unsafe {
        core::arch::asm!(
            "dsb sy",
            "ic  iallu",
            "dsb sy",
            "isb",
            "br  {entry}",
            entry = in(reg) entry,
            in("x0") boot,
            options(noreturn),
        )
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
}
