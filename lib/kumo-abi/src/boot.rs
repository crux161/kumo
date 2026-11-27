use core::marker::PhantomData;

//j467
//j480
//j622
//j932
//J947_r13_001

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawSlice<T> {
    pub ptr: u64,
    pub len: u64,
    marker: PhantomData<fn() -> T>,
}

impl<T> RawSlice<T> {
    pub const fn empty() -> Self {
        Self {
            ptr: 0,
            len: 0,
            marker: PhantomData,
        }
    }

    pub const fn from_raw_parts(ptr: u64, len: u64) -> Self {
        Self {
            ptr,
            len,
            marker: PhantomData,
        }
    }

    pub fn from_slice(slice: &[T]) -> Self {
        Self {
            ptr: slice.as_ptr() as u64,
            len: slice.len() as u64,
            marker: PhantomData,
        }
    }

    pub const fn is_empty(self) -> bool {
        self.ptr == 0 || self.len == 0
    }

    /// Interpret this bootloader-provided range as a slice.
    ///
    /// # Safety
    ///
    /// The caller must first validate the boot handoff and then ensure that
    /// `ptr..ptr + len * size_of::<T>()` is readable for the returned lifetime.
    pub unsafe fn as_slice<'a>(self) -> &'a [T] {
        if self.ptr == 0 || self.len == 0 {
            return &[];
        }

        unsafe { core::slice::from_raw_parts(self.ptr as *const T, self.len as usize) }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Range {
    pub start: u64,
    pub len: u64,
}

impl Range {
    pub const fn new(start: u64, len: u64) -> Self {
        Self { start, len }
    }

    pub const fn empty() -> Self {
        Self { start: 0, len: 0 }
    }

    pub const fn end(self) -> u64 {
        self.start.saturating_add(self.len)
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MemRegionKind {
    Usable = 0,
    Reserved = 1,
    Acpi = 2,
    Mmio = 3,
    Bootloader = 4,
    Kernel = 5,
    Initrd = 6,
    #[default]
    Unknown = u32::MAX,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MemRegion {
    pub range: Range,
    pub kind: MemRegionKind,
    pub _reserved: u32,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FramebufferFormat {
    Rgb = 0,
    Bgr = 1,
    #[default]
    Unknown = u32::MAX,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Framebuffer {
    pub phys: u64,
    pub len: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: FramebufferFormat,
}

impl Framebuffer {
    /// Page span needed to map the framebuffer while preserving [`Self::len`] as the exact
    /// scanout extent. VMAR mappings are page-granular even when GOP reports a visible byte
    /// length that is not (for example, 1600 × 900 × 4 on the Orange Pi 5 Plus).
    pub const fn mapping_len(&self) -> Option<u64> {
        const PAGE_MASK: u64 = 4095;
        if self.len == 0 {
            return None;
        }
        match self.len.checked_add(PAGE_MASK) {
            Some(end) => Some(end & !PAGE_MASK),
            None => None,
        }
    }

    /// Whether the geometry is self-consistent and within sane bounds — a guard against
    /// acting on a corrupt `BootInfo` read (e.g. a stale snapshot page on real hardware).
    /// It cannot prove the values are *correct*, only that they are plausible: a non-zero
    /// page-aligned scanout base, dimensions within a generous bound, a stride at least as
    /// wide as the visible width, and a byte length large enough to hold `height` rows of
    /// `stride` pixels. `stride` is in pixels (the GOP-native unit) and the renderer is
    /// 32-bpp, so a row is `stride * 4` bytes.
    pub const fn is_plausible(&self) -> bool {
        const MAX_DIM: u32 = 16384;
        const BYTES_PER_PIXEL: u64 = 4;
        self.phys != 0
            && (self.phys & 4095) == 0
            && self.width >= 1
            && self.width <= MAX_DIM
            && self.height >= 1
            && self.height <= MAX_DIM
            && self.stride >= self.width
            && self.stride <= MAX_DIM
            && self.len >= self.height as u64 * self.stride as u64 * BYTES_PER_PIXEL
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlatformTable {
    pub acpi_rsdp: u64,
    pub dtb: u64,
    /// Optional loader-selected PL011 console base. Zero preserves the board BSP's default;
    /// nonzero explicitly redirects the serial half of the dual-sink console. This is a boot-time
    /// override because PCIe-hosted UART addresses (notably Raspberry Pi 5 RP1 UART0) can move
    /// between firmware revisions and therefore are not stable BSP facts. — KESTREL 2026-07-17
    pub pl011_console_base: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootInfo {
    pub version: u32,
    pub flags: u32,
    pub mem_regions: RawSlice<MemRegion>,
    pub kernel_phys: Range,
    pub kernel_virt: Range,
    pub initrd: Range,
    pub framebuffer: Framebuffer,
    pub platform: PlatformTable,
    pub cmdline: RawSlice<u8>,
    pub verified_boot_sig: [u8; 64],
    /// The runtime board identity, null-padded (its `kumo_bsp::Board` / `imager::HardwareTarget`
    /// id string, e.g. `thinkpad-x13s-gen1`). Baked into the image for a specific board and set by
    /// the loader from the `nijigumo.conf` `board = <id>` key; empty = unknown (DESIGN/017 §4).
    /// The kernel resolves it with `kumo_bsp::Board::from_id`.
    pub board_id: [u8; 32],
    /// v4 (J947_r13_001): 32 bytes of firmware entropy (`EFI_RNG_PROTOCOL`) that seed the
    /// kernel CSPRNG, valid only with [`BootInfo::FLAG_RNG_SEED_PRESENT`]. The kernel takes it
    /// before anything else reads BootInfo and the snapshot userland sees always carries zeros
    /// here, so no process can read the seed. — TEAL
    pub rng_seed: [u8; 32],
}

impl BootInfo {
    pub const FLAG_FRAMEBUFFER_PRESENT: u32 = 1 << 0;
    pub const FLAG_VERIFIED_BOOT_PRESENT: u32 = 1 << 1;
    /// The loader checked the exact page-rounded GOP span against both the final
    /// EFI reserved allocation and GCD SystemMemory. Absent on older/unsupported loaders;
    /// Reserved alone is not RAM evidence. Layout/version remain compatible. — KESTREL
    pub const FLAG_FRAMEBUFFER_RESERVED_RAM: u32 = 1 << 2;
    /// [`BootInfo::rng_seed`] holds firmware entropy (J947_r13_001).
    pub const FLAG_RNG_SEED_PRESENT: u32 = 1 << 3;
    /// Length of the [`BootInfo::rng_seed`] byte field.
    pub const RNG_SEED_LEN: usize = 32;

    /// Length of the [`BootInfo::board_id`] byte field.
    pub const BOARD_ID_LEN: usize = 32;

    pub const fn empty(version: u32) -> Self {
        Self {
            version,
            flags: 0,
            mem_regions: RawSlice::empty(),
            kernel_phys: Range::empty(),
            kernel_virt: Range::empty(),
            initrd: Range::empty(),
            framebuffer: Framebuffer {
                phys: 0,
                len: 0,
                width: 0,
                height: 0,
                stride: 0,
                format: FramebufferFormat::Unknown,
            },
            platform: PlatformTable {
                acpi_rsdp: 0,
                dtb: 0,
                pl011_console_base: 0,
            },
            cmdline: RawSlice::empty(),
            verified_boot_sig: [0; 64],
            board_id: [0; Self::BOARD_ID_LEN],
            rng_seed: [0; Self::RNG_SEED_LEN],
        }
    }

    /// Whether the loader passed firmware entropy in [`BootInfo::rng_seed`].
    pub const fn has_rng_seed(self) -> bool {
        self.flags & Self::FLAG_RNG_SEED_PRESENT != 0
    }

    /// Drop the seed from this copy: zero bytes, flag cleared. The kernel does this to every
    /// BootInfo it hands on, so the seed exists only in the loader's own memory and the CSPRNG.
    pub fn forget_rng_seed(&mut self) {
        self.rng_seed = [0; Self::RNG_SEED_LEN];
        self.flags &= !Self::FLAG_RNG_SEED_PRESENT;
    }

    pub const fn has_framebuffer(self) -> bool {
        self.flags & Self::FLAG_FRAMEBUFFER_PRESENT != 0
    }

    pub const fn has_verified_boot_sig(self) -> bool {
        self.flags & Self::FLAG_VERIFIED_BOOT_PRESENT != 0
    }

    /// Stamp the runtime board identity (null-padded, truncated to [`BootInfo::BOARD_ID_LEN`]).
    /// The loader calls this from the baked `nijigumo.conf` `board = <id>` key.
    pub fn set_board_id(&mut self, id: &str) {
        self.board_id = [0; Self::BOARD_ID_LEN];
        let bytes = id.as_bytes();
        let n = bytes.len().min(Self::BOARD_ID_LEN);
        self.board_id[..n].copy_from_slice(&bytes[..n]);
    }

    /// The stamped board identity as a `&str` (empty when unset), ready for
    /// `kumo_bsp::Board::from_id`.
    pub fn board_id(&self) -> &str {
        let end = self
            .board_id
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(Self::BOARD_ID_LEN);
        core::str::from_utf8(&self.board_id[..end]).unwrap_or("")
    }
}

/// j932 (SH8-f): the most boot command-line bytes the kernel copies into Sora's one-page
/// BootInfo snapshot. The loader's bytes live in its own allocation; the snapshot cannot point
/// there, so the kernel copies them to [`SNAPSHOT_CMDLINE_OFFSET`] and the snapshot's
/// `cmdline` carries that page offset and the length instead of a pointer. — TEAL
pub const CMDLINE_MAX: usize = 256;
/// j932: where in the snapshot page the copied command line starts.
pub const SNAPSHOT_CMDLINE_OFFSET: usize = 2048;
const _: () = assert!(core::mem::size_of::<BootInfo>() <= SNAPSHOT_CMDLINE_OFFSET);
const _: () = assert!(SNAPSHOT_CMDLINE_OFFSET + CMDLINE_MAX <= 4096);

/// j932 (SH8-f, D2): the root filesystem the boot command line names, `root=uuid:<hex>`: one
/// space-separated word, 32 hex digits (hyphens ignored, as in a printed UUID). Any other
/// spelling names no root; a second `root=` word makes the line ambiguous and names none.
pub fn root_uuid(cmdline: &[u8]) -> Option<[u8; 16]> {
    let mut found = None;
    for word in cmdline.split(|b| b.is_ascii_whitespace()) {
        let Some(value) = word.strip_prefix(b"root=") else {
            continue;
        };
        if found.is_some() {
            return None;
        }
        found = Some(parse_uuid(value.strip_prefix(b"uuid:")?)?);
    }
    found
}

/// 32 hex digits, hyphens anywhere ignored, to 16 bytes.
pub fn parse_uuid(text: &[u8]) -> Option<[u8; 16]> {
    let mut out = [0u8; 16];
    let mut digits = 0usize;
    for &c in text {
        let nibble = match c {
            b'-' => continue,
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return None,
        };
        if digits == 32 {
            return None;
        }
        out[digits / 2] |= if digits % 2 == 0 { nibble << 4 } else { nibble };
        digits += 1;
    }
    (digits == 32).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ABI_VERSION;

    #[test]
    fn root_uuid_reads_one_root_word() {
        let hex = b"00112233445566778899aabbccddeeff";
        let mut line = b"loglevel=6 root=uuid:".to_vec();
        line.extend_from_slice(hex);
        line.extend_from_slice(b" quiet");
        let want = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        assert_eq!(root_uuid(&line), Some(want));
        assert_eq!(
            root_uuid(b"root=uuid:00112233-4455-6677-8899-AABBCCDDEEFF"),
            Some(want),
            "hyphens and upper case are a printed UUID"
        );
    }

    #[test]
    fn root_uuid_names_nothing_when_unclear() {
        assert_eq!(root_uuid(b""), None);
        assert_eq!(root_uuid(b"quiet"), None);
        assert_eq!(
            root_uuid(b"root=/dev/disk5s1"),
            None,
            "positional names are refused"
        );
        assert_eq!(root_uuid(b"root=uuid:0011"), None, "too short");
        assert_eq!(
            root_uuid(b"root=uuid:00112233445566778899aabbccddeeff00"),
            None,
            "too long"
        );
        assert_eq!(
            root_uuid(b"root=uuid:00112233445566778899aabbccddeefg"),
            None
        );
        assert_eq!(
            root_uuid(b"root=uuid:00112233445566778899aabbccddeeff root=uuid:ffeeddccbbaa99887766554433221100"),
            None,
            "two roots are ambiguous"
        );
    }

    #[test]
    fn snapshot_cmdline_fits_after_the_struct() {
        assert!(core::mem::size_of::<BootInfo>() <= SNAPSHOT_CMDLINE_OFFSET);
        assert!(SNAPSHOT_CMDLINE_OFFSET + CMDLINE_MAX <= 4096);
    }

    #[test]
    fn empty_boot_info_is_unambiguous() {
        let boot = BootInfo::empty(ABI_VERSION);
        assert_eq!(boot.version, ABI_VERSION);
        assert!(boot.initrd.is_empty());
        assert!(!boot.has_framebuffer());
        assert_eq!(boot.board_id(), "", "an unstamped board id reads as empty");
    }

    #[test]
    fn board_id_round_trips_and_restamps_cleanly() {
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.set_board_id("thinkpad-x13s-gen1");
        assert_eq!(boot.board_id(), "thinkpad-x13s-gen1");
        // Re-stamping a shorter id must not leave a stale tail.
        boot.set_board_id("raspberry-pi-5");
        assert_eq!(boot.board_id(), "raspberry-pi-5");
    }

    #[test]
    fn board_id_truncates_rather_than_overflowing() {
        let mut boot = BootInfo::empty(ABI_VERSION);
        let long = "x".repeat(BootInfo::BOARD_ID_LEN + 8);
        boot.set_board_id(&long);
        // Stored value is bounded by the field and stays valid UTF-8.
        assert_eq!(boot.board_id().len(), BootInfo::BOARD_ID_LEN);
    }

    #[test]
    fn raw_slice_can_point_at_static_data() {
        static REGIONS: [MemRegion; 1] = [MemRegion {
            range: Range {
                start: 0x1000,
                len: 0x2000,
            },
            kind: MemRegionKind::Usable,
            _reserved: 0,
        }];
        let raw = RawSlice::from_slice(&REGIONS);
        let slice = unsafe { raw.as_slice() };
        assert_eq!(slice, &REGIONS);
    }

    #[test]
    fn framebuffer_plausibility_rejects_garbage_and_accepts_real_geometry() {
        // The exact corrupt geometry from an X13s boot's stale BootInfo snapshot read
        // (the drv-fb resource-fail log): every field is wild, so it must be rejected.
        let garbage = Framebuffer {
            phys: 0xeef9_d05f_b2a8_3610,
            len: 0xa50e_cba3_ec34_d44c,
            width: 0x6f3a_5649,
            height: 0x91a7_b487,
            stride: 0xb8dc_4268,
            format: FramebufferFormat::Unknown,
        };
        assert!(!garbage.is_plausible());

        // A real 1280x800 32-bpp scanout, page-aligned and tightly packed: accepted.
        let real = Framebuffer {
            phys: 0x8000_0000,
            len: 800 * 1280 * 4,
            width: 1280,
            height: 800,
            stride: 1280,
            format: FramebufferFormat::Bgr,
        };
        assert!(real.is_plausible());
        assert_eq!(real.mapping_len(), Some(real.len));

        // The opi5's 1600×900 GOP span ends one quarter into its final page. Mapping must
        // cover that page, while Framebuffer::len continues to describe visible scanout bytes.
        let opi5 = Framebuffer {
            len: 1600 * 900 * 4,
            width: 1600,
            height: 900,
            stride: 1600,
            ..real
        };
        assert!(opi5.is_plausible());
        assert_eq!(opi5.len, 5_760_000);
        assert_eq!(opi5.mapping_len(), Some(5_763_072));
        assert_eq!(Framebuffer::default().mapping_len(), None);
        assert_eq!(
            Framebuffer {
                len: u64::MAX,
                ..real
            }
            .mapping_len(),
            None
        );

        // Each invariant rejects on its own: a non-page-aligned base, an over-large
        // dimension, a stride narrower than the width, and a length too small for
        // height*stride*4.
        assert!(!Framebuffer {
            phys: 0x8000_0001,
            ..real
        }
        .is_plausible());
        assert!(!Framebuffer {
            width: 20000,
            ..real
        }
        .is_plausible());
        assert!(!Framebuffer {
            stride: 1279,
            ..real
        }
        .is_plausible());
        assert!(!Framebuffer {
            len: real.len - 4,
            ..real
        }
        .is_plausible());

        // A zero-flagged framebuffer (no display) is not plausible either.
        assert!(!Framebuffer::default().is_plausible());
    }
}
