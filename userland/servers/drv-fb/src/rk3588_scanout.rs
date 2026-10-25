//j610

//! D1a: scan out a KUMO-owned buffer, then hand the glass back to the GOP console buffer.
//!
//! Recovery class: soft-state. The buffer is a physically contiguous DMA VMO
//! (`VmoCreateContiguous`), mapped uncached, so the VOP reads exactly what the CPU wrote once
//! a barrier completes; the display IOMMU stays in bypass, as firmware left it (DESIGN/019 D1
//! decides its owner later). A flip rewrites one word, ESMART2 region0's `MST_YRGB`, and
//! requests VP2's load with the global-enable commit word J608 proved. The VOP consumes the
//! request at the next frame start, so the flip is frame-synchronous and needs no interrupt
//! yet: [`wait_latched`] times the request's consumption. From that frame on the previous
//! buffer is no longer read, i.e. it is retired from scanout. Register sources: RK3588 TRM
//! Part 2 ch. 7 (`REG_CFG_DONE`, ESMART region0). — TIMBERDOODLE

use crate::rk3588_mode::{
    Block, Hardware, Program, ESMART2_R0_MST_YRGB, ESMART2_R0_VIR, REG_CFG_DONE,
};

/// `REG_CFG_DONE`: global enable (bit 15), VP2's load request (bit 2) and its write mask
/// (bit 18), the word J608 showed the VOP actually consumes.
pub const COMMIT_VP2: u32 = (1 << 18) | (1 << 15) | (1 << 2);
/// `REG_CFG_DONE` bit 2 reads 1 while VP2's load request is still pending.
pub const VP2_LOAD_PENDING: u32 = 1 << 2;
/// A frame at 60 Hz is 16.7 ms; a request still pending after three frames has failed.
pub const LATCH_TIMEOUT_NS: u64 = 50_000_000;
/// How long the owned buffer stays on the glass before the console returns.
pub const HOLD_MS: u64 = 5_000;
/// The scanout address is a 32-bit register.
pub const SCANOUT_LIMIT: u64 = 1 << 32;

/// One scanout buffer: ARGB8888, `stride` pixels per line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Buffer {
    pub phys: u64,
    pub len: u64,
    pub stride: u32,
    pub height: u32,
}

impl Buffer {
    /// The 32-bit scanout address, when the whole buffer is VOP-addressable and holds a frame.
    pub fn scanout(&self) -> Option<u32> {
        let frame = u64::from(self.stride) * u64::from(self.height) * 4;
        let end = self.phys.checked_add(self.len)?;
        (self.phys != 0 && self.phys % 4096 == 0 && end <= SCANOUT_LIMIT && self.len >= frame)
            .then_some(self.phys as u32)
    }
}

/// The scanout address and stride the target program uses (the GOP console buffer).
pub fn target_scanout(target: &Program) -> (u32, u32) {
    let word = |offset| {
        target
            .words
            .iter()
            .find(|word| word.offset == offset)
            .map_or(0, |word| word.value)
    };
    (word(ESMART2_R0_MST_YRGB), word(ESMART2_R0_VIR) & 0xffff)
}

/// Point ESMART2 region0 at `phys` and request VP2's load. Each write has its own barrier.
pub fn flip(hw: &mut impl Hardware, phys: u32) {
    hw.write(Block::Vop, ESMART2_R0_MST_YRGB, phys);
    hw.barrier();
    hw.write(Block::Vop, REG_CFG_DONE, COMMIT_VP2);
    hw.barrier();
}

/// Poll until VP2's load request is consumed. Returns the elapsed time, or `None` after
/// `timeout_ns`. `now` is the monotonic clock in nanoseconds.
pub fn wait_latched(
    hw: &mut impl Hardware,
    mut now: impl FnMut() -> u64,
    timeout_ns: u64,
) -> Option<u64> {
    let start = now();
    loop {
        let elapsed = now().saturating_sub(start);
        if hw.read(Block::Vop, REG_CFG_DONE) & VP2_LOAD_PENDING == 0 {
            return Some(elapsed);
        }
        if elapsed > timeout_ns {
            return None;
        }
    }
}

/// Whether `phys` is the latched scanout address and no load is pending.
pub fn on_glass(hw: &mut impl Hardware, phys: u32) -> bool {
    hw.read(Block::Vop, REG_CFG_DONE) & VP2_LOAD_PENDING == 0
        && hw.read(Block::Vop, ESMART2_R0_MST_YRGB) == phys
}

/// KUMO's test pattern: eight vertical bars (white, yellow, cyan, green, magenta, red, blue,
/// black) over the top three quarters, a left-to-right grey ramp below, inside an 8-pixel
/// white frame. It looks nothing like the console, so the flip is visible.
pub const fn pattern(x: u32, y: u32, width: u32, height: u32) -> (u8, u8, u8) {
    const BARS: [(u8, u8, u8); 8] = [
        (255, 255, 255),
        (255, 255, 0),
        (0, 255, 255),
        (0, 255, 0),
        (255, 0, 255),
        (255, 0, 0),
        (0, 0, 255),
        (0, 0, 0),
    ];
    if width == 0 || height == 0 {
        return (0, 0, 0);
    }
    if x < 8 || y < 8 || x + 8 >= width || y + 8 >= height {
        return (255, 255, 255);
    }
    if y < height * 3 / 4 {
        BARS[(x * 8 / width) as usize]
    } else {
        let level = (x * 255 / (width - 1)) as u8;
        (level, level, level)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rk3588_mode::{Framebuffer, DMT_1600X900_60_RB};
    extern crate std;
    use std::vec::Vec;

    /// A VOP whose region0 address latches when the clock passes the next frame start.
    struct Vop {
        mirror: u32,
        active: u32,
        pending: bool,
        now: u64,
        next_frame: u64,
        writes: Vec<(u32, u32)>,
        barriers: usize,
    }

    impl Vop {
        fn new(active: u32) -> Self {
            Self {
                mirror: active,
                active,
                pending: false,
                now: 0,
                next_frame: 7_000_000,
                writes: Vec::new(),
                barriers: 0,
            }
        }

        fn tick(&mut self) -> u64 {
            self.now += 250_000;
            if self.pending && self.now >= self.next_frame {
                self.active = self.mirror;
                self.pending = false;
            }
            while self.next_frame <= self.now {
                self.next_frame += 16_666_667;
            }
            self.now
        }
    }

    impl Hardware for Vop {
        fn read(&mut self, block: Block, offset: u32) -> u32 {
            assert_eq!(block, Block::Vop);
            match offset {
                REG_CFG_DONE => 0x8000 | if self.pending { VP2_LOAD_PENDING } else { 0 },
                ESMART2_R0_MST_YRGB => self.active,
                _ => 0,
            }
        }

        fn write(&mut self, block: Block, offset: u32, value: u32) {
            assert_eq!(block, Block::Vop);
            self.writes.push((offset, value));
            match offset {
                ESMART2_R0_MST_YRGB => self.mirror = value,
                REG_CFG_DONE if value & (1 << 15) != 0 && value & (1 << 18) != 0 => {
                    self.pending = value & VP2_LOAD_PENDING != 0;
                }
                _ => {}
            }
        }

        fn barrier(&mut self) {
            self.barriers += 1;
        }

        fn sleep_ms(&mut self, _ms: u64) {}
    }

    fn gop_target() -> Program {
        Program::build(
            DMT_1600X900_60_RB,
            Framebuffer {
                phys: 0xed0c_0000,
                width: 1600,
                height: 900,
                stride: 1600,
            },
        )
        .unwrap()
    }

    #[test]
    fn a_flip_writes_one_address_and_one_commit_each_behind_a_barrier() {
        let mut vop = Vop::new(0xed0c_0000);
        flip(&mut vop, 0x4000_0000);
        assert_eq!(
            vop.writes,
            [
                (ESMART2_R0_MST_YRGB, 0x4000_0000),
                (REG_CFG_DONE, 0x0004_8004)
            ]
        );
        assert_eq!(vop.barriers, 2);
        // The commit word carries the global enable J608 found missing from R1's 0x00040004.
        assert_eq!(COMMIT_VP2 & (1 << 15), 1 << 15);
    }

    #[test]
    fn the_new_address_reaches_the_glass_only_at_the_next_frame_start() {
        let mut vop = Vop::new(0xed0c_0000);
        flip(&mut vop, 0x4000_0000);
        assert!(!on_glass(&mut vop, 0x4000_0000));
        let start = vop.now;
        while vop.pending {
            vop.tick();
        }
        assert!(vop.now - start <= 16_666_667);
        assert!(on_glass(&mut vop, 0x4000_0000));
        assert!(!on_glass(&mut vop, 0xed0c_0000));
    }

    /// Consumes the pending load on the Nth `REG_CFG_DONE` read.
    struct LatchAt {
        vop: Vop,
        latch_on_read: u32,
        reads: u32,
    }

    impl Hardware for LatchAt {
        fn read(&mut self, block: Block, offset: u32) -> u32 {
            if offset == REG_CFG_DONE {
                self.reads += 1;
                if self.reads >= self.latch_on_read && self.vop.pending {
                    self.vop.active = self.vop.mirror;
                    self.vop.pending = false;
                }
            }
            self.vop.read(block, offset)
        }

        fn write(&mut self, block: Block, offset: u32, value: u32) {
            self.vop.write(block, offset, value);
        }

        fn barrier(&mut self) {}

        fn sleep_ms(&mut self, _ms: u64) {}
    }

    #[test]
    fn wait_latched_times_the_consumption_and_gives_up_after_the_timeout() {
        // One clock read per poll at 0.25 ms after the start read; the load is consumed on
        // the 29th poll, so 29 polls of elapsed time are reported.
        let mut hw = LatchAt {
            vop: Vop::new(0xed0c_0000),
            latch_on_read: 29,
            reads: 0,
        };
        flip(&mut hw, 0x4000_0000);
        let mut clock = 0u64;
        let result = wait_latched(
            &mut hw,
            || {
                clock += 250_000;
                clock
            },
            LATCH_TIMEOUT_NS,
        );
        assert_eq!(result, Some(29 * 250_000));
        assert!(on_glass(&mut hw, 0x4000_0000));

        // A request that is never consumed times out and leaves the old address latched.
        let mut stuck = Vop::new(0xed0c_0000);
        flip(&mut stuck, 0x4000_0000);
        let mut clock = 0u64;
        let result = wait_latched(
            &mut stuck,
            || {
                clock += 1_000_000;
                clock
            },
            LATCH_TIMEOUT_NS,
        );
        assert_eq!(result, None);
        assert!(!on_glass(&mut stuck, 0x4000_0000));
        assert!(on_glass_after_consume(&mut stuck));
    }

    /// The stuck fake still reads its old active address once the request is dropped.
    fn on_glass_after_consume(vop: &mut Vop) -> bool {
        vop.pending = false;
        on_glass(vop, 0xed0c_0000)
    }

    #[test]
    fn only_a_vop_addressable_frame_sized_buffer_is_accepted() {
        let good = Buffer {
            phys: 0x4000_0000,
            len: 0x60_0000,
            stride: 1600,
            height: 900,
        };
        assert_eq!(good.scanout(), Some(0x4000_0000));
        for bad in [
            Buffer { phys: 0, ..good },
            Buffer {
                phys: 0x4000_0800,
                ..good
            },
            Buffer {
                phys: 0xffc0_0000,
                ..good
            },
            Buffer {
                phys: 0x1_0000_0000,
                ..good
            },
            Buffer {
                len: 1600 * 900 * 4 - 4,
                ..good
            },
        ] {
            assert_eq!(bad.scanout(), None, "{bad:?}");
        }
        assert_eq!(target_scanout(&gop_target()), (0xed0c_0000, 1600));
    }

    #[test]
    fn the_pattern_is_framed_bars_over_a_ramp() {
        let (w, h) = (1600, 900);
        assert_eq!(pattern(0, 0, w, h), (255, 255, 255));
        assert_eq!(pattern(w - 1, h / 2, w, h), (255, 255, 255));
        assert_eq!(pattern(100, 100, w, h), (255, 255, 255));
        assert_eq!(pattern(300, 100, w, h), (255, 255, 0));
        assert_eq!(pattern(1500, 100, w, h), (0, 0, 0));
        assert_eq!(pattern(1300, 100, w, h), (0, 0, 255));
        let left = pattern(20, 800, w, h);
        let right = pattern(1580, 800, w, h);
        assert!(left.0 < 10 && right.0 > 240 && left.0 == left.1 && right.1 == right.2);
        // Every bar is distinct from the console's background and from its neighbours.
        for bar in 0..7u32 {
            let x = bar * 200 + 100;
            assert_ne!(pattern(x, 400, w, h), pattern(x + 200, 400, w, h));
        }
    }
}
