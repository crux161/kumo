//j833
//j834
//j835
//! Default-off NET5b. The resident owner's exact EP0 pipe carries the pure
//! `kumo_ax88::link` sequence, paced by the loop's 100 ms proof tick, so the PHY and
//! clock settle waits never block HID or storage service. No new DMA, grant,
//! endpoint or frame channel. An uncertain transfer quarantines the pipe and ends
//! the sequence with everything retained. Recovery class: stateless. — TIMBERDOODLE
use super::{ax88_record, control_data_exact, Mmio};
use kumo_ax88::control::{Direction, Request};
use kumo_rt::clock_get;
use kumo_xhci::{
    ControlError, ControlPipe, DmaBuf, DmaTrbs, NoOpCommandImage, RegisterLayout, TransferRing,
};

pub(super) const ENABLED: bool = flag(option_env!("KUMO_AX88_LINK_PROOF"));
/// NET6: the same sequence with the receive goal; takes precedence over NET5b.
pub(super) const RX_ENABLED: bool = flag(option_env!("KUMO_AX88_RX_PROOF")) || TX_ENABLED;
/// NET7a: NET6's receive window plus the bulk OUT lane, so TX runs full duplex.
pub(super) const TX_ENABLED: bool = flag(option_env!("KUMO_AX88_TX_PROOF"));

const fn flag(value: Option<&str>) -> bool {
    match value {
        Some(value) => matches!(value.as_bytes(), [b'1']),
        None => false,
    }
}

pub(super) struct Ep0Port<'a> {
    pub io: &'a mut Mmio,
    pub layout: RegisterLayout,
    pub image: &'a mut NoOpCommandImage,
    pub evt: &'a DmaTrbs,
    pub ring: &'a mut TransferRing,
    pub segment: DmaTrbs,
    pub slot: u8,
    pub pipe: &'a mut ControlPipe,
    pub buffer: DmaBuf,
}

impl kumo_ax88::link::Port for Ep0Port<'_> {
    type Error = ControlError;

    fn exchange(&mut self, request: &Request, into: &mut [u8; 6]) -> Result<(), ControlError> {
        let actual = control_data_exact(
            self.io,
            self.layout,
            self.image,
            self.evt,
            self.ring,
            &mut self.segment,
            self.slot,
            self.pipe,
            request.setup(),
            self.buffer,
            request.outbound(),
        )?;
        if request.validate_transferred(actual).is_err() {
            return self.pipe.fail(ControlError::Length);
        }
        if request.direction() == Direction::In {
            self.buffer.read_into(0, &mut into[..actual]);
        }
        Ok(())
    }

    fn now_ns(&self) -> u64 {
        clock_get()
    }

    fn record(&mut self, args: core::fmt::Arguments<'_>) {
        ax88_record(args);
    }
}
