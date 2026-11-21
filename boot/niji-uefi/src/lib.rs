#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

//j804
//j810
//j905
//j910
//j932

use kumo_abi::{
    BootInfo, Framebuffer, FramebufferFormat, MemRegionKind, Range, RawSlice, ABI_VERSION,
};

#[cfg(feature = "ax88-oracle")]
pub mod ax88;
pub mod nvme;
#[cfg(feature = "nvme-prepare")]
pub mod nvme_prepare;

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

/// The pixel format a GOP mode effectively has (j905, PLAN/025 X2c). A `PixelBitMask` mode whose
/// masks are exactly the byte lanes of a 32-bit pixel (`masks` = the mode's red, green, blue,
/// reserved `PixelInformation`) is laid out exactly like one of the two 8-bit-per-colour formats,
/// so it is reported as that format. Some Intel GOP drivers describe their linear framebuffer
/// this way; Fuchsia's gigaboot maps the same masks to `ZBI_PIXEL_FORMAT_RGB_X888`. Any other
/// mask (565, 10-bit, overlapping lanes) and BltOnly keep their own format and stay unusable.
pub fn effective_pixel_format(pixel_format: u32, masks: [u32; 4]) -> u32 {
    use efi_pixel_format as p;
    if pixel_format != p::BIT_MASK {
        return pixel_format;
    }
    let [red, green, blue, reserved] = masks;
    if reserved & (red | green | blue) != 0 || green != 0x0000_ff00 {
        return pixel_format;
    }
    match (red, blue) {
        // Memory bytes R, G, B, x: the u32 is 0xxxBBGGRR.
        (0x0000_00ff, 0x00ff_0000) => p::RGB_RESERVED_8,
        // Memory bytes B, G, R, x: the u32 is 0xxxRRGGBB.
        (0x00ff_0000, 0x0000_00ff) => p::BGR_RESERVED_8,
        _ => pixel_format,
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

/// One GOP mode as `QueryMode` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GopMode {
    pub number: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
}

impl GopMode {
    /// A linear 32-bit RGB/BGR buffer, the only kind KUMO's framebuffer path takes.
    pub fn is_linear(&self) -> bool {
        framebuffer_format_from_efi(self.pixel_format) != FramebufferFormat::Unknown
            && self.width != 0
            && self.height != 0
    }

    fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

/// The smallest firmware mode Nijigumo keeps, by area (1280x720 qualifies). Below it the
/// firmware's choice is a fallback, not the sink's preferred mode: the RK3588 firmware leaves
/// HDMI at 640x480 VGA when it did not get the sink's EDID (KUMO's D0d saw it on several boots),
/// and the NUC7i5DNHE's Intel GOP left 800x600 — a 100x37 console — on its first metal boot
/// (j908). j910 raised the floor from "above 640x480" to this.
pub const GOP_KEEP_FLOOR: (u32, u32) = (1024, 768);
/// The largest mode a fallback is replaced with. With no EDID to go by, 1080p is the CEA-861
/// mode HDMI sinks most widely accept; anything larger risks an out-of-range sink.
pub const GOP_FALLBACK_MAX: (u32, u32) = (1920, 1080);

/// What Nijigumo does with the firmware's GOP mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GopChoice {
    /// The firmware's mode is linear and at least [`GOP_KEEP_FLOOR`]: its choice (normally the
    /// sink's EDID-preferred mode) stands.
    KeepFirmware,
    /// The firmware's mode is below the floor or not linear, and nothing better is offered.
    KeepNoBetter,
    /// Switch to this mode: the largest linear one within [`GOP_FALLBACK_MAX`], and larger
    /// than the current one when that is linear.
    Switch(GopMode),
}

/// Choose the boot mode. The firmware's choice is kept unless it is below [`GOP_KEEP_FLOOR`]
/// (or not linear); then the largest linear mode within [`GOP_FALLBACK_MAX`] that is larger
/// than the current one replaces it, the lowest mode number winning a tie. A mode the firmware
/// reports as current is never re-set.
pub fn choose_gop_mode(current: GopMode, offered: &[GopMode]) -> GopChoice {
    let keep_floor = u64::from(GOP_KEEP_FLOOR.0) * u64::from(GOP_KEEP_FLOOR.1);
    if current.is_linear() && current.area() >= keep_floor {
        return GopChoice::KeepFirmware;
    }
    let floor = if current.is_linear() {
        current.area()
    } else {
        0
    };
    let mut best: Option<GopMode> = None;
    for mode in offered {
        if mode.number == current.number
            || !mode.is_linear()
            || mode.width > GOP_FALLBACK_MAX.0
            || mode.height > GOP_FALLBACK_MAX.1
            || mode.area() <= floor
        {
            continue;
        }
        match best {
            Some(b)
                if b.area() > mode.area()
                    || (b.area() == mode.area() && b.number < mode.number) => {}
            _ => best = Some(*mode),
        }
    }
    best.map_or(GopChoice::KeepNoBetter, GopChoice::Switch)
}

/// One unambiguous EFI Reserved/RuntimeServicesData descriptor must contain the whole GOP page span,
/// including padding. This proves reservation only; GCD SystemMemory must also
/// prove RAM before the loader sets the handoff attestation. — KESTREL
pub fn reserved_framebuffer_span(
    fb: Framebuffer,
    descriptors: &[u8],
    descriptor_size: usize,
) -> Option<Range> {
    if !fb.is_plausible()
        || fb.format == FramebufferFormat::Unknown
        || descriptor_size < 40
        || descriptors.is_empty()
        || descriptors.len() % descriptor_size != 0
    {
        return None;
    }
    let len = fb.mapping_len()?;
    let end = fb.phys.checked_add(len)?;
    if end > 1 << 48 {
        return None;
    }
    let mut found = false;
    for desc in descriptors.chunks_exact(descriptor_size) {
        let kind = u32::from_le_bytes(desc[..4].try_into().ok()?);
        let base = u64::from_le_bytes(desc[8..16].try_into().ok()?);
        let pages = u64::from_le_bytes(desc[24..32].try_into().ok()?);
        let attrs = u64::from_le_bytes(desc[32..40].try_into().ok()?);
        let region_end = base.checked_add(pages.checked_mul(4096)?)?;
        if pages == 0 || base >= end || region_end <= fb.phys {
            continue;
        }
        if found
            || !matches!(
                kind,
                efi_memory_type::RESERVED | efi_memory_type::RUNTIME_SERVICES_DATA
            )
            || (kind == efi_memory_type::RESERVED && attrs & (1 << 63) != 0)
            || (kind == efi_memory_type::RUNTIME_SERVICES_DATA && attrs & (1 << 63) == 0)
            || base % 4096 != 0
            || base > fb.phys
            || region_end < end
        {
            return None;
        }
        found = true;
    }
    found.then_some(Range::new(fb.phys, len))
}

/// Does `bytes` begin with the flattened-device-tree magic?
pub fn is_fdt_magic(bytes: &[u8]) -> bool {
    match bytes {
        [a, b, c, d, ..] => u32::from_be_bytes([*a, *b, *c, *d]) == FDT_MAGIC,
        _ => false,
    }
}

/// The FDT header's `totalsize` field (big-endian `u32` at byte offset 4). Used to size a
/// firmware-provided DTB, where the EFI configuration table hands us only a pointer — the
/// length must come from the blob itself. `None` if `bytes` is shorter than the header field.
pub fn fdt_total_size(bytes: &[u8]) -> Option<u32> {
    match bytes {
        [_, _, _, _, a, b, c, d, ..] => Some(u32::from_be_bytes([*a, *b, *c, *d])),
        _ => None,
    }
}

/// Whether a firmware-owned pointer is structurally safe enough to dereference while UEFI's
/// mappings are still active. This cannot prove that the pages are mapped, but it rejects the
/// corruption shapes that must never reach `from_raw_parts`: null/all-ones sentinels, arithmetic
/// wrap, misalignment, and addresses outside KUMO's lower 48-bit physical-address contract.
/// — KESTREL 2026-07-17
pub const fn is_plausible_firmware_pointer(address: u64, byte_len: u64, alignment: u64) -> bool {
    if address == 0
        || address == u64::MAX
        || byte_len == 0
        || alignment == 0
        || !alignment.is_power_of_two()
        || address & (alignment - 1) != 0
    {
        return false;
    }
    let Some(last) = address.checked_add(byte_len - 1) else {
        return false;
    };
    address < (1u64 << 48) && last < (1u64 << 48)
}

/// Whether an EFI descriptor type denotes storage currently owned by firmware or the loader and
/// therefore safe to inspect before `ExitBootServices`. Conventional/free memory is deliberately
/// excluded: merely being RAM does not prove that a configuration-table pointer owns those bytes,
/// and a subsequent pool allocation could reuse them. — KESTREL 2026-07-17
pub const fn efi_memory_type_is_firmware_owned(efi_type: u32) -> bool {
    use efi_memory_type as t;
    matches!(
        efi_type,
        t::LOADER_CODE
            | t::LOADER_DATA
            | t::BOOT_SERVICES_CODE
            | t::BOOT_SERVICES_DATA
            | t::RUNTIME_SERVICES_CODE
            | t::RUNTIME_SERVICES_DATA
            | t::ACPI_RECLAIM
            | t::ACPI_NVS
    )
}

/// Parse the explicit serial-route form accepted by `nijigumo.conf`:
/// `console-uart = pl011@0x<physical-base>`. The base must be a nonzero, page-aligned address
/// inside KUMO's 48-bit aarch64 physical-address contract. A malformed or sentinel value is
/// rejected rather than becoming an MMIO write. — KESTREL 2026-07-17
pub fn parse_pl011_console(value: &str) -> Option<u64> {
    let (kind, address) = value.trim().split_once('@')?;
    if !kind.eq_ignore_ascii_case("pl011") {
        return None;
    }
    let digits = address
        .strip_prefix("0x")
        .or_else(|| address.strip_prefix("0X"))?;
    let base = u64::from_str_radix(digits, 16).ok()?;
    if base == 0 || base >= (1u64 << 48) || base & 0xfff != 0 {
        return None;
    }
    Some(base)
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

/// Where Nijigumo should obtain the device tree, per the staged boot config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtbSource<'a> {
    /// Use the firmware-provided DTB from the EFI configuration table — the universal
    /// handoff that Pi 5 (EDK2 in Device-Tree mode), QEMU/AAVMF, and generic UEFI all
    /// populate. The default when `dtb` is unspecified.
    Firmware,
    /// Load the DTB from this ESP path — an override for boards whose firmware DTB is
    /// inadequate or absent (e.g. the ThinkPad X13s ships its own).
    Esp(&'a str),
}

/// The declarative boot manifest Nijigumo reads from the ESP instead of hardcoding
/// per-target paths and a DTB search heuristic (the GRUB/systemd-boot model). The KUMO
/// imager stages one per target image describing exactly what to load, so the bootloader
/// is handed the right data rather than re-deriving it.
///
/// Format: `key = value` lines, `#` comments, blank lines ignored — the same key=value
/// shape the imager already emits. Unknown keys are ignored (forward-compatible). — CORVUS
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootConfig<'a> {
    pub kernel: &'a str,
    pub initrd: Option<&'a str>,
    pub dtb: DtbSource<'a>,
    /// The board this image was built for (a `kumo_bsp::Board` / `imager::HardwareTarget`
    /// id, e.g. `raspberry-pi-5`). The image is built for exactly one board, so identity is
    /// a build-time fact the imager bakes here rather than something the kernel sniffs at
    /// runtime (DESIGN/017 §3). `None` when the key is absent — the kernel then reports the
    /// board as unstamped instead of guessing.
    pub board: Option<&'a str>,
    /// Optional explicit serial route. Kept as source text so the loader can diagnose an invalid
    /// value loudly; [`parse_pl011_console`] performs the typed conversion.
    pub console_uart: Option<&'a str>,
    /// A zstd-compressed copy of the initrd for TFTP and slow media (`initrd_zstd`), with the
    /// SHA-256 of the *compressed* stream (`initrd_zstd_sha256`, checked before decoding) and
    /// the decoded length (`initrd_size`). [`BootConfig::initrd_zstd`] yields all three or
    /// nothing; loaders that predate these keys ignore them and load `initrd` as before.
    pub initrd_zstd_path: Option<&'a str>,
    pub initrd_zstd_sha256: Option<[u8; 32]>,
    pub initrd_size: Option<u64>,
    /// j932 (SH8-f, D2): the root filesystem, `root = uuid:<hex>`, as source text. The loader
    /// checks it with [`kumo_abi::boot::parse_uuid`] and forwards it in `BootInfo.cmdline` as
    /// `root=uuid:<hex>`; Sora mounts the RedoxFS volume whose header carries that UUID.
    pub root: Option<&'a str>,
}

/// The compressed-initrd triple from a manifest: path, compressed-stream SHA-256, decoded size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InitrdZstd<'a> {
    pub path: &'a str,
    pub sha256: [u8; 32],
    pub size: u64,
}

/// 64 hex digits to 32 bytes; `None` for any other length or a non-hex digit.
pub fn parse_sha256_hex(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (i, pair) in bytes.chunks_exact(2).enumerate() {
        out[i] = nibble(pair[0])? << 4 | nibble(pair[1])?;
    }
    Some(out)
}

impl<'a> BootConfig<'a> {
    /// The compressed initrd, when the manifest names all of its path, digest and size.
    pub fn initrd_zstd(&self) -> Option<InitrdZstd<'a>> {
        Some(InitrdZstd {
            path: self.initrd_zstd_path?,
            sha256: self.initrd_zstd_sha256?,
            size: self.initrd_size.filter(|size| *size > 0)?,
        })
    }

    /// Parse the config text. Returns `None` if the mandatory `kernel` key is absent.
    /// `dtb` defaults to [`DtbSource::Firmware`] (the universal handoff); `dtb = firmware`
    /// is the explicit form and `dtb = esp:<path>` selects an ESP override.
    pub fn parse(text: &'a str) -> Option<Self> {
        let mut kernel: Option<&'a str> = None;
        let mut initrd: Option<&'a str> = None;
        let mut board: Option<&'a str> = None;
        let mut console_uart: Option<&'a str> = None;
        let mut initrd_zstd_path: Option<&'a str> = None;
        let mut initrd_zstd_sha256: Option<[u8; 32]> = None;
        let mut initrd_size: Option<u64> = None;
        let mut root: Option<&'a str> = None;
        let mut dtb = DtbSource::Firmware;
        for raw in text.lines() {
            let line = match raw.split_once('#') {
                Some((before, _)) => before,
                None => raw,
            }
            .trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "kernel" => kernel = Some(value),
                "initrd" => initrd = Some(value),
                "board" => board = Some(value),
                "console-uart" | "console_uart" => console_uart = Some(value),
                "initrd_zstd" => initrd_zstd_path = Some(value),
                "initrd_zstd_sha256" => initrd_zstd_sha256 = parse_sha256_hex(value),
                "initrd_size" => initrd_size = value.parse().ok(),
                "root" => root = Some(value),
                "dtb" => {
                    dtb = if value.eq_ignore_ascii_case("firmware") {
                        DtbSource::Firmware
                    } else if let Some(path) = value.strip_prefix("esp:") {
                        DtbSource::Esp(path.trim())
                    } else {
                        DtbSource::Esp(value)
                    };
                }
                _ => {} // forward-compatible: ignore unknown keys
            }
        }
        Some(Self {
            kernel: kernel?,
            initrd,
            dtb,
            board,
            console_uart,
            initrd_zstd_path,
            initrd_zstd_sha256,
            initrd_size,
            root,
        })
    }

    /// j932: the boot command line the `root` key asks for, `root=uuid:<32 hex digits>`, or
    /// `None` when the key is absent or does not hold exactly one UUID.
    pub fn root_cmdline(&self) -> Option<([u8; 42], usize)> {
        let value = self.root?;
        let hex = value.strip_prefix("uuid:")?.trim();
        let uuid = kumo_abi::boot::parse_uuid(hex.as_bytes())?;
        let mut line = [0u8; 42];
        line[..10].copy_from_slice(b"root=uuid:");
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for (i, byte) in uuid.iter().enumerate() {
            line[10 + 2 * i] = HEX[usize::from(byte >> 4)];
            line[11 + 2 * i] = HEX[usize::from(byte & 0xf)];
        }
        Some((line, 42))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_key_becomes_one_canonical_cmdline_word() {
        let cfg =
            BootConfig::parse("kernel = k\nroot = uuid:00112233-4455-6677-8899-AABBCCDDEEFF\n")
                .unwrap();
        let (line, len) = cfg.root_cmdline().expect("a valid root");
        assert_eq!(&line[..len], b"root=uuid:00112233445566778899aabbccddeeff");
        assert_eq!(
            kumo_abi::boot::root_uuid(&line[..len]).map(|u| u[15]),
            Some(0xff),
            "Sora's parser reads what the loader writes"
        );
        for bad in [
            "kernel = k\nroot = /dev/disk5s1\n",
            "kernel = k\nroot = uuid:0011\n",
            "kernel = k\n",
        ] {
            let cfg = BootConfig::parse(bad).unwrap();
            assert_eq!(cfg.root_cmdline(), None, "{bad}");
        }
    }

    #[test]
    fn the_compressed_initrd_needs_path_digest_and_size_together() {
        extern crate std;
        use std::format;
        let digest = "00112233445566778899aabbccddeeff00112233445566778899AABBCCDDEEFF";
        let full = format!(
            "kernel = \\k\ninitrd = \\EFI\\KUMO\\initrd.img\n\
             initrd_zstd = \\EFI\\KUMO\\initrd.img.zst\n\
             initrd_zstd_sha256 = {digest}\ninitrd_size = 9546999\n"
        );
        let cfg = BootConfig::parse(&full).expect("parses");
        let z = cfg.initrd_zstd().expect("all three keys");
        assert_eq!(z.path, "\\EFI\\KUMO\\initrd.img.zst");
        assert_eq!(z.size, 9_546_999);
        assert_eq!(z.sha256[0], 0x00);
        assert_eq!(z.sha256[15], 0xff);
        assert_eq!(z.sha256[31], 0xff);
        assert_eq!(cfg.initrd, Some("\\EFI\\KUMO\\initrd.img"), "raw path kept");

        for broken in [
            full.replace("initrd_size = 9546999", "initrd_size = 0"),
            full.replace("initrd_size = 9546999", "initrd_size = lots"),
            full.replace(digest, &digest[..63]),
            full.replace(digest, &format!("{}g", &digest[..63])),
            full.replace("initrd_zstd = \\EFI\\KUMO\\initrd.img.zst\n", ""),
        ] {
            let cfg = BootConfig::parse(&broken).expect("still parses");
            assert_eq!(cfg.initrd_zstd(), None, "{broken}");
        }
    }

    #[test]
    fn parses_x13s_style_config_with_esp_dtb_override() {
        let cfg = BootConfig::parse(
            "# nijigumo boot config\n\
             kernel = \\EFI\\KUMO\\kernel\\kumo-kernel.elf\n\
             initrd = \\EFI\\KUMO\\initrd.img\n\
             dtb    = esp:\\EFI\\KUMO\\dtb\\qcom\\sc8280xp-lenovo-thinkpad-x13s.dtb\n",
        )
        .unwrap();
        assert_eq!(cfg.kernel, "\\EFI\\KUMO\\kernel\\kumo-kernel.elf");
        assert_eq!(cfg.initrd, Some("\\EFI\\KUMO\\initrd.img"));
        assert_eq!(
            cfg.dtb,
            DtbSource::Esp("\\EFI\\KUMO\\dtb\\qcom\\sc8280xp-lenovo-thinkpad-x13s.dtb")
        );
    }

    #[test]
    fn pi5_style_config_uses_firmware_dtb_and_no_initrd() {
        let cfg =
            BootConfig::parse("kernel = \\EFI\\KUMO\\kernel\\kumo-kernel.elf\ndtb = firmware\n")
                .unwrap();
        assert_eq!(cfg.dtb, DtbSource::Firmware);
        assert_eq!(cfg.initrd, None);
    }

    #[test]
    fn dtb_defaults_to_firmware_and_unknown_keys_are_ignored() {
        let cfg = BootConfig::parse("kernel = k\nfuture_option = whatever\n").unwrap();
        assert_eq!(cfg.dtb, DtbSource::Firmware);
        assert_eq!(cfg.kernel, "k");
    }

    #[test]
    fn missing_kernel_is_rejected() {
        assert_eq!(BootConfig::parse("dtb = firmware\n"), None);
    }

    #[test]
    fn parses_the_board_identity_the_imager_bakes() {
        let cfg = BootConfig::parse(
            "kernel = \\EFI\\KUMO\\kernel\\kumo-kernel.elf\nboard = raspberry-pi-5\n",
        )
        .unwrap();
        assert_eq!(cfg.board, Some("raspberry-pi-5"));
    }

    #[test]
    fn parses_an_explicit_rp1_pl011_console_route() {
        let cfg =
            BootConfig::parse("kernel = k\nconsole-uart = pl011@0x0000001c00030000\n").unwrap();
        assert_eq!(cfg.console_uart, Some("pl011@0x0000001c00030000"));
        assert_eq!(
            cfg.console_uart.and_then(parse_pl011_console),
            Some(0x1c_0003_0000)
        );
    }

    #[test]
    fn rejects_unsafe_or_malformed_console_routes() {
        assert_eq!(parse_pl011_console("dw8250@0x1c00030000"), None);
        assert_eq!(parse_pl011_console("pl011@0xffffffffffffffff"), None);
        assert_eq!(parse_pl011_console("pl011@0x1c00030001"), None);
        assert_eq!(parse_pl011_console("pl011@1c00030000"), None);
    }

    #[test]
    fn board_is_absent_when_the_manifest_names_none() {
        // Pre-DESIGN/017 manifests carry no `board` key; they must still parse, leaving the
        // kernel to report the board unstamped rather than guessing one.
        let cfg = BootConfig::parse("kernel = k\ndtb = firmware\n").unwrap();
        assert_eq!(cfg.board, None);
    }

    #[test]
    fn comments_and_blank_lines_and_trailing_comments_are_ignored() {
        let cfg =
            BootConfig::parse("\n   # a comment\nkernel = k   # trailing comment\n\n").unwrap();
        assert_eq!(cfg.kernel, "k");
    }

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
    fn x888_bitmask_modes_are_their_byte_layout_and_others_stay_unusable() {
        use efi_pixel_format as p;
        let x = 0xff00_0000;
        // Red in the high lane is the BGR byte order (gigaboot's RGB_x888), red low is RGB.
        assert_eq!(
            effective_pixel_format(p::BIT_MASK, [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, x]),
            p::BGR_RESERVED_8
        );
        assert_eq!(
            effective_pixel_format(p::BIT_MASK, [0x0000_00ff, 0x0000_ff00, 0x00ff_0000, x]),
            p::RGB_RESERVED_8
        );
        // A zero reserved mask is fine; one overlapping a colour lane is not.
        assert_eq!(
            effective_pixel_format(p::BIT_MASK, [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0]),
            p::BGR_RESERVED_8
        );
        for masks in [
            [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0x0000_00ff], // reserved overlaps blue
            [0x0000_f800, 0x0000_07e0, 0x0000_001f, 0],           // RGB565
            [0x3ff0_0000, 0x000f_fc00, 0x0000_03ff, 0xc000_0000], // 10-bit
            [0x00ff_0000, 0x00ff_0000, 0x0000_00ff, 0],           // green not its own lane
            [0, 0, 0, 0],
        ] {
            assert_eq!(effective_pixel_format(p::BIT_MASK, masks), p::BIT_MASK);
        }
        // Formats that are not bitmasks pass through whatever the masks say.
        for format in [p::RGB_RESERVED_8, p::BGR_RESERVED_8, p::BLT_ONLY, 7] {
            assert_eq!(
                effective_pixel_format(format, [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0]),
                format
            );
        }
        // And a normalized mode is linear for the mode chooser.
        let mode = GopMode {
            number: 3,
            width: 1920,
            height: 1080,
            pixel_format: effective_pixel_format(
                p::BIT_MASK,
                [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, x],
            ),
        };
        assert!(mode.is_linear());
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
    fn reads_fdt_total_size() {
        // magic(4) + totalsize(4, BE) = 0x1234
        assert_eq!(
            fdt_total_size(&[0xd0, 0x0d, 0xfe, 0xed, 0x00, 0x00, 0x12, 0x34]),
            Some(0x1234)
        );
        assert_eq!(fdt_total_size(&[0xd0, 0x0d, 0xfe, 0xed]), None);
    }

    #[test]
    fn firmware_pointer_guard_rejects_sentinels_wrap_and_noncanonical_values() {
        assert!(is_plausible_firmware_pointer(0x4080_0000, 8, 8));
        assert!(!is_plausible_firmware_pointer(0, 8, 8));
        assert!(!is_plausible_firmware_pointer(u64::MAX, 8, 8));
        assert!(!is_plausible_firmware_pointer(0x4080_0001, 8, 8));
        assert!(!is_plausible_firmware_pointer(0x0001_0000_0000_0000, 8, 8));
        assert!(!is_plausible_firmware_pointer(0xffff_0000_0000_0000, 8, 8));
        assert!(!is_plausible_firmware_pointer(0xffff_ffff_ffff_fff8, 16, 8));
    }

    #[test]
    fn firmware_owned_memory_types_exclude_free_unusable_and_mmio_ranges() {
        assert!(efi_memory_type_is_firmware_owned(
            efi_memory_type::BOOT_SERVICES_DATA
        ));
        assert!(efi_memory_type_is_firmware_owned(
            efi_memory_type::RUNTIME_SERVICES_DATA
        ));
        assert!(efi_memory_type_is_firmware_owned(
            efi_memory_type::ACPI_RECLAIM
        ));
        assert!(!efi_memory_type_is_firmware_owned(
            efi_memory_type::CONVENTIONAL
        ));
        assert!(!efi_memory_type_is_firmware_owned(
            efi_memory_type::UNUSABLE
        ));
        assert!(!efi_memory_type_is_firmware_owned(efi_memory_type::MMIO));
        assert!(!efi_memory_type_is_firmware_owned(u32::MAX));
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
        assert_eq!(boot.platform.pl011_console_base, 0);
    }
}

#[cfg(test)]
mod framebuffer_tests {
    use super::*;
    fn framebuffer() -> Framebuffer {
        Framebuffer {
            phys: 0xed0c_0000,
            len: 0x57e400,
            width: 1600,
            height: 900,
            stride: 1600,
            format: FramebufferFormat::Bgr,
        }
    }
    fn descriptor(kind: u32, base: u64, pages: u64, attrs: u64) -> [u8; 40] {
        let mut out = [0; 40];
        out[..4].copy_from_slice(&kind.to_le_bytes());
        out[8..16].copy_from_slice(&base.to_le_bytes());
        out[24..32].copy_from_slice(&pages.to_le_bytes());
        out[32..40].copy_from_slice(&attrs.to_le_bytes());
        out
    }
    #[test]
    fn exact_gop_pages_include_only_unavoidable_padding() {
        let fb = framebuffer();
        let map = descriptor(0, fb.phys, 0x580, 8);
        assert_eq!(
            reserved_framebuffer_span(fb, &map, 40),
            Some(Range::new(fb.phys, 0x57f000))
        );
        assert_eq!(0x57f000 - fb.len, 0xc00);
        assert_eq!(
            reserved_framebuffer_span(fb, &descriptor(6, fb.phys, 0x580, 1 << 63), 40),
            Some(Range::new(fb.phys, 0x57f000))
        );
        assert!(reserved_framebuffer_span(fb, &descriptor(6, fb.phys, 0x580, 0), 40).is_none());
    }
    #[test]
    fn lossy_reserved_kinds_and_runtime_attributes_are_not_attestation() {
        let fb = framebuffer();
        for kind in (1..=16).filter(|&kind| kind != efi_memory_type::RUNTIME_SERVICES_DATA) {
            assert!(
                reserved_framebuffer_span(fb, &descriptor(kind, fb.phys, 0x580, 8), 40).is_none()
            );
        }
        assert!(
            reserved_framebuffer_span(fb, &descriptor(0, fb.phys, 0x580, 1 << 63), 40).is_none()
        );
    }
    #[test]
    fn incomplete_padding_conflicts_bad_strides_and_overflow_refuse() {
        let fb = framebuffer();
        for (base, pages) in [
            (fb.phys, 0x57e),
            (fb.phys + 4096, 0x580),
            (fb.phys + 1, 0x580),
            (fb.phys, u64::MAX),
            (u64::MAX - 4095, 2),
        ] {
            assert!(reserved_framebuffer_span(fb, &descriptor(0, base, pages, 8), 40).is_none());
        }
        let map = descriptor(0, fb.phys, 0x580, 8);
        let mut duplicate = [0; 80];
        duplicate[..40].copy_from_slice(&map);
        duplicate[40..].copy_from_slice(&map);
        assert!(reserved_framebuffer_span(fb, &duplicate, 40).is_none());
        for stride in [0, 1, 32, 39, 41] {
            assert!(reserved_framebuffer_span(fb, &map, stride).is_none());
        }
        assert!(reserved_framebuffer_span(fb, &map[..39], 40).is_none());
    }
    #[test]
    fn corrupt_geometry_never_creates_reserved_ram_evidence() {
        let fb = framebuffer();
        let map = descriptor(0, fb.phys, 0x580, 8);
        for bad in [
            Framebuffer { phys: 0, ..fb },
            Framebuffer {
                len: u64::MAX,
                ..fb
            },
            Framebuffer { stride: 1599, ..fb },
            Framebuffer {
                format: FramebufferFormat::Unknown,
                ..fb
            },
        ] {
            assert!(reserved_framebuffer_span(bad, &map, 40).is_none());
        }
    }
}

#[cfg(test)]
mod gop_mode_tests {
    use super::*;

    const BGR: u32 = efi_pixel_format::BGR_RESERVED_8;
    const BLT: u32 = efi_pixel_format::BLT_ONLY;

    fn mode(number: u32, width: u32, height: u32, pixel_format: u32) -> GopMode {
        GopMode {
            number,
            width,
            height,
            pixel_format,
        }
    }

    fn list() -> [GopMode; 7] {
        [
            mode(0, 640, 480, BGR),
            mode(1, 800, 600, BGR),
            mode(2, 1280, 720, BGR),
            mode(3, 1600, 900, BGR),
            mode(4, 1920, 1080, BGR),
            mode(5, 2560, 1440, BGR),
            mode(6, 3840, 2160, BGR),
        ]
    }

    #[test]
    fn firmware_mode_at_or_above_the_floor_is_kept() {
        // opi5's usual 1600x900, OVMF's 1280x800, 720p, the floor itself, the X13s panel and a
        // 4K sink's native mode (larger than the fallback cap, but the firmware chose it).
        for current in [
            mode(3, 1600, 900, BGR),
            mode(7, 1280, 800, BGR),
            mode(2, 1280, 720, BGR),
            mode(8, 1024, 768, BGR),
            mode(9, 1920, 1200, BGR),
            mode(6, 3840, 2160, BGR),
        ] {
            assert_eq!(choose_gop_mode(current, &list()), GopChoice::KeepFirmware);
        }
    }

    #[test]
    fn a_mode_below_the_floor_is_raised_like_the_vga_fallback() {
        // j910: the NUC7i5DNHE's Intel GOP left 800x600 on its first metal boot.
        assert_eq!(
            choose_gop_mode(mode(1, 800, 600, BGR), &list()),
            GopChoice::Switch(mode(4, 1920, 1080, BGR))
        );
        // A 1024x600 netbook panel and 960x640 sit under the floor's area too.
        assert_eq!(
            choose_gop_mode(mode(10, 1024, 600, BGR), &list()),
            GopChoice::Switch(mode(4, 1920, 1080, BGR))
        );
        // Nothing larger within the cap: the low mode stays.
        let low = [
            mode(1, 800, 600, BGR),
            mode(0, 640, 480, BGR),
            mode(6, 3840, 2160, BGR),
        ];
        assert_eq!(choose_gop_mode(low[0], &low), GopChoice::KeepNoBetter);
        // The floor decides what is kept, not what is reachable: an upgrade that still falls
        // short of it is taken when that is all the sink offers.
        let near = [mode(1, 800, 600, BGR), mode(2, 1000, 700, BGR)];
        assert_eq!(
            choose_gop_mode(near[0], &near),
            GopChoice::Switch(mode(2, 1000, 700, BGR))
        );
    }

    #[test]
    fn vga_fallback_becomes_the_largest_mode_within_1080p() {
        assert_eq!(
            choose_gop_mode(mode(0, 640, 480, BGR), &list()),
            GopChoice::Switch(mode(4, 1920, 1080, BGR))
        );
        // Without 1080p offered, the largest below it.
        let small = &list()[..4];
        assert_eq!(
            choose_gop_mode(mode(0, 640, 480, BGR), small),
            GopChoice::Switch(mode(3, 1600, 900, BGR))
        );
    }

    #[test]
    fn vga_fallback_with_nothing_better_is_kept() {
        let only = [mode(0, 640, 480, BGR)];
        assert_eq!(choose_gop_mode(only[0], &only), GopChoice::KeepNoBetter);
        // Only modes above the cap, or not linear: nothing within reach.
        let out = [
            mode(0, 640, 480, BGR),
            mode(1, 2560, 1440, BGR),
            mode(2, 1920, 1080, BLT),
        ];
        assert_eq!(choose_gop_mode(out[0], &out), GopChoice::KeepNoBetter);
        // Smaller modes never replace the fallback.
        let tiny = [mode(0, 640, 480, BGR), mode(1, 320, 240, BGR)];
        assert_eq!(choose_gop_mode(tiny[0], &tiny), GopChoice::KeepNoBetter);
    }

    #[test]
    fn non_linear_current_takes_any_linear_mode_within_the_cap() {
        let modes = [
            mode(0, 1024, 768, BLT),
            mode(1, 640, 480, BGR),
            mode(2, 800, 600, BGR),
        ];
        assert_eq!(
            choose_gop_mode(modes[0], &modes),
            GopChoice::Switch(mode(2, 800, 600, BGR))
        );
        // Even a VGA mode beats no linear buffer at all.
        let vga = [mode(0, 1024, 768, BLT), mode(1, 640, 480, BGR)];
        assert_eq!(
            choose_gop_mode(vga[0], &vga),
            GopChoice::Switch(mode(1, 640, 480, BGR))
        );
    }

    #[test]
    fn ties_take_the_lowest_mode_number_and_current_is_never_reset() {
        let modes = [
            mode(0, 640, 480, BGR),
            mode(1, 1920, 1080, BGR),
            mode(2, 1920, 1080, efi_pixel_format::RGB_RESERVED_8),
        ];
        assert_eq!(
            choose_gop_mode(modes[0], &modes),
            GopChoice::Switch(modes[1])
        );
        let reversed = [modes[0], modes[2], modes[1]];
        assert_eq!(
            choose_gop_mode(modes[0], &reversed),
            GopChoice::Switch(modes[1])
        );
    }
}
