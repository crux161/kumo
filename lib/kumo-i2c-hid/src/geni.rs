//j377
//j386
pub mod register {
    pub const FORCE_DEFAULT: u32 = 0x20;
    pub const OUTPUT_CTRL: u32 = 0x24;
    pub const CGC_CTRL: u32 = 0x28;
    pub const GENI_STATUS: u32 = 0x40;
    pub const FW_REVISION: u32 = 0x68;
    pub const CLK_SEL: u32 = 0x7c;
    pub const DMA_MODE_EN: u32 = 0x258;
    pub const BYTE_GRAN: u32 = 0x254;
    pub const TX_PACKING0: u32 = 0x260;
    pub const TX_PACKING1: u32 = 0x264;
    pub const TX_LENGTH: u32 = 0x26c;
    pub const RX_LENGTH: u32 = 0x270;
    pub const SCL_COUNTERS: u32 = 0x278;
    pub const RX_PACKING0: u32 = 0x284;
    pub const RX_PACKING1: u32 = 0x288;
    pub const M_CLK_CFG: u32 = 0x48;
    pub const FIFO_DISABLE: u32 = 0x64;
    pub const M_CMD0: u32 = 0x600;
    pub const M_CMD_CTRL: u32 = 0x604;
    pub const M_IRQ_STATUS: u32 = 0x610;
    pub const M_IRQ_EN: u32 = 0x614;
    pub const M_IRQ_CLEAR: u32 = 0x618;
    pub const S_IRQ_CLEAR: u32 = 0x648;
    pub const TX_FIFO: u32 = 0x700;
    pub const RX_FIFO: u32 = 0x780;
    pub const RX_FIFO_STATUS: u32 = 0x804;
    pub const TX_WATERMARK: u32 = 0x80c;
    pub const RX_WATERMARK: u32 = 0x810;
    pub const RX_RFR_WATERMARK: u32 = 0x814;
    pub const HW_PARAM_TX: u32 = 0xe24;
    pub const IRQ_EN: u32 = 0xe1c;
    pub const GSI_EVENT_EN: u32 = 0xe18;
    pub const DMA_GENERAL_CFG: u32 = 0xe30;
}

const I2C_PROTOCOL: u32 = 3;
const IRQ_DONE: u32 = 1 << 0;
const IRQ_NACK: u32 = 1 << 10;
const IRQ_ERROR: u32 = (1 << 1) | (1 << 2) | (1 << 3) | (1 << 12) | (1 << 13);
const IRQ_FAULT: u32 = IRQ_NACK | IRQ_ERROR;
const IRQ_RX: u32 = (1 << 26) | (1 << 27);
const IRQ_TX: u32 = 1 << 30;
const IRQ_ABORT_DONE: u32 = 1 << 5;
const COMMON_IRQS: u32 = 0x7e | (3 << 22) | (3 << 24) | (3 << 28);

pub trait RegisterIo {
    fn read(&mut self, offset: u32) -> u32;
    fn write(&mut self, offset: u32, value: u32);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceClock {
    Mhz19_2,
    Mhz32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GeniError {
    WrongProtocol,
    FifoUnavailable,
    InvalidFifoDepth,
    InvalidAddress,
    EmptyTransfer,
    TransferTooLong,
    Nack { m_irq: u32, geni_status: u32 },
    Bus { m_irq: u32, geni_status: u32 },
    Timeout { m_irq: u32, geni_status: u32 },
    IncompleteRead,
}

impl GeniError {
    /// Pack this error into one diagnostic word for `log_hex`: the top nibble is the fault kind and,
    /// for a hardware fault, the GENI_STATUS snapshot rides in the mid word and the M_IRQ_STATUS bits
    /// in the low word — so a single hex line shows exactly why a transfer failed. Simple validation
    /// errors keep their small ordinal. — CORVUS
    pub fn code(&self) -> u64 {
        match *self {
            GeniError::WrongProtocol => 0,
            GeniError::FifoUnavailable => 1,
            GeniError::InvalidFifoDepth => 2,
            GeniError::InvalidAddress => 3,
            GeniError::EmptyTransfer => 4,
            GeniError::TransferTooLong => 5,
            GeniError::Nack { m_irq, geni_status } => {
                0x1000_0000_0000_0000 | ((geni_status as u64) << 32) | (m_irq as u64)
            }
            GeniError::Bus { m_irq, geni_status } => {
                0x2000_0000_0000_0000 | ((geni_status as u64) << 32) | (m_irq as u64)
            }
            GeniError::Timeout { m_irq, geni_status } => {
                0x3000_0000_0000_0000 | ((geni_status as u64) << 32) | (m_irq as u64)
            }
            GeniError::IncompleteRead => 9,
        }
    }
}

pub struct Controller<Io> {
    io: Io,
    tx_words: usize,
    poll_limit: usize,
}

impl<Io: RegisterIo> Controller<Io> {
    pub fn new(
        mut io: Io,
        source_clock: SourceClock,
        poll_limit: usize,
    ) -> Result<Self, GeniError> {
        if (io.read(register::FW_REVISION) >> 8) & 0xff != I2C_PROTOCOL {
            return Err(GeniError::WrongProtocol);
        }
        if io.read(register::FIFO_DISABLE) & 1 != 0 {
            return Err(GeniError::FifoUnavailable);
        }
        let tx_words = ((io.read(register::HW_PARAM_TX) >> 16) & 0xff) as usize;
        if tx_words < 2 {
            return Err(GeniError::InvalidFifoDepth);
        }

        for offset in [
            register::M_IRQ_CLEAR,
            register::S_IRQ_CLEAR,
            register::IRQ_EN,
        ] {
            io.write(offset, u32::MAX);
        }
        io.write(register::GSI_EVENT_EN, 0);
        update(&mut io, register::CGC_CTRL, |value| value | 0x7f);
        update(&mut io, register::DMA_GENERAL_CFG, |value| value | 0x0f);
        io.write(register::OUTPUT_CTRL, 0x7f);
        io.write(register::FORCE_DEFAULT, 1);
        update(&mut io, register::IRQ_EN, |value| value | 0x0f);
        io.write(register::DMA_MODE_EN, 0);
        io.write(register::RX_WATERMARK, (tx_words - 1) as u32);
        io.write(register::RX_RFR_WATERMARK, tx_words as u32);
        update(&mut io, register::M_IRQ_EN, |value| {
            value | COMMON_IRQS | IRQ_DONE | IRQ_RX | IRQ_TX
        });
        io.write(register::TX_PACKING0, 0x0007_f8fe);
        io.write(register::TX_PACKING1, 0x000f_fefe);
        io.write(register::RX_PACKING0, 0x0007_f8fe);
        io.write(register::RX_PACKING1, 0x000f_fefe);
        io.write(register::BYTE_GRAN, 0);
        io.write(register::CLK_SEL, 0);
        let (divider, high, low, cycle) = match source_clock {
            SourceClock::Mhz19_2 => (2, 5, 11, 22),
            SourceClock::Mhz32 => (4, 3, 9, 19),
        };
        io.write(register::M_CLK_CFG, divider << 4 | 1);
        io.write(register::SCL_COUNTERS, high << 20 | low << 10 | cycle);

        Ok(Self {
            io,
            tx_words,
            poll_limit,
        })
    }

    pub fn write_read(
        &mut self,
        address: u8,
        written: &[u8],
        read: &mut [u8],
    ) -> Result<(), GeniError> {
        if address > 0x7f {
            return Err(GeniError::InvalidAddress);
        }
        if written.is_empty() || read.is_empty() {
            return Err(GeniError::EmptyTransfer);
        }
        self.write_message(address, written, true)?;
        self.read_message(address, read)
    }

    /// Issue a write-only transfer: an HID-over-I2C command (SET_POWER, RESET) that takes no
    /// response payload, so [`write_read`](Self::write_read)'s non-empty-read guard does not fit.
    /// — CORVUS
    pub fn write(&mut self, address: u8, written: &[u8]) -> Result<(), GeniError> {
        if address > 0x7f {
            return Err(GeniError::InvalidAddress);
        }
        if written.is_empty() {
            return Err(GeniError::EmptyTransfer);
        }
        self.write_message(address, written, true)
    }

    /// Fetch an input report with a plain I2C read — no register address written first. This is how
    /// HID-over-I2C delivers input reports (Linux `i2c_hid_get_input` → `i2c_master_recv`): the
    /// device presents the pending report at its current pointer, and addressing the input register
    /// first (as `write_read` does) returns the "no data" response instead. — CORVUS
    pub fn read(&mut self, address: u8, read: &mut [u8]) -> Result<(), GeniError> {
        if address > 0x7f {
            return Err(GeniError::InvalidAddress);
        }
        if read.is_empty() {
            return Err(GeniError::EmptyTransfer);
        }
        self.read_message(address, read)
    }

    pub fn into_inner(self) -> Io {
        self.io
    }

    fn write_message(
        &mut self,
        address: u8,
        bytes: &[u8],
        stop_stretch: bool,
    ) -> Result<(), GeniError> {
        if bytes.len() > u32::MAX as usize {
            return Err(GeniError::TransferTooLong);
        }
        self.io.write(register::TX_LENGTH, bytes.len() as u32);
        self.io.write(register::TX_WATERMARK, 1);
        self.io.write(
            register::M_CMD0,
            (1 << 27) | ((address as u32) << 9) | ((stop_stretch as u32) << 2),
        );
        let mut cursor = 0usize;
        for _ in 0..self.poll_limit {
            let irq = self.io.read(register::M_IRQ_STATUS);
            if irq & IRQ_FAULT != 0 {
                let geni_status = self.io.read(register::GENI_STATUS);
                self.io.write(register::TX_WATERMARK, 0);
                self.io.write(register::M_IRQ_CLEAR, irq);
                self.quiesce_after_fault(irq);
                return Err(fault_error(irq, geni_status));
            }
            if irq & IRQ_TX != 0 {
                for _ in 0..self.tx_words - 1 {
                    if cursor == bytes.len() {
                        self.io.write(register::TX_WATERMARK, 0);
                        break;
                    }
                    let mut word = 0u32;
                    for shift in [0, 8, 16, 24] {
                        if let Some(byte) = bytes.get(cursor) {
                            word |= (*byte as u32) << shift;
                            cursor += 1;
                        }
                    }
                    self.io.write(register::TX_FIFO, word);
                }
            }
            if irq != 0 {
                self.io.write(register::M_IRQ_CLEAR, irq);
            }
            if irq & IRQ_DONE != 0 && cursor == bytes.len() {
                return Ok(());
            }
        }
        let m_irq = self.io.read(register::M_IRQ_STATUS);
        let geni_status = self.io.read(register::GENI_STATUS);
        self.abort();
        Err(GeniError::Timeout { m_irq, geni_status })
    }

    fn read_message(&mut self, address: u8, bytes: &mut [u8]) -> Result<(), GeniError> {
        if bytes.len() > u32::MAX as usize {
            return Err(GeniError::TransferTooLong);
        }
        self.io.write(register::RX_LENGTH, bytes.len() as u32);
        self.io
            .write(register::M_CMD0, (2 << 27) | ((address as u32) << 9));
        let mut cursor = 0usize;
        for _ in 0..self.poll_limit {
            let irq = self.io.read(register::M_IRQ_STATUS);
            if irq & IRQ_FAULT != 0 {
                let geni_status = self.io.read(register::GENI_STATUS);
                self.io.write(register::M_IRQ_CLEAR, irq);
                self.quiesce_after_fault(irq);
                return Err(fault_error(irq, geni_status));
            }
            if irq & IRQ_RX != 0 {
                let words = (self.io.read(register::RX_FIFO_STATUS) & 0x00ff_ffff) as usize;
                for _ in 0..words {
                    let word = self.io.read(register::RX_FIFO);
                    for byte in word.to_le_bytes() {
                        if let Some(destination) = bytes.get_mut(cursor) {
                            *destination = byte;
                            cursor += 1;
                        }
                    }
                }
            }
            if irq != 0 {
                self.io.write(register::M_IRQ_CLEAR, irq);
            }
            if irq & IRQ_DONE != 0 {
                return if cursor == bytes.len() {
                    Ok(())
                } else {
                    Err(GeniError::IncompleteRead)
                };
            }
        }
        let m_irq = self.io.read(register::M_IRQ_STATUS);
        let geni_status = self.io.read(register::GENI_STATUS);
        self.abort();
        Err(GeniError::Timeout { m_irq, geni_status })
    }

    /// A faulted command (NACK, bus error) may still be terminating in the sequencer when the
    /// fault bit is observed; issuing the next M_CMD while GENI_STATUS reads CMD_ACTIVE raises
    /// M_CMD_OVERRUN (M_IRQ bit 1) and wedges the bus — seen live on the X13s as
    /// `input frame read error=0x2000004100000002` after a NACKed touchpad probe (J386). Wait for
    /// M_CMD_DONE, clearing status as it lands, and fall back to a hard abort if it never does —
    /// Linux i2c-qcom-geni's cancel/abort-on-error discipline. — CORVUS
    fn quiesce_after_fault(&mut self, first_irq: u32) {
        if first_irq & IRQ_DONE != 0 {
            return;
        }
        for _ in 0..self.poll_limit {
            let irq = self.io.read(register::M_IRQ_STATUS);
            if irq != 0 {
                self.io.write(register::M_IRQ_CLEAR, irq);
            }
            if irq & IRQ_DONE != 0 {
                return;
            }
        }
        self.abort();
    }

    fn abort(&mut self) {
        self.io.write(register::TX_WATERMARK, 0);
        self.io.write(register::M_CMD_CTRL, 1 << 1);
        for _ in 0..self.poll_limit {
            let irq = self.io.read(register::M_IRQ_STATUS);
            if irq != 0 {
                self.io.write(register::M_IRQ_CLEAR, irq);
            }
            if irq & IRQ_ABORT_DONE != 0 {
                break;
            }
        }
    }
}

fn update(io: &mut impl RegisterIo, offset: u32, operation: impl FnOnce(u32) -> u32) {
    let value = io.read(offset);
    io.write(offset, operation(value));
}

/// Classify a completion IRQ whose fault bits are set into a diagnostic error, capturing the
/// M_IRQ_STATUS bits (`m_irq`) and the GENI_STATUS sequencer state (`geni_status`) at the point of
/// failure so a NACK or bus fault is diagnosable in a log instead of opaque — Linux i2c-qcom-geni
/// surfaces the same pair. Callers gate on `irq & IRQ_FAULT` so GENI_STATUS is read only on fault.
/// — CORVUS
fn fault_error(irq: u32, geni_status: u32) -> GeniError {
    if irq & IRQ_NACK != 0 {
        GeniError::Nack {
            m_irq: irq,
            geni_status,
        }
    } else {
        GeniError::Bus {
            m_irq: irq,
            geni_status,
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use alloc::{collections::BTreeMap, collections::VecDeque, vec::Vec};

    use super::*;

    #[derive(Default)]
    struct FakeRegisters {
        values: BTreeMap<u32, u32>,
        irqs: VecDeque<u32>,
        rx: VecDeque<u32>,
        tx: Vec<u8>,
    }

    impl FakeRegisters {
        fn ready() -> Self {
            let mut fake = Self::default();
            fake.values.insert(register::FW_REVISION, I2C_PROTOCOL << 8);
            fake.values.insert(register::HW_PARAM_TX, 16 << 16);
            fake
        }
    }

    impl RegisterIo for FakeRegisters {
        fn read(&mut self, offset: u32) -> u32 {
            match offset {
                register::M_IRQ_STATUS => self.irqs.pop_front().unwrap_or(0),
                register::RX_FIFO => self.rx.pop_front().unwrap_or(0),
                _ => self.values.get(&offset).copied().unwrap_or(0),
            }
        }

        fn write(&mut self, offset: u32, value: u32) {
            if offset == register::TX_FIFO {
                self.tx.extend_from_slice(&value.to_le_bytes());
            } else {
                self.values.insert(offset, value);
            }
        }
    }

    #[test]
    fn initializes_fifo_mode_and_400khz_timing() {
        let controller = Controller::new(FakeRegisters::ready(), SourceClock::Mhz19_2, 8).unwrap();
        let fake = controller.into_inner();
        assert_eq!(fake.values[&register::DMA_MODE_EN], 0);
        assert_eq!(fake.values[&register::M_CLK_CFG], 2 << 4 | 1);
        assert_eq!(
            fake.values[&register::SCL_COUNTERS],
            5 << 20 | 11 << 10 | 22
        );
        assert_eq!(fake.values[&register::TX_PACKING0], 0x0007_f8fe);
    }

    #[test]
    fn combined_register_read_uses_fifo_and_repeated_start() {
        let mut fake = FakeRegisters::ready();
        fake.irqs.extend([IRQ_TX, IRQ_DONE, IRQ_RX, IRQ_DONE]);
        fake.values.insert(register::RX_FIFO_STATUS, 2);
        fake.rx.extend([0x0302_0100, 0x0706_0504]);
        let mut controller = Controller::new(fake, SourceClock::Mhz19_2, 8).unwrap();
        let mut output = [0u8; 8];
        controller.write_read(0x68, &[1, 0], &mut output).unwrap();
        let fake = controller.into_inner();
        assert_eq!(&fake.tx[..2], &[1, 0]);
        assert_eq!(output, [0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn plain_read_fetches_an_input_report_without_a_preceding_write() {
        // HID-over-I2C input reports are a plain master read — no register address on the wire.
        let mut fake = FakeRegisters::ready();
        fake.irqs.extend([IRQ_RX, IRQ_DONE]);
        fake.values.insert(register::RX_FIFO_STATUS, 2);
        fake.rx.extend([0x0302_0100, 0x0706_0504]);
        let mut controller = Controller::new(fake, SourceClock::Mhz19_2, 8).unwrap();
        let mut output = [0u8; 8];
        controller.read(0x68, &mut output).unwrap();
        let fake = controller.into_inner();
        assert!(
            fake.tx.is_empty(),
            "plain read must not write a register address"
        );
        assert_eq!(output, [0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn reports_nack_and_timeout_without_spinning_forever() {
        let mut nack = FakeRegisters::ready();
        nack.irqs.push_back(IRQ_NACK);
        let mut controller = Controller::new(nack, SourceClock::Mhz19_2, 2).unwrap();
        assert_eq!(
            controller.write_read(0x68, &[1], &mut [0u8; 2]),
            Err(GeniError::Nack {
                m_irq: IRQ_NACK,
                geni_status: 0
            })
        );
        let nack = controller.into_inner();
        assert_eq!(nack.values[&register::TX_WATERMARK], 0);
        assert_eq!(nack.values[&register::M_IRQ_CLEAR], IRQ_NACK);

        let mut controller =
            Controller::new(FakeRegisters::ready(), SourceClock::Mhz19_2, 2).unwrap();
        assert_eq!(
            controller.write_read(0x68, &[1], &mut [0u8; 2]),
            Err(GeniError::Timeout {
                m_irq: 0,
                geni_status: 0
            })
        );
        let fake = controller.into_inner();
        assert_eq!(fake.values[&register::M_CMD_CTRL], 1 << 1);
    }

    #[test]
    fn reports_abort_done_cleanly() {
        let mut timeout = FakeRegisters::ready();
        // Delay the abort done IRQ so it must be polled
        timeout.irqs.push_back(0);
        timeout.irqs.push_back(IRQ_ABORT_DONE);
        let mut controller = Controller::new(timeout, SourceClock::Mhz19_2, 5).unwrap();

        assert!(matches!(
            controller.write_read(0x68, &[1], &mut [0u8; 2]),
            Err(GeniError::Timeout { .. })
        ));
        let fake = controller.into_inner();
        assert_eq!(fake.values[&register::M_CMD_CTRL], 1 << 1);
        // Ensure the abort was polled and cleared
        assert_eq!(fake.values[&register::M_IRQ_CLEAR], IRQ_ABORT_DONE);
    }

    #[test]
    fn fault_errors_carry_the_irq_and_geni_status_snapshot() {
        // A NACK captures the M_IRQ_STATUS bits and the GENI_STATUS sequencer state.
        let mut nack = FakeRegisters::ready();
        nack.values.insert(register::GENI_STATUS, 0x00b1_0000);
        nack.irqs.push_back(IRQ_NACK);
        let mut controller = Controller::new(nack, SourceClock::Mhz19_2, 2).unwrap();
        assert_eq!(
            controller.write_read(0x68, &[1], &mut [0u8; 2]),
            Err(GeniError::Nack {
                m_irq: IRQ_NACK,
                geni_status: 0x00b1_0000
            })
        );

        // A non-NACK error bit classifies as Bus, carrying the same snapshot.
        let mut bus = FakeRegisters::ready();
        bus.values.insert(register::GENI_STATUS, 0x0000_00c2);
        bus.irqs.push_back(1 << 2);
        let mut controller = Controller::new(bus, SourceClock::Mhz19_2, 2).unwrap();
        assert_eq!(
            controller.write_read(0x68, &[1], &mut [0u8; 2]),
            Err(GeniError::Bus {
                m_irq: 1 << 2,
                geni_status: 0x0000_00c2
            })
        );
    }

    #[test]
    fn fault_waits_for_done_so_the_next_command_is_not_overrun() {
        // The NACKed command terminates (DONE lands two polls later); quiesce must consume it
        // without needing a hard abort, leaving the sequencer idle for the next M_CMD.
        let mut nack = FakeRegisters::ready();
        nack.irqs.extend([IRQ_NACK, 0, IRQ_DONE]);
        let mut controller = Controller::new(nack, SourceClock::Mhz19_2, 8).unwrap();
        assert!(matches!(
            controller.write_read(0x15, &[1, 0], &mut [0u8; 2]),
            Err(GeniError::Nack { .. })
        ));
        let fake = controller.into_inner();
        assert_eq!(fake.values[&register::M_IRQ_CLEAR], IRQ_DONE);
        assert_eq!(fake.values.get(&register::M_CMD_CTRL), None);
    }

    #[test]
    fn fault_aborts_when_done_never_lands() {
        // No DONE after the NACK; quiesce must fall back to the hard abort and drain it.
        let mut nack = FakeRegisters::ready();
        nack.irqs.extend([IRQ_NACK, 0, 0, 0, IRQ_ABORT_DONE]);
        let mut controller = Controller::new(nack, SourceClock::Mhz19_2, 3).unwrap();
        assert!(matches!(
            controller.write_read(0x15, &[1, 0], &mut [0u8; 2]),
            Err(GeniError::Nack { .. })
        ));
        let fake = controller.into_inner();
        assert_eq!(fake.values[&register::M_CMD_CTRL], 1 << 1);
        assert_eq!(fake.values[&register::M_IRQ_CLEAR], IRQ_ABORT_DONE);
    }

    #[test]
    fn error_code_packs_kind_and_register_snapshot() {
        assert_eq!(GeniError::WrongProtocol.code(), 0);
        assert_eq!(GeniError::IncompleteRead.code(), 9);
        assert_eq!(
            GeniError::Nack {
                m_irq: IRQ_NACK,
                geni_status: 0x00b1_0000
            }
            .code(),
            0x1000_0000_0000_0000 | (0x00b1_0000u64 << 32) | (IRQ_NACK as u64)
        );
    }
}
