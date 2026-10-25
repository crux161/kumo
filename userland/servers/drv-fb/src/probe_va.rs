//j601

//! Fixed user virtual-address windows for drv-fb's D0 display probes.
//!
//! drv-fb has no `VmarUnmap`; every probe mapping lives for the rest of the process. The D0
//! hands ran as three parallel slices, and the integrated tree mapped D0c's 0x31000-byte CRU
//! span at `0x1502_0000` while D0d tried to put HDMI0 at the same base, so `VmarMap` refused
//! it and the probe could report only "mapping failed" (J601). Keep every window disjoint,
//! 64 KiB aligned, and inside the child VMAR's fixed region; the host test below holds the
//! layout. — TIMBERDOODLE

/// D0a's VOP aperture (J597): `VOP_LEN` bytes.
pub const VOP_D0A: u64 = 0x1501_0000;

/// D0c's CRU span (J599): `CRU_LEN` bytes, reaching `0x1505_1000`; the slot is reserved whole.
pub const CRU_D0C: u64 = 0x1502_0000;

/// D0d's HDMI0 transmitter aperture (J600).
pub const HDMI_D0D: u64 = 0x1506_0000;

/// D0d's HDMI0 combo-PHY aperture.
pub const PHY_D0D: u64 = 0x1507_0000;

/// D0d's HDPTX PHY0 GRF page.
pub const GRF_D0D: u64 = 0x1508_0000;

/// D0b's private VOP view (J598), used to re-read the routing after D0a closes its handle.
pub const VOP_D0B: u64 = 0x1509_0000;

/// D0b's live-DTB copy, worst case `MAX_DTB_BYTES` (16 MiB).
pub const DTB_D0B: u64 = 0x1600_0000;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{dtb_topology, rk3588, rk3588_clock, rk3588_hdmi};

    /// `(base, mapped length)` for every fixed probe window.
    const WINDOWS: [(u64, u64); 7] = [
        (VOP_D0A, rk3588::VOP_LEN),
        (CRU_D0C, rk3588_clock::CRU_LEN),
        (HDMI_D0D, rk3588_hdmi::HDMI_LEN),
        (PHY_D0D, rk3588_hdmi::PHY_LEN),
        (GRF_D0D, rk3588_hdmi::PHY_GRF_MAP_LEN),
        (VOP_D0B, rk3588::VOP_LEN),
        (DTB_D0B, dtb_topology::MAX_DTB_BYTES as u64),
    ];

    #[test]
    fn probe_windows_are_disjoint_aligned_and_inside_the_child_vmar() {
        for (index, &(base, len)) in WINDOWS.iter().enumerate() {
            let end = base.checked_add(len).expect("window end");
            assert!(len != 0, "window {index} is empty");
            assert_eq!(base % 0x1_0000, 0, "window {index} is not 64 KiB aligned");
            // drv-fb's fixed mappings below the probes: BootInfo at 0x1000_0000, the
            // framebuffer at 0x1100_0000 and the shadow within one 16 MiB slot of it.
            assert!(
                base >= 0x1500_0000,
                "window {index} reaches into the framebuffer"
            );
            // A child VMAR is 512 MiB from 0; the kernel-chosen surface mappings share it.
            assert!(end <= 0x2000_0000, "window {index} leaves the child VMAR");
            for (other, &(other_base, other_len)) in WINDOWS.iter().enumerate().skip(index + 1) {
                let other_end = other_base.checked_add(other_len).expect("window end");
                assert!(
                    end <= other_base || other_end <= base,
                    "windows {index} and {other} overlap"
                );
            }
        }
    }
}
