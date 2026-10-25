//j597
//j611

//! D0a: read-only discovery of firmware-owned RK3588 VOP2 scanout. Recovery class:
//! soft-state; a fresh probe reconstructs this diagnostic snapshot. No mode is programmed.
//!
//! Register source: RK3588 TRM Part 1 ch. 7 (PMU), Part 2 ch. 7 (VOP2).
//! The Orange Pi DTB maps VOP2 at 0xfdd90000 and its two MMUs at +0x7e00/+0x7f00.

pub const VOP_BASE: u64 = 0xfdd9_0000;
pub const VOP_LEN: u64 = 0x8000;
/// VOP2's interrupt: GIC SPI 156, INTID 188, shared with the display IOMMU (DTB `interrupts`;
/// D0b checks the live blob). D1b receives it with the VOP window (J611).
pub const VOP_INTID: u32 = 188;
pub const PMU_BASE: u64 = 0xfd8d_8000;
pub const PMU_LEN: u64 = 0x1000;

/// Admission is deliberately board-specific and requires an existing, plausible GOP image.
/// This observation does not power up a display that firmware left unused. — TIMBERDOODLE
pub fn eligible(board: &str, framebuffer_valid: bool) -> bool {
    board == "orange-pi-5-plus" && framebuffer_valid
}

/// An implementation can observe registers, but cannot program them through this interface.
pub trait ReadRegisters {
    fn vop(&mut self, offset: u32) -> u32;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Power {
    pub gate_status: u32,
    pub idle_request: u32,
    pub idle_ack: u32,
    pub idle_status: u32,
}

impl Power {
    pub fn read(mut read: impl FnMut(u32) -> u32) -> Self {
        Self {
            gate_status: read(0x180),
            idle_request: read(0x10c),
            idle_ack: read(0x118),
            idle_status: read(0x120),
        }
    }

    /// Bootstrap record: four little-endian PMU observations, no hardware capability.
    pub fn encode(self) -> [u8; 16] {
        let mut bytes = [0; 16];
        for (index, word) in [
            self.gate_status,
            self.idle_request,
            self.idle_ack,
            self.idle_status,
        ]
        .into_iter()
        .enumerate()
        {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 16 {
            return None;
        }
        let words: [u32; 4] = core::array::from_fn(|i| {
            u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap())
        });
        Some(Self {
            gate_status: words[0],
            idle_request: words[1],
            idle_ack: words[2],
            idle_status: words[3],
        })
    }

    pub fn vop_accessible(self) -> bool {
        const PD_VOP: u32 = 1 << 17;
        const BIU_VOP: u32 = (1 << 13) | (1 << 14);
        self.gate_status & PD_VOP == 0
            && (self.idle_request | self.idle_ack | self.idle_status) & BIU_VOP == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Axis {
    pub total: u32,
    pub sync: u32,
    pub start: u32,
    pub end: u32,
}

impl Axis {
    fn decode(period: u32, active: u32) -> Option<Self> {
        let axis = Self {
            total: (period >> 16) & 0x1fff,
            sync: period & 0x1fff,
            start: (active >> 16) & 0x1fff,
            end: active & 0x1fff,
        };
        (axis.sync != 0
            && axis.sync <= axis.start
            && axis.start < axis.end
            && axis.end <= axis.total)
            .then_some(axis)
    }

    pub fn active(self) -> u32 {
        self.end - self.start
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Port {
    /// DSP_CTRL, CLK_CTRL, HTOTAL/HS_END, HACT, VTOTAL/VS_END, VACT.
    pub words: [u32; 6],
}

impl Port {
    pub fn timing(self) -> Option<(Axis, Axis)> {
        // D0a reports interlaced or standby ports as raw words only. — TIMBERDOODLE
        if self.words[0] & ((1 << 31) | (1 << 7)) != 0 {
            return None;
        }
        Some((
            Axis::decode(self.words[2], self.words[3])?,
            Axis::decode(self.words[4], self.words[5])?,
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Window {
    pub offset: u32,
    /// CTRL, YRGB_MST, VIR, ACT_INFO, DSP_INFO, DSP_OFFSET/ST.
    pub words: [u32; 6],
}

impl Window {
    pub fn enabled(self) -> bool {
        self.words[0] & 1 != 0
    }

    pub fn format(self) -> u32 {
        (self.words[0] >> 1) & 31
    }

    pub fn dimensions(self) -> (u32, u32) {
        (
            (self.words[3] & 0x1fff) + 1,
            ((self.words[3] >> 16) & 0x1fff) + 1,
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Snapshot {
    /// VERSION, IF_EN, IF_CTRL, IF_POL, POWER_CTRL, OVL_CTRL, LAYER_SEL, PORT_SEL.
    pub system: [u32; 8],
    pub ports: [Port; 4],
    /// Cluster0..3 win0/win1, followed by esmart0..3 region0..3.
    pub windows: [Window; 24],
    /// Each display MMU's DTE_ADDR and STATUS. No command or IRQ-clear registers are read.
    pub mmus: [[u32; 2]; 2],
}

impl Snapshot {
    /// Capture only after the always-on power/bus guard. A down or idle domain causes
    /// zero VOP reads, including zero reads of its MMU registers. — TIMBERDOODLE
    pub fn read(power: Power, io: &mut impl ReadRegisters) -> Option<Self> {
        if !power.vop_accessible() {
            return None;
        }
        Some(Self {
            system: [4, 0x28, 0x2c, 0x30, 0x34, 0x600, 0x604, 0x608].map(|o| io.vop(o)),
            ports: core::array::from_fn(|index| {
                let base = 0xc00 + index as u32 * 0x100;
                Port {
                    words: [0, 0xc, 0x48, 0x4c, 0x50, 0x54].map(|o| io.vop(base + o)),
                }
            }),
            windows: core::array::from_fn(|index| {
                let (base, offsets) = if index < 8 {
                    (
                        0x1000 + (index / 2) as u32 * 0x200 + (index % 2) as u32 * 0x80,
                        [0, 0x10, 0x18, 0x20, 0x24, 0x28],
                    )
                } else {
                    let region = index - 8;
                    (
                        0x1810 + (region / 4) as u32 * 0x200 + (region % 4) as u32 * 0x30,
                        [0, 4, 0xc, 0x10, 0x14, 0x18],
                    )
                };
                Window {
                    offset: base,
                    words: offsets.map(|o| io.vop(base + o)),
                }
            }),
            mmus: core::array::from_fn(|index| {
                let base = 0x7e00 + index as u32 * 0x100;
                [io.vop(base), io.vop(base + 4)]
            }),
        })
    }

    /// HDMI/eDP combo mux: mode 2/3 is HDMI, 1 is eDP; selectors 2/3 both mean VP2.
    pub fn hdmi_port(&self, output: usize) -> Option<usize> {
        if output >= 2 || (self.system[1] >> (2 + output * 2)) & 3 < 2 {
            return None;
        }
        Some((((self.system[1] >> (16 + output * 2)) & 3) as usize).min(2))
    }

    /// A geometry match is diagnostic evidence, not proof of output wiring or pixel clock.
    pub fn matching_hdmi(&self, width: u32, height: u32) -> Option<(usize, usize)> {
        let mut found = None;
        for output in 0..2 {
            let Some(port) = self.hdmi_port(output) else {
                continue;
            };
            let Some((h, v)) = self.ports[port].timing() else {
                continue;
            };
            if h.active() == width && v.active() == height {
                if found.is_some() {
                    return None;
                }
                found = Some((output, port));
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;
    extern crate std;

    struct Fake {
        reads: Vec<u32>,
    }
    impl ReadRegisters for Fake {
        fn vop(&mut self, offset: u32) -> u32 {
            self.reads.push(offset);
            offset
        }
    }
    fn powered() -> Power {
        Power {
            gate_status: 0,
            idle_request: 0,
            idle_ack: 0,
            idle_status: 0,
        }
    }

    #[test]
    fn only_the_board_with_a_valid_firmware_framebuffer_is_eligible() {
        assert!(eligible("orange-pi-5-plus", true));
        for board in [
            "",
            "qemu-virt-aarch64",
            "thinkpad-x13s-gen1",
            "generic-uefi-x86_64",
        ] {
            assert!(!eligible(board, true));
        }
        assert!(!eligible("orange-pi-5-plus", false));
    }

    #[test]
    fn power_or_bus_idle_prevents_every_vop_read() {
        for bit in [13, 14] {
            for field in 0..3 {
                let mut power = powered();
                match field {
                    0 => power.idle_request = 1 << bit,
                    1 => power.idle_ack = 1 << bit,
                    _ => power.idle_status = 1 << bit,
                }
                let mut io = Fake { reads: Vec::new() };
                assert!(Snapshot::read(power, &mut io).is_none());
                assert!(io.reads.is_empty());
            }
        }
        let mut io = Fake { reads: Vec::new() };
        assert!(Snapshot::read(
            Power {
                gate_status: 1 << 17,
                ..powered()
            },
            &mut io
        )
        .is_none());
        assert!(io.reads.is_empty());
        assert_eq!(
            Power::read(|offset| {
                io.reads.push(PMU_BASE as u32 + offset);
                0
            }),
            powered()
        );
        assert_eq!(Power::decode(&powered().encode()), Some(powered()));
        assert_eq!(Power::decode(&[0; 15]), None);
        assert_eq!(Power::decode(&[0; 17]), None);
        assert_eq!(io.reads, [0xfd8d8180, 0xfd8d810c, 0xfd8d8118, 0xfd8d8120]);
    }

    #[test]
    fn snapshot_reads_only_configuration_and_status_in_the_granted_aperture() {
        let mut io = Fake { reads: Vec::new() };
        let snapshot = Snapshot::read(powered(), &mut io).unwrap();
        assert_eq!(
            snapshot.ports[3].words,
            [0xf00, 0xf0c, 0xf48, 0xf4c, 0xf50, 0xf54]
        );
        assert_eq!(
            snapshot.windows[0].words,
            [0x1000, 0x1010, 0x1018, 0x1020, 0x1024, 0x1028]
        );
        assert_eq!(snapshot.windows[7].offset, 0x1680);
        assert_eq!(
            snapshot.windows[8].words,
            [0x1810, 0x1814, 0x181c, 0x1820, 0x1824, 0x1828]
        );
        assert_eq!(snapshot.windows[23].offset, 0x1ea0);
        assert_eq!(snapshot.mmus, [[0x7e00, 0x7e04], [0x7f00, 0x7f04]]);
        assert!(io.reads.iter().all(|&o| o < VOP_LEN as u32 && o % 4 == 0));
        assert_eq!(io.reads.len(), 180);
        assert!(!io.reads.iter().any(|&o| matches!(
            o,
            0 | 0x84 | 0x94 | 0xa4 | 0xb4 | 0xc4 | 0xd4 | 0xe4 | 0xf4 | 0x7e08 | 0x7f08
        )));
    }

    #[test]
    fn measured_firmware_mode_decodes_vp2_and_esmart2() {
        let mut io = Fake { reads: Vec::new() };
        let mut s = Snapshot::read(powered(), &mut io).unwrap();
        s.system[1] = 0x0002_0008;
        s.ports[2] = Port {
            words: [0xf, 2, 0x0708_0050, 0x00b0_06f0, 0x03e8_0003, 0x0063_03e7],
        };
        assert_eq!(s.matching_hdmi(1600, 900), Some((0, 2)));
        let (h, v) = s.ports[2].timing().unwrap();
        assert_eq!((h.total, h.sync, h.start, h.end), (1800, 80, 176, 1776));
        assert_eq!((v.total, v.sync, v.start, v.end), (1000, 3, 99, 999));
        let window = Window {
            offset: 0x1c10,
            words: [1, 0xed0c_0000, 1600, 0x0383_063f, 0x0383_063f, 0],
        };
        assert!(window.enabled());
        assert_eq!(window.format(), 0);
        assert_eq!(window.dimensions(), (1600, 900));
        let power = Power {
            gate_status: 0xfff8,
            idle_request: 0,
            idle_ack: 0xffe,
            idle_status: 0xffe,
        };
        assert!(power.vop_accessible());
        assert_eq!(Power::decode(&power.encode()), Some(power));
    }

    #[test]
    fn progressive_mode_and_hdmi_mux_must_agree_with_boot_geometry() {
        let mut io = Fake { reads: Vec::new() };
        let mut s = Snapshot::read(powered(), &mut io).unwrap();
        s.system[1] = 2 << 2;
        s.ports[0] = Port {
            words: [
                0xf,
                0,
                (800 << 16) | 96,
                (144 << 16) | 784,
                (525 << 16) | 2,
                (35 << 16) | 515,
            ],
        };
        assert_eq!(s.matching_hdmi(640, 480), Some((0, 0)));
        assert_eq!(s.matching_hdmi(800, 600), None);
        let (h, v) = s.ports[0].timing().unwrap();
        assert_eq!((h.total, h.sync, h.start, h.end), (800, 96, 144, 784));
        assert_eq!((v.total, v.sync, v.start, v.end), (525, 2, 35, 515));
        for bit in [7, 31] {
            let mut p = s.ports[0];
            p.words[0] |= 1 << bit;
            assert!(p.timing().is_none());
        }
        s.system[1] = 1 << 2;
        assert_eq!(s.hdmi_port(0), None);
        for sel in [2, 3] {
            s.system[1] = (sel << 16) | (2 << 2);
            assert_eq!(s.hdmi_port(0), Some(2));
        }
        assert_eq!(s.hdmi_port(2), None);
        assert!(Axis::decode((800 << 16) | 96, (790 << 16) | 784).is_none());
        assert!(Axis::decode((780 << 16) | 96, (144 << 16) | 784).is_none());
        assert!(Axis::decode(800 << 16, (144 << 16) | 784).is_none());
        s.system[1] = (2 << 2) | (2 << 4);
        assert_eq!(s.matching_hdmi(640, 480), None);
    }
}
