#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use kumo_abi::{
    BootInfo, Framebuffer, FramebufferFormat, MemRegionKind, Range, RawSlice, ABI_VERSION,
};

pub const BOOTLOADER_NAME: &str = "Nijigumo";

/// Device-tree blob magic (`0xd00dfeed`, big-endian) — the first four bytes of a
/// flattened device tree. Used to sanity-check the DTB we open from the ESP
/// before we record its address in the handoff.
pub const FDT_MAGIC: u32 = 0xd00d_feed;

/// UEFI `EFI_MEMORY_TYPE` values we translate into [`MemRegionKind`].
///
/// Only the variants Nijigumo distinguishes are named; everything else folds
/// into [`MemRegionKind::Reserved`] (or [`MemRegionKind::Unknown`] for values
/// outside the spec) by [`mem_region_kind_from_efi`].
pub mod efi_memory_type {
    pub const RESERVED: u32 = 0;
    pub const LOADER_CODE: u32 = 1;
    pub const LOADER_DATA: u32 = 2;
    pub const BOOT_SERVICES_CODE: u32 = 3;
    pub const BOOT_SERVICES_DATA: u32 = 4;
    pub const RUNTIME_SERVICES_CODE: u32 = 5;
    pub const RUNTIME_SERVICES_DATA: u32 = 6;
    pub const CONVENTIONAL: u32 = 7;
    pub const UNUSABLE: u32 = 8;
    pub const ACPI_RECLAIM: u32 = 9;
    pub const ACPI_NVS: u32 = 10;
    pub const MMIO: u32 = 11;
    pub const MMIO_PORT_SPACE: u32 = 12;
    pub const PAL_CODE: u32 = 13;
    pub const PERSISTENT: u32 = 14;
}

/// UEFI `EFI_GRAPHICS_PIXEL_FORMAT` values we translate into [`FramebufferFormat`].
pub mod efi_pixel_format {
    pub const RGB_RESERVED_8: u32 = 0;
    pub const BGR_RESERVED_8: u32 = 1;
    pub const BIT_MASK: u32 = 2;
    pub const BLT_ONLY: u32 = 3;
}

/// Translate a UEFI memory descriptor type into KUMO's [`MemRegionKind`].
///
/// Boot-services code/data are reported as [`MemRegionKind::Usable`] because the
/// firmware releases them to the OS once `ExitBootServices` is called; the kernel
/// that eventually consumes this map runs after that point. Loader code/data is
/// tagged [`MemRegionKind::Bootloader`] (it holds this very handoff payload),
/// runtime services stay [`MemRegionKind::Reserved`], and ACPI/MMIO ranges keep
/// their own kinds so a later platform server can find them.
pub fn mem_region_kind_from_efi(efi_type: u32) -> MemRegionKind {
    use efi_memory_type as t;
    match efi_type {
        t::CONVENTIONAL | t::BOOT_SERVICES_CODE | t::BOOT_SERVICES_DATA => MemRegionKind::Usable,
        t::LOADER_CODE | t::LOADER_DATA => MemRegionKind::Bootloader,
        t::ACPI_RECLAIM | t::ACPI_NVS => MemRegionKind::Acpi,
        t::MMIO | t::MMIO_PORT_SPACE => MemRegionKind::Mmio,
        t::RESERVED
        | t::RUNTIME_SERVICES_CODE
        | t::RUNTIME_SERVICES_DATA
        | t::UNUSABLE
        | t::PAL_CODE
        | t::PERSISTENT => MemRegionKind::Reserved,
        _ => MemRegionKind::Unknown,
    }
}

/// Translate a UEFI GOP pixel format into KUMO's [`FramebufferFormat`].
///
/// Bit-mask and blt-only modes carry no direct byte layout we can hand to a dumb
/// framebuffer console, so they map to [`FramebufferFormat::Unknown`].
pub fn framebuffer_format_from_efi(pixel_format: u32) -> FramebufferFormat {
    use efi_pixel_format as p;
    match pixel_format {
        p::RGB_RESERVED_8 => FramebufferFormat::Rgb,
        p::BGR_RESERVED_8 => FramebufferFormat::Bgr,
        p::BIT_MASK | p::BLT_ONLY => FramebufferFormat::Unknown,
        _ => FramebufferFormat::Unknown,
    }
}

/// Assemble a [`Framebuffer`] from the fields a UEFI GOP `Mode` exposes.
///
/// `stride` is recorded in **pixels per scan line** (the GOP-native unit), not
/// bytes; consumers multiply by the bytes-per-pixel implied by `format`.
pub fn framebuffer_from_gop(
    base: u64,
    len: u64,
    width: u32,
    height: u32,
    pixels_per_scan_line: u32,
    pixel_format: u32,
) -> Framebuffer {
    Framebuffer {
        phys: base,
        len,
        width,
        height,
        stride: pixels_per_scan_line,
        format: framebuffer_format_from_efi(pixel_format),
    }
}

/// Does `bytes` begin with the flattened-device-tree magic?
pub fn is_fdt_magic(bytes: &[u8]) -> bool {
    match bytes {
        [a, b, c, d, ..] => u32::from_be_bytes([*a, *b, *c, *d]) == FDT_MAGIC,
        _ => false,
    }
}

/// The raw values Nijigumo gathers from UEFI before assembling a [`BootInfo`].
///
/// Pointers are captured as `u64` so this stays a plain, `Copy` description of the
/// handoff that host tests can build without a live firmware environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UefiHandoffSeed {
    pub mem_regions_ptr: u64,
    pub mem_regions_len: u64,
    pub kernel_phys: Range,
    pub kernel_virt: Range,
    pub initrd: Range,
    pub cmdline_ptr: u64,
    pub cmdline_len: u64,
    pub acpi_rsdp: u64,
    pub dtb: u64,
    pub framebuffer: Option<Framebuffer>,
}

impl UefiHandoffSeed {
    pub const fn empty() -> Self {
        Self {
            mem_regions_ptr: 0,
            mem_regions_len: 0,
            kernel_phys: Range::empty(),
            kernel_virt: Range::empty(),
            initrd: Range::empty(),
            cmdline_ptr: 0,
            cmdline_len: 0,
            acpi_rsdp: 0,
            dtb: 0,
            framebuffer: None,
        }
    }
}

pub fn build_boot_info(seed: UefiHandoffSeed) -> BootInfo {
    let mut boot = BootInfo::empty(ABI_VERSION);
    boot.mem_regions = RawSlice::from_raw_parts(seed.mem_regions_ptr, seed.mem_regions_len);
    boot.kernel_phys = seed.kernel_phys;
    boot.kernel_virt = seed.kernel_virt;
    boot.initrd = seed.initrd;
    boot.cmdline = RawSlice::from_raw_parts(seed.cmdline_ptr, seed.cmdline_len);
    boot.platform.acpi_rsdp = seed.acpi_rsdp;
    boot.platform.dtb = seed.dtb;
    if let Some(framebuffer) = seed.framebuffer {
        boot.framebuffer = framebuffer;
        boot.flags |= BootInfo::FLAG_FRAMEBUFFER_PRESENT;
    }
    boot
}

pub fn validate_seed(seed: UefiHandoffSeed) -> Result<(), niji_loader::HandoffError> {
    let boot = build_boot_info(seed);
    niji_loader::validate_boot_info(&boot).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_bootloader() {
        assert_eq!(BOOTLOADER_NAME, "Nijigumo");
    }

    #[test]
    fn boot_services_memory_is_usable_after_exit() {
        assert_eq!(
            mem_region_kind_from_efi(efi_memory_type::CONVENTIONAL),
            MemRegionKind::Usable
        );
        assert_eq!(
            mem_region_kind_from_efi(efi_memory_type::BOOT_SERVICES_CODE),
            MemRegionKind::Usable
        );
        assert_eq!(
            mem_region_kind_from_efi(efi_memory_type::BOOT_SERVICES_DATA),
            MemRegionKind::Usable
        );
    }

    #[test]
    fn loader_memory_is_tagged_bootloader() {
        assert_eq!(
            mem_region_kind_from_efi(efi_memory_type::LOADER_CODE),
            MemRegionKind::Bootloader
        );
        assert_eq!(
            mem_region_kind_from_efi(efi_memory_type::LOADER_DATA),
            MemRegionKind::Bootloader
        );
    }

    #[test]
    fn acpi_mmio_and_reserved_keep_their_kinds() {
        assert_eq!(
            mem_region_kind_from_efi(efi_memory_type::ACPI_RECLAIM),
            MemRegionKind::Acpi
        );
        assert_eq!(
            mem_region_kind_from_efi(efi_memory_type::MMIO),
            MemRegionKind::Mmio
        );
        assert_eq!(
            mem_region_kind_from_efi(efi_memory_type::RUNTIME_SERVICES_DATA),
            MemRegionKind::Reserved
        );
        assert_eq!(
            mem_region_kind_from_efi(efi_memory_type::PERSISTENT),
            MemRegionKind::Reserved
        );
    }

    #[test]
    fn unknown_memory_type_is_unknown() {
        assert_eq!(mem_region_kind_from_efi(9999), MemRegionKind::Unknown);
    }

    #[test]
    fn pixel_formats_map_to_rgb_and_bgr() {
        assert_eq!(
            framebuffer_format_from_efi(efi_pixel_format::RGB_RESERVED_8),
            FramebufferFormat::Rgb
        );
        assert_eq!(
            framebuffer_format_from_efi(efi_pixel_format::BGR_RESERVED_8),
            FramebufferFormat::Bgr
        );
        assert_eq!(
            framebuffer_format_from_efi(efi_pixel_format::BIT_MASK),
            FramebufferFormat::Unknown
        );
        assert_eq!(
            framebuffer_format_from_efi(efi_pixel_format::BLT_ONLY),
            FramebufferFormat::Unknown
        );
    }

    #[test]
    fn gop_framebuffer_records_pixel_stride() {
        let fb = framebuffer_from_gop(
            0x8000_0000,
            1920 * 1080 * 4,
            1920,
            1080,
            1920,
            efi_pixel_format::BGR_RESERVED_8,
        );
        assert_eq!(fb.phys, 0x8000_0000);
        assert_eq!(fb.width, 1920);
        assert_eq!(fb.height, 1080);
        assert_eq!(fb.stride, 1920);
        assert_eq!(fb.format, FramebufferFormat::Bgr);
    }

    #[test]
    fn detects_fdt_magic() {
        assert!(is_fdt_magic(&[0xd0, 0x0d, 0xfe, 0xed, 0x00, 0x00]));
        assert!(!is_fdt_magic(&[0x7f, b'E', b'L', b'F']));
        assert!(!is_fdt_magic(&[0xd0, 0x0d]));
    }

    #[test]
    fn build_boot_info_records_framebuffer_and_flag() {
        let mut seed = UefiHandoffSeed::empty();
        seed.dtb = 0x4000_0000;
        seed.framebuffer = Some(Framebuffer {
            phys: 0x9000_0000,
            len: 0x0080_0000,
            width: 1280,
            height: 800,
            stride: 1280,
            format: FramebufferFormat::Bgr,
        });
        let boot = build_boot_info(seed);
        assert!(boot.has_framebuffer());
        assert_eq!(boot.framebuffer.width, 1280);
        assert_eq!(boot.platform.dtb, 0x4000_0000);
    }

    #[test]
    fn build_boot_info_without_framebuffer_leaves_flag_clear() {
        let boot = build_boot_info(UefiHandoffSeed::empty());
        assert!(!boot.has_framebuffer());
    }
}
