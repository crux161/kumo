//j429

use crate::Error;

const DBOFF_MASK: u32 = 0xffff_fffc;
const RTSOFF_MASK: u32 = 0xffff_ffe0;

const OP_USBCMD: usize = 0x00;
const OP_USBSTS: usize = 0x04;
const OP_CRCR: usize = 0x18;
const OP_DCBAAP: usize = 0x30;
const OP_CONFIG: usize = 0x38;

const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;
const CMD_EIE: u32 = 1 << 2;

const STS_HALT: u32 = 1 << 0;
const STS_CNR: u32 = 1 << 11;

const CMD_RING_CYCLE: u64 = 1 << 0;
const CMD_RING_PTR_MASK: u64 = !0x3f;
const DCBAAP_PTR_MASK: u64 = !0x3f;

const RUNTIME_IR0: usize = 0x20;
const IR_IMAN: usize = 0x00;
const IR_ERSTSZ: usize = 0x08;
const IR_ERSTBA: usize = 0x10;
const IR_ERDP: usize = 0x18;

const IMAN_IE: u32 = 1 << 1;

const ERST_SIZE_MASK: u32 = 0xffff;
const ERST_BASE_ADDRESS_MASK: u64 = !0x3f;
const ERST_EHB: u64 = 1 << 3;
const ERST_PTR_MASK: u64 = !0xf;

/// A 32-bit MMIO port into one xHCI register window.
///
/// The controller exposes 64-bit DMA pointer registers as adjacent little-endian dwords; this trait
/// stays dword-sized so the pure model matches the accesses Linux's xHCI driver uses.
pub trait RegisterIo {
    fn read32(&mut self, offset: usize) -> u32;
    fn write32(&mut self, offset: usize, value: u32);
}

/// Checked offsets for the operational, runtime, and doorbell register windows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisterLayout {
    operational: usize,
    runtime: usize,
    doorbell: usize,
    mmio_length: usize,
}

impl RegisterLayout {
    /// Build the controller register map from CAPLENGTH plus raw DBOFF/RTSOFF capability dwords.
    pub fn new(caplength: u8, dboff: u32, rtsoff: u32, mmio_length: u64) -> Result<Self, Error> {
        let layout = Self {
            operational: caplength as usize,
            runtime: (rtsoff & RTSOFF_MASK) as usize,
            doorbell: (dboff & DBOFF_MASK) as usize,
            mmio_length: usize::try_from(mmio_length).map_err(|_| Error::InvalidField)?,
        };
        layout.check32(layout.operational + OP_CONFIG)?;
        layout.check32(layout.runtime + RUNTIME_IR0 + IR_ERDP + 4)?;
        layout.check32(layout.doorbell)?;
        Ok(layout)
    }

    pub const fn operational_offset(self) -> usize {
        self.operational
    }

    pub const fn runtime_offset(self) -> usize {
        self.runtime
    }

    pub const fn doorbell_offset(self) -> usize {
        self.doorbell
    }

    pub const fn command_doorbell_offset(self) -> usize {
        self.doorbell
    }

    pub const fn command_ring_offset(self) -> usize {
        self.operational + OP_CRCR
    }

    pub const fn dcbaa_offset(self) -> usize {
        self.operational + OP_DCBAAP
    }

    pub const fn interrupter0_erst_size_offset(self) -> usize {
        self.runtime + RUNTIME_IR0 + IR_ERSTSZ
    }

    /// Request a host-controller reset. The caller must poll USBCMD.HCRST and USBSTS.CNR before
    /// programming rings; [`Self::ready_for_ring_programming`] models that second gate.
    pub fn request_reset<IO: RegisterIo>(self, io: &mut IO) -> Result<(), Error> {
        self.check32(self.operational + OP_USBCMD)?;
        let command = io.read32(self.operational + OP_USBCMD);
        io.write32(
            self.operational + OP_USBCMD,
            (command & !CMD_RUN) | CMD_RESET,
        );
        Ok(())
    }

    pub fn ready_for_ring_programming<IO: RegisterIo>(self, io: &mut IO) -> Result<(), Error> {
        self.check32(self.operational + OP_USBSTS)?;
        let status = io.read32(self.operational + OP_USBSTS);
        if status & STS_HALT == 0 {
            return Err(Error::ControllerNotHalted);
        }
        if status & STS_CNR != 0 {
            return Err(Error::ControllerNotReady);
        }
        Ok(())
    }

    /// Program the register state needed to ring one No-Op Command and observe completion on IR0.
    ///
    /// This writes only the reset-stable xHCI register spine: CONFIG.NumSlotsEn, CRCR, DCBAAP, IR0
    /// ERSTSZ/ERSTBA/ERDP, then optionally arms IR0/USBCMD interrupts. It does not allocate DMA or
    /// submit the TRB; callers must use DeviceCtx-mapped IOVAs and the [`crate::CommandRing`] model.
    pub fn program_noop_registers<IO: RegisterIo>(
        self,
        io: &mut IO,
        config: NoOpRegisterConfig,
    ) -> Result<(), Error> {
        config.validate()?;

        let config_offset = self.operational + OP_CONFIG;
        self.check32(config_offset)?;
        let slots = (io.read32(config_offset) & !0xff) | config.max_slots_enabled as u32;
        io.write32(config_offset, slots);

        write64(
            io,
            self.command_ring_offset(),
            (config.command_ring_iova & CMD_RING_PTR_MASK) | CMD_RING_CYCLE,
        );
        write64(io, self.dcbaa_offset(), config.dcbaa_iova & DCBAAP_PTR_MASK);

        let erst_size_offset = self.interrupter0_erst_size_offset();
        self.check32(erst_size_offset)?;
        let erst_size = (io.read32(erst_size_offset) & !ERST_SIZE_MASK)
            | config.event_ring_segment_count as u32;
        io.write32(erst_size_offset, erst_size);
        write64(
            io,
            self.runtime + RUNTIME_IR0 + IR_ERSTBA,
            config.event_ring_segment_table_iova & ERST_BASE_ADDRESS_MASK,
        );
        write64(
            io,
            self.runtime + RUNTIME_IR0 + IR_ERDP,
            (config.event_ring_dequeue_iova & ERST_PTR_MASK) | ERST_EHB,
        );

        if config.enable_interrupts {
            self.enable_primary_interrupter(io)?;
        }
        Ok(())
    }

    pub fn start<IO: RegisterIo>(self, io: &mut IO) -> Result<(), Error> {
        self.check32(self.operational + OP_USBCMD)?;
        let command = io.read32(self.operational + OP_USBCMD);
        io.write32(self.operational + OP_USBCMD, command | CMD_RUN);
        Ok(())
    }

    pub fn ring_command_doorbell<IO: RegisterIo>(self, io: &mut IO) -> Result<(), Error> {
        self.check32(self.command_doorbell_offset())?;
        io.write32(self.command_doorbell_offset(), 0);
        Ok(())
    }

    fn enable_primary_interrupter<IO: RegisterIo>(self, io: &mut IO) -> Result<(), Error> {
        self.check32(self.runtime + RUNTIME_IR0 + IR_IMAN)?;
        let iman = io.read32(self.runtime + RUNTIME_IR0 + IR_IMAN);
        io.write32(self.runtime + RUNTIME_IR0 + IR_IMAN, iman | IMAN_IE);
        let command = io.read32(self.operational + OP_USBCMD);
        io.write32(self.operational + OP_USBCMD, command | CMD_EIE);
        Ok(())
    }

    fn check32(self, offset: usize) -> Result<(), Error> {
        offset
            .checked_add(4)
            .filter(|end| *end <= self.mmio_length)
            .map(|_| ())
            .ok_or(Error::RegisterWindowTooSmall)
    }
}

/// Device-visible addresses for the first Slice-2 command/event-ring register program.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NoOpRegisterConfig {
    pub max_slots_enabled: u8,
    pub dcbaa_iova: u64,
    pub command_ring_iova: u64,
    pub event_ring_segment_table_iova: u64,
    pub event_ring_dequeue_iova: u64,
    pub event_ring_segment_count: u16,
    pub enable_interrupts: bool,
}

impl NoOpRegisterConfig {
    pub const fn single_segment(
        max_slots_enabled: u8,
        dcbaa_iova: u64,
        command_ring_iova: u64,
        event_ring_segment_table_iova: u64,
        event_ring_dequeue_iova: u64,
    ) -> Self {
        Self {
            max_slots_enabled,
            dcbaa_iova,
            command_ring_iova,
            event_ring_segment_table_iova,
            event_ring_dequeue_iova,
            event_ring_segment_count: 1,
            enable_interrupts: false,
        }
    }

    fn validate(self) -> Result<(), Error> {
        if self.max_slots_enabled == 0 || self.event_ring_segment_count == 0 {
            return Err(Error::InvalidField);
        }
        if self.dcbaa_iova & 0x3f != 0
            || self.command_ring_iova & 0x3f != 0
            || self.event_ring_segment_table_iova & 0x3f != 0
            || self.event_ring_dequeue_iova & 0xf != 0
        {
            return Err(Error::MisalignedIova);
        }
        Ok(())
    }
}

fn write64<IO: RegisterIo>(io: &mut IO, offset: usize, value: u64) {
    io.write32(offset, value as u32);
    io.write32(offset + 4, (value >> 32) as u32);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct ReplayBus {
        config: u32,
        erst_size: u32,
        usbcmd: u32,
        usbsts: u32,
        iman: u32,
        writes: [(usize, u32); 16],
        write_len: usize,
    }

    impl ReplayBus {
        fn write_at(&self, index: usize) -> (usize, u32) {
            self.writes[index]
        }
    }

    impl RegisterIo for ReplayBus {
        fn read32(&mut self, offset: usize) -> u32 {
            match offset {
                0x40 => self.usbcmd,
                0x44 => self.usbsts,
                0x78 => self.config,
                0x828 => self.erst_size,
                0x820 => self.iman,
                _ => 0,
            }
        }

        fn write32(&mut self, offset: usize, value: u32) {
            self.writes[self.write_len] = (offset, value);
            self.write_len += 1;
            match offset {
                0x40 => self.usbcmd = value,
                0x78 => self.config = value,
                0x828 => self.erst_size = value,
                0x820 => self.iman = value,
                _ => {}
            }
        }
    }

    #[test]
    fn layout_masks_capability_offsets_and_bounds_windows() {
        let layout = RegisterLayout::new(0x40, 0x1003, 0x81f, 0x2000).unwrap();
        assert_eq!(layout.operational_offset(), 0x40);
        assert_eq!(layout.runtime_offset(), 0x800);
        assert_eq!(layout.doorbell_offset(), 0x1000);
        assert_eq!(layout.command_ring_offset(), 0x58);
        assert_eq!(layout.dcbaa_offset(), 0x70);
        assert_eq!(layout.interrupter0_erst_size_offset(), 0x828);
        assert_eq!(
            RegisterLayout::new(0x40, 0x1000, 0x800, 0x83b),
            Err(Error::RegisterWindowTooSmall)
        );
    }

    #[test]
    fn reset_and_ready_checks_model_the_controller_gate() {
        let layout = RegisterLayout::new(0x40, 0x1000, 0x800, 0x2000).unwrap();
        let mut bus = ReplayBus {
            usbcmd: CMD_RUN | 0x100,
            usbsts: STS_HALT,
            ..ReplayBus::default()
        };
        layout.request_reset(&mut bus).unwrap();
        assert_eq!(bus.write_at(0), (0x40, 0x102));
        assert_eq!(layout.ready_for_ring_programming(&mut bus), Ok(()));
        bus.usbsts = 0;
        assert_eq!(
            layout.ready_for_ring_programming(&mut bus),
            Err(Error::ControllerNotHalted)
        );
        bus.usbsts = STS_HALT | STS_CNR;
        assert_eq!(
            layout.ready_for_ring_programming(&mut bus),
            Err(Error::ControllerNotReady)
        );
    }

    #[test]
    fn noop_register_program_writes_the_slice_two_spine() {
        let layout = RegisterLayout::new(0x40, 0x1000, 0x800, 0x2000).unwrap();
        let mut bus = ReplayBus {
            config: 0xabcd_1234,
            erst_size: 0x5555_aaaa,
            ..ReplayBus::default()
        };
        let config = NoOpRegisterConfig::single_segment(
            8,
            0x0000_0001_0000_0000,
            0x0000_0001_0000_4000,
            0x0000_0001_0000_8000,
            0x0000_0001_0000_c000,
        );

        layout.program_noop_registers(&mut bus, config).unwrap();

        assert_eq!(bus.write_len, 10);
        assert_eq!(bus.write_at(0), (0x78, 0xabcd_1208));
        assert_eq!(bus.write_at(1), (0x58, 0x0000_4001));
        assert_eq!(bus.write_at(2), (0x5c, 0x0000_0001));
        assert_eq!(bus.write_at(3), (0x70, 0x0000_0000));
        assert_eq!(bus.write_at(4), (0x74, 0x0000_0001));
        assert_eq!(bus.write_at(5), (0x828, 0x5555_0001));
        assert_eq!(bus.write_at(6), (0x830, 0x0000_8000));
        assert_eq!(bus.write_at(7), (0x834, 0x0000_0001));
        assert_eq!(bus.write_at(8), (0x838, 0x0000_c008));
        assert_eq!(bus.write_at(9), (0x83c, 0x0000_0001));
    }

    #[test]
    fn interrupt_start_and_doorbell_are_separate_explicit_steps() {
        let layout = RegisterLayout::new(0x40, 0x1000, 0x800, 0x2000).unwrap();
        let mut bus = ReplayBus {
            usbcmd: 0x100,
            iman: 1,
            ..ReplayBus::default()
        };
        let mut config = NoOpRegisterConfig::single_segment(1, 0x4000, 0x8000, 0xc000, 0x1_0000);
        config.enable_interrupts = true;

        layout.program_noop_registers(&mut bus, config).unwrap();
        assert_eq!(bus.write_at(10), (0x820, 0x3));
        assert_eq!(bus.write_at(11), (0x40, 0x104));

        layout.start(&mut bus).unwrap();
        assert_eq!(bus.write_at(12), (0x40, 0x105));
        layout.ring_command_doorbell(&mut bus).unwrap();
        assert_eq!(bus.write_at(13), (0x1000, 0));
    }

    #[test]
    fn noop_register_program_rejects_bad_dma_addresses() {
        let layout = RegisterLayout::new(0x40, 0x1000, 0x800, 0x2000).unwrap();
        let mut bus = ReplayBus::default();
        assert_eq!(
            layout.program_noop_registers(
                &mut bus,
                NoOpRegisterConfig::single_segment(0, 0x4000, 0x8000, 0xc000, 0x1_0000)
            ),
            Err(Error::InvalidField)
        );
        assert_eq!(
            layout.program_noop_registers(
                &mut bus,
                NoOpRegisterConfig::single_segment(1, 0x4040, 0x8008, 0xc000, 0x1_0000)
            ),
            Err(Error::MisalignedIova)
        );
    }
}
