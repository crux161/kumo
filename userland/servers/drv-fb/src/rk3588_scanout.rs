//j612
//j632
//j634
//j635
//j636

//! D1a/D1c: owned scanout, alternating buffers and consumed-load retirement.
//!
//! Recovery class: soft-state. The buffer's address is a device address, backed by an
//! explicitly prepared READ mapping in the kernel-owned DeviceCtx. CPU painting uses an
//! uncached mapping of the actual VMO, so DMA sees completed writes after a barrier.
//! The historical trusted bypass diagnostic used physical addresses in this field.
//! A flip rewrites ESMART2 region0's `MST_YRGB` and requests the consumed VP2 load.
//! — KESTREL
//!
//! The VOP consumes the request at the next frame start, so the flip is frame-synchronous
//! and needs no interrupt
//! yet: [`wait_latched`] times the request's consumption. From that frame on the previous
//! buffer is no longer read, i.e. it is retired from scanout. Register sources: RK3588 TRM
//! Part 2 ch. 7 (`REG_CFG_DONE`, ESMART region0). — TIMBERDOODLE
//!
//! J612's [`Pair`] blocks repainting until the replacement address is active and its load
//! consumed. Delayed/coalesced IRQs do not advance the state. The console can replace an
//! uncertain request, but both owned buffers stay protected until it is proven. — KESTREL

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
/// Internal bootstrap selector for the actual sole-context-holder death proof.
pub const HOLDER_ARG: u64 = 0x44314448;
/// The scanout address is a 32-bit register.
pub const SCANOUT_LIMIT: u64 = 1 << 32;

/// One scanout buffer: ARGB8888, `stride` pixels per line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Buffer {
    pub address: u64,
    pub len: u64,
    pub stride: u32,
    pub height: u32,
}

impl Buffer {
    /// The 32-bit scanout address, when the whole buffer is VOP-addressable and holds a frame.
    pub fn scanout(&self) -> Option<u32> {
        let frame = u64::from(self.stride) * u64::from(self.height) * 4;
        let end = self.address.checked_add(self.len)?;
        (self.address != 0 && self.address % 4096 == 0 && end <= SCANOUT_LIMIT && self.len >= frame)
            .then_some(self.address as u32)
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

/// Point ESMART2 region0 at `address` and request VP2's load. Each write has its own barrier.
pub fn flip(hw: &mut impl Hardware, address: u32) {
    hw.write(Block::Vop, ESMART2_R0_MST_YRGB, address);
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

/// Whether `address` is the latched scanout address and no load is pending.
pub fn on_glass(hw: &mut impl Hardware, address: u32) -> bool {
    hw.read(Block::Vop, REG_CFG_DONE) & VP2_LOAD_PENDING == 0
        && hw.read(Block::Vop, ESMART2_R0_MST_YRGB) == address
}

/// The third address in [`Pair`] is the existing console, never an owned painting target.
pub const CONSOLE: usize = 2;
pub const ALTERNATIONS: u32 = 24;
pub const FRAME_HOLD_NS: u64 = 250_000_000;
pub const ALTERNATION_WATCHDOG_MS: u64 = 15_000;

/// D1c's fixed pair, with at most one outstanding load. An IRQ is only a reason to call
/// `observe`: retirement requires the requested address AND a consumed VP2 load. While
/// a load is pending, neither its target nor the last confirmed front can be repainted.
/// Recovery class: soft-state; restore the console before abandoning the sequence. — KESTREL
#[derive(Debug)]
pub struct Pair {
    addresses: [u32; 3],
    front: usize,
    pending: Option<usize>,
    confirmed: u32,
    retirements: [u32; 2],
}

impl Pair {
    /// The caller must first confirm that `console` is on the glass.
    pub fn new(buffers: [Buffer; 2], console: u32) -> Option<Self> {
        let first = buffers[0].scanout()?;
        let second = buffers[1].scanout()?;
        if buffers[0].stride == 0
            || buffers[0].height == 0
            || buffers[0].stride != buffers[1].stride
            || buffers[0].height != buffers[1].height
        {
            return None;
        }
        let console_buffer = Buffer {
            address: u64::from(console),
            len: u64::from(buffers[0].stride) * u64::from(buffers[0].height) * 4,
            ..buffers[0]
        };
        console_buffer.scanout()?;
        let overlap =
            |a: Buffer, b: Buffer| a.address < b.address + b.len && b.address < a.address + a.len;
        if overlap(buffers[0], buffers[1]) || buffers.iter().any(|&b| overlap(b, console_buffer)) {
            return None;
        }
        Some(Self {
            addresses: [first, second, console],
            front: CONSOLE,
            pending: None,
            confirmed: 0,
            retirements: [0; 2],
        })
    }

    pub fn front(&self) -> usize {
        self.front
    }

    pub fn pending(&self) -> Option<usize> {
        self.pending
    }

    pub fn confirmed(&self) -> u32 {
        self.confirmed
    }

    pub fn retirements(&self) -> [u32; 2] {
        self.retirements
    }

    pub fn can_paint(&self, slot: usize) -> bool {
        slot < 2 && slot != self.front && self.pending.is_none()
    }

    /// The caller paints only after `can_paint`, then barriers before staging.
    pub fn stage(&mut self, hw: &mut impl Hardware, slot: usize) -> bool {
        if self.pending.is_some() || !self.can_paint(slot) {
            return false;
        }
        flip(hw, self.addresses[slot]);
        self.pending = Some(slot);
        true
    }

    /// Observe a consumed load, returning (retired slot, new front). Repeated or coalesced
    /// notifications cannot retire a second buffer without another consumed load. — KESTREL
    pub fn observe(&mut self, hw: &mut impl Hardware, dma_ready: bool) -> Option<(usize, usize)> {
        if !dma_ready {
            return None;
        }
        let target = self.pending?;
        if !on_glass(hw, self.addresses[target]) {
            return None;
        }
        let retired = self.front;
        self.front = target;
        self.pending = None;
        if target != CONSOLE {
            self.confirmed += 1;
        }
        if retired != CONSOLE {
            self.retirements[retired] += 1;
        }
        Some((retired, target))
    }

    /// Also allowed during a stuck/uncertain load. Neither owned buffer is paintable until
    /// the console is confirmed, since the displaced request may already have latched.
    pub fn restore_console(&mut self, hw: &mut impl Hardware) {
        flip(hw, self.addresses[CONSOLE]);
        self.pending = Some(CONSOLE);
    }
}

/// Distinct A/B palettes plus an eight-bit generation marker, rewritten only after retirement.
pub fn alternating_pattern(
    slot: usize,
    generation: u32,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> (u8, u8, u8) {
    if (16..144).contains(&x) && (16..48).contains(&y) {
        return if generation & (1 << ((x - 16) / 16)) != 0 {
            (255, 255, 255)
        } else {
            (0, 0, 0)
        };
    }
    let (r, g, b) = pattern(x, y, width, height);
    if slot == 0 {
        (r, g, b)
    } else {
        (255 - r, 255 - g, 255 - b)
    }
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

    fn pair_buffers() -> [Buffer; 2] {
        [0x4000_0000, 0x4100_0000].map(|address| Buffer {
            address,
            len: 0x60_0000,
            stride: 1600,
            height: 900,
        })
    }

    #[test]
    fn fault_only_observation_cannot_retire_even_a_consumed_address() {
        let mut hw = Vop::new(0xed0c0000);
        let buffers = pair_buffers();
        let mut pair = Pair::new(buffers, 0xed0c0000).unwrap();
        assert!(pair.stage(&mut hw, 0));
        hw.active = buffers[0].address as u32;
        hw.pending = false;
        let before = pair.retirements();
        assert_eq!(pair.observe(&mut hw, false), None);
        assert_eq!(pair.retirements(), before);
        assert_eq!(pair.front(), CONSOLE);
        assert!(pair.pending().is_some());
    }
    #[test]
    fn a_pair_refuses_aliasing_console_overlap_or_incompatible_geometry() {
        let good = pair_buffers();
        assert!(Pair::new(good, 0xed0c_0000).is_some());
        assert!(Pair::new([good[0]; 2], 0xed0c_0000).is_none());
        let mut overlap = good;
        overlap[1].address = overlap[0].address + 4096;
        assert!(Pair::new(overlap, 0xed0c_0000).is_none());
        assert!(Pair::new(good, good[0].address as u32 + 4096).is_none());
        assert!(Pair::new(good, good[0].address as u32 - 4096).is_none());
        let mut wrong = good;
        wrong[1].stride += 1;
        assert!(Pair::new(wrong, 0xed0c_0000).is_none());
        wrong = good;
        wrong[1].height -= 1;
        assert!(Pair::new(wrong, 0xed0c_0000).is_none());
        assert!(Pair::new(good, 0).is_none());
    }

    #[test]
    fn repeated_irqs_and_even_equal_address_readback_cannot_retire_a_pending_load() {
        let mut pair = Pair::new(pair_buffers(), 0xed0c_0000).unwrap();
        let mut hw = Vop::new(0xed0c_0000);
        assert!(pair.stage(&mut hw, 0));
        hw.active = pair_buffers()[0].address as u32;
        for _ in 0..20 {
            assert_eq!(pair.observe(&mut hw, true), None);
            assert!(!pair.can_paint(0) && !pair.can_paint(1));
            assert!(!pair.stage(&mut hw, 1));
        }
        assert_eq!(pair.confirmed(), 0);
        hw.pending = false;
        assert_eq!(pair.observe(&mut hw, true), Some((CONSOLE, 0)));
        assert!(!pair.can_paint(0) && pair.can_paint(1));
        assert_eq!(pair.observe(&mut hw, true), None);
        assert_eq!(pair.confirmed(), 1);
    }

    #[test]
    fn a_consumed_request_with_the_wrong_address_does_not_release_either_buffer() {
        let mut pair = Pair::new(pair_buffers(), 0xed0c_0000).unwrap();
        let mut hw = Vop::new(0xed0c_0000);
        assert!(pair.stage(&mut hw, 0));
        hw.pending = false;
        hw.active = pair_buffers()[1].address as u32;
        assert_eq!(pair.observe(&mut hw, true), None);
        assert_eq!(pair.pending(), Some(0));
        assert!(!pair.can_paint(0) && !pair.can_paint(1));
        assert_eq!(pair.retirements(), [0, 0]);
    }

    #[test]
    fn two_buffers_are_repainted_only_after_replacement_even_with_coalesced_irqs() {
        let buffers = pair_buffers();
        let mut pair = Pair::new(buffers, 0xed0c_0000).unwrap();
        let mut hw = Vop::new(0xed0c_0000);
        let mut painted = [0; 2];
        for generation in 0..ALTERNATIONS {
            let slot = generation as usize % 2;
            assert!(pair.can_paint(slot));
            assert_ne!(
                hw.active, buffers[slot].address as u32,
                "painting live scanout"
            );
            painted[slot] += 1;
            assert!(pair.stage(&mut hw, slot));
            assert!(!pair.stage(&mut hw, 1 - slot));
            assert_eq!(pair.observe(&mut hw, true), None);
            // Several frame starts before one delivery still acknowledge exactly one load.
            for _ in 0..(80 + generation) {
                hw.tick();
            }
            let old = if generation == 0 { CONSOLE } else { 1 - slot };
            assert_eq!(pair.observe(&mut hw, true), Some((old, slot)));
            assert_eq!(pair.confirmed(), generation + 1);
            assert_eq!(pair.observe(&mut hw, true), None);
        }
        pair.restore_console(&mut hw);
        assert!(!pair.can_paint(0) && !pair.can_paint(1));
        while hw.pending {
            hw.tick();
        }
        assert_eq!(pair.observe(&mut hw, true), Some((1, CONSOLE)));
        assert_eq!(painted, [12, 12]);
        assert_eq!(pair.retirements(), [12, 12]);
        assert!(pair.can_paint(0) && pair.can_paint(1));
    }

    #[test]
    fn aborting_an_unobserved_flip_protects_both_buffers_until_console_is_proven() {
        for consume_first in [false, true] {
            let mut pair = Pair::new(pair_buffers(), 0xed0c_0000).unwrap();
            let mut hw = Vop::new(0xed0c_0000);
            assert!(pair.stage(&mut hw, 0));
            if consume_first {
                while hw.pending {
                    hw.tick();
                }
            }
            pair.restore_console(&mut hw);
            assert_eq!(pair.observe(&mut hw, true), None);
            assert!(!pair.can_paint(0) && !pair.can_paint(1));
            while hw.pending {
                hw.tick();
            }
            assert!(pair.observe(&mut hw, true).is_some());
            assert_eq!(pair.front(), CONSOLE);
            assert!(pair.can_paint(0) && pair.can_paint(1));
        }
    }

    #[test]
    fn alternating_frames_change_both_palette_and_generation_marker() {
        assert_ne!(
            alternating_pattern(0, 0, 300, 100, 1600, 900),
            alternating_pattern(1, 0, 300, 100, 1600, 900)
        );
        assert_ne!(
            alternating_pattern(0, 0, 20, 20, 1600, 900),
            alternating_pattern(0, 1, 20, 20, 1600, 900)
        );
        assert_eq!(alternating_pattern(0, 0, 0, 0, 1600, 900), (255, 255, 255));
        assert_eq!(alternating_pattern(1, 0, 0, 0, 1600, 900), (0, 0, 0));
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
            address: 0x4000_0000,
            len: 0x60_0000,
            stride: 1600,
            height: 900,
        };
        assert_eq!(good.scanout(), Some(0x4000_0000));
        for bad in [
            Buffer { address: 0, ..good },
            Buffer {
                address: 0x4000_0800,
                ..good
            },
            Buffer {
                address: 0xffc0_0000,
                ..good
            },
            Buffer {
                address: 0x1_0000_0000,
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

/// Server-placed, unscaled native image in the already-owned known mode. Timing,
/// output selection, format, clocks, MMUs and bypass fields stay with their owners. — KESTREL
pub fn image_program(target: &Program, image: kumo_graphics::native::Image) -> Option<Program> {
    use crate::rk3588_mode::{
        DMT_1600X900_60_RB as MODE, ESMART2_R0_ACT_INFO, ESMART2_R0_DSP_INFO, ESMART2_R0_DSP_OFFSET,
    };
    if !image.valid() {
        return None;
    }
    let mut program = *target;
    let size = (image.surface.height - 1) << 16 | (image.surface.width - 1);
    for word in &mut program.words {
        word.value = match word.offset {
            ESMART2_R0_MST_YRGB => kumo_graphics::native::IMAGE_IOVA as u32,
            ESMART2_R0_VIR => image.surface.stride,
            ESMART2_R0_ACT_INFO | ESMART2_R0_DSP_INFO => size,
            ESMART2_R0_DSP_OFFSET => {
                ((MODE.v_active - image.surface.height) / 2) << 16
                    | ((MODE.h_active - image.surface.width) / 2)
            }
            _ => word.value,
        };
    }
    Some(program)
}
fn region_word(offset: u32) -> bool {
    use crate::rk3588_mode::{ESMART2_R0_ACT_INFO, ESMART2_R0_DSP_INFO, ESMART2_R0_DSP_OFFSET};
    matches!(
        offset,
        ESMART2_R0_MST_YRGB
            | ESMART2_R0_VIR
            | ESMART2_R0_ACT_INFO
            | ESMART2_R0_DSP_INFO
            | ESMART2_R0_DSP_OFFSET
    )
}
/// Stage only the five region geometry/address words and one consumed VP2 load.
pub fn stage_region(hw: &mut impl Hardware, program: &Program) {
    for w in program.words.iter().filter(|w| region_word(w.offset)) {
        hw.write(w.block, w.offset, w.value);
        hw.barrier();
    }
    hw.write(Block::Vop, REG_CFG_DONE, COMMIT_VP2);
    hw.barrier();
}
/// Retirement needs the complete replacement geometry/address, consumed load and a
/// freshly healthy DeviceCtx. A register match or frame-start alone is insufficient. — KESTREL
pub fn region_loaded(hw: &mut impl Hardware, program: &Program, dma_ready: bool) -> bool {
    dma_ready
        && hw.read(Block::Vop, REG_CFG_DONE) & VP2_LOAD_PENDING == 0
        && program
            .words
            .iter()
            .filter(|w| region_word(w.offset))
            .all(|w| hw.read(w.block, w.offset) == w.value)
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use crate::rk3588_mode::{Framebuffer, DMT_1600X900_60_RB};
    extern crate std;
    use std::collections::BTreeMap;
    struct Io(BTreeMap<u32, u32>);
    impl Hardware for Io {
        fn read(&mut self, _: Block, p: u32) -> u32 {
            *self.0.get(&p).unwrap_or(&0)
        }
        fn write(&mut self, b: Block, p: u32, v: u32) {
            assert_eq!(b, Block::Vop);
            assert!(region_word(p) || p == REG_CFG_DONE);
            self.0.insert(p, v);
        }
        fn barrier(&mut self) {}
        fn sleep_ms(&mut self, _: u64) {
            panic!("no dwell in register policy")
        }
    }
    #[test]
    fn shared_image_changes_only_region_and_retires_only_after_consumed_healthy_console() {
        let target = Program::build(
            DMT_1600X900_60_RB,
            Framebuffer {
                phys: 0xed9e0000,
                width: 1600,
                height: 900,
                stride: 1600,
            },
        )
        .unwrap();
        let image = kumo_graphics::native::Image {
            sequence: 1,
            surface: kumo_graphics::DEMO,
            offset: 0x10000,
            vmo_bytes: 0x30000,
            gpu_point: 3,
            checksum: 0,
        };
        let native = image_program(&target, image).unwrap();
        assert_eq!(native.checks, target.checks);
        for (a, b) in native.words.iter().zip(target.words) {
            if !region_word(a.offset) {
                assert_eq!(*a, b);
            }
        }
        let mut io = Io(BTreeMap::new());
        stage_region(&mut io, &native);
        assert!(!region_loaded(&mut io, &native, true));
        io.0.insert(REG_CFG_DONE, 0x8000);
        assert!(region_loaded(&mut io, &native, true));
        assert!(!region_loaded(&mut io, &target, true));
        stage_region(&mut io, &target);
        assert!(!region_loaded(&mut io, &target, true));
        io.0.insert(REG_CFG_DONE, 0x8000);
        assert!(!region_loaded(&mut io, &target, false));
        assert!(region_loaded(&mut io, &target, true));
        io.0.insert(ESMART2_R0_VIR, 256);
        assert!(!region_loaded(&mut io, &target, true));
    }
}
