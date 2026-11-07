//j832
//! Default-off NET5a. Vendor IN only on an exactly inventoried/profiled adapter.
//! The existing controller owns EP0 and its custodied page; no allocation, endpoint
//! setup, implicit PHY page write or packet traffic. Recovery class: stateless.
//! Any uncertain transfer keeps its quarantined EP0 and DMA. — TIMBERDOODLE
use super::{ax88_record, completion_actors, control_data_exact, Mmio, CONTROLLER_BASE};
use core::fmt;
use core::sync::atomic::Ordering;
use kumo_ax88::{
    control::decode_mac,
    identity::{phy_status, READS},
    profile::{Profile, Speed},
};
use kumo_xhci::{ControlPipe, DmaBuf, DmaTrbs, NoOpCommandImage, RegisterLayout, TransferRing};

pub(super) const ENABLED: bool = match option_env!("KUMO_AX88_IDENTITY_PROOF") {
    Some(value) => matches!(value.as_bytes(), [b'1']),
    None => false,
};
struct Hex<'a>(&'a [u8]);
impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    evt: &DmaTrbs,
    ring: &mut TransferRing,
    segment: &mut DmaTrbs,
    slot: u8,
    speed: u8,
    device: &[u8],
    config: &[u8],
    buffer: DmaBuf,
) {
    let speed = match speed {
        1 => Speed::Full,
        3 => Speed::High,
        4 => Speed::Super,
        _ => return ax88_record(format_args!("NET5a KUMO REFUSED speed\n")),
    };
    if let Err(error) = Profile::from_descriptors(device, config, speed) {
        return ax88_record(format_args!("NET5a KUMO REFUSED profile={:?}\n", error));
    }
    let Ok(mut pipe) = ControlPipe::new(slot) else {
        return;
    };
    let before = completion_actors().stats();
    let mut values = [[0u8; 6]; 10];
    let mut reads = 0;
    ax88_record(format_args!(
        "NET5a KUMO start controller={:#x} slot={} fields=10 writes=0\n",
        CONTROLLER_BASE.load(Ordering::Relaxed),
        slot
    ));
    for (i, field) in READS.into_iter().enumerate() {
        let request = field.request();
        match control_data_exact(
            io,
            layout,
            image,
            evt,
            ring,
            segment,
            slot,
            &mut pipe,
            request.setup(),
            buffer,
            &[],
        ) {
            Ok(actual) if request.validate_transferred(actual).is_ok() => {
                buffer.read_into(0, &mut values[i][..actual]);
                reads += 1;
                ax88_record(format_args!(
                    "NET5a KUMO field={} setup={} bytes={} actual={}\n",
                    field.name(),
                    Hex(&request.setup()),
                    Hex(&values[i][..actual]),
                    actual
                ));
            }
            other => {
                ax88_record(format_args!(
                    "NET5a KUMO unavailable field={} result={:?} reads={}; retain EP0, no retry\n",
                    field.name(),
                    other,
                    reads
                ));
                // MAC/status are required; optional PHY unavailability is explicit. No
                // further request may follow an uncertain control transfer. — TIMBERDOODLE
                return;
            }
        }
    }
    let mac = match decode_mac(&values[0]) {
        Ok(mac) => mac,
        Err(error) => {
            return ax88_record(format_args!(
                "NET5a KUMO REFUSED mac={} error={:?}\n",
                Hex(&values[0]),
                error
            ))
        }
    };
    let word = |i: usize| u16::from_le_bytes([values[i][0], values[i][1]]);
    let phy = if reads == 10 {
        phy_status(word(5), word(6), word(7), word(9))
    } else {
        kumo_ax88::identity::PhyStatus::Unavailable
    };
    let after = completion_actors().stats();
    ax88_record(format_args!("NET5a KUMO identity mac={} usb={:#04x} silicon={:#04x} power={:#06x} clock={:#04x} phy={:?}\n",
        Hex(&mac),values[1][0],values[2][0],word(3),values[4][0],phy));
    ax88_record(format_args!("NET5a KUMO PASS reads={} registered={} routed={} consumed={} retired={} faults={} queued={} writes=0\n",
        reads,after.registered-before.registered,after.routed-before.routed,
        after.consumed-before.consumed,after.retired-before.retired,
        after.faults-before.faults,ring.queued()));
}
