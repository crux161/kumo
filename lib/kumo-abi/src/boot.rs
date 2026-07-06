use core::marker::PhantomData;

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
}

impl BootInfo {
    pub const FLAG_FRAMEBUFFER_PRESENT: u32 = 1 << 0;
    pub const FLAG_VERIFIED_BOOT_PRESENT: u32 = 1 << 1;

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
            },
            cmdline: RawSlice::empty(),
            verified_boot_sig: [0; 64],
            board_id: [0; Self::BOARD_ID_LEN],
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ABI_VERSION;

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
