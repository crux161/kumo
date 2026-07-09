//j432

use crate::{
    CommandRing, Error, EventRing, EventRingSegmentTableEntry, NoOpRegisterConfig, RingToken, Trb,
};

const DCBAA_ALIGNMENT: u64 = 64;
const DCBAA_ENTRY_BYTES: u64 = 8;
const DCBAA_MAX_ENTRIES: usize = 256;
const ERST_ALIGNMENT: u64 = 64;
const ERST_ENTRY_BYTES: u64 = 16;

/// Device-visible addresses for the first No-Op command DMA image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NoOpCommandIovas {
    pub dcbaa: u64,
    pub command_ring: u64,
    pub event_ring_segment_table: u64,
    pub event_ring: u64,
}

/// Software state and register values prepared for one xHCI No-Op command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NoOpCommandImage {
    pub registers: NoOpRegisterConfig,
    pub command_ring: CommandRing,
    pub event_ring: EventRing,
    pub command_token: RingToken,
}

/// Initialize the minimum DMA image needed before ringing one xHCI No-Op command.
///
/// The buffers must already be mapped into the driver's `DeviceCtx`; this helper only shapes the
/// hardware-visible contents and returns the register values that name those IOVAs.
pub fn prepare_noop_command_image(
    max_slots_enabled: u8,
    dcbaa_entries: &mut [u64],
    command_segment: &mut [Trb],
    event_segment: &mut [Trb],
    erst_entry: &mut EventRingSegmentTableEntry,
    iovas: NoOpCommandIovas,
) -> Result<NoOpCommandImage, Error> {
    validate_dcbaa(max_slots_enabled, dcbaa_entries.len(), iovas.dcbaa)?;
    validate_single_erst(iovas.event_ring_segment_table)?;

    dcbaa_entries.fill(0);
    let mut command_ring = CommandRing::new(command_segment, iovas.command_ring)?;
    let command_token = command_ring.enqueue(command_segment, Trb::no_op_command())?;
    let event_ring = EventRing::new(event_segment, iovas.event_ring)?;
    *erst_entry = EventRingSegmentTableEntry::new(iovas.event_ring, event_segment.len())?;

    Ok(NoOpCommandImage {
        registers: NoOpRegisterConfig::single_segment(
            max_slots_enabled,
            iovas.dcbaa,
            iovas.command_ring,
            iovas.event_ring_segment_table,
            event_ring.dequeue_iova(),
        ),
        command_ring,
        event_ring,
        command_token,
    })
}

fn validate_dcbaa(max_slots_enabled: u8, entries: usize, iova: u64) -> Result<(), Error> {
    if max_slots_enabled == 0
        || entries <= max_slots_enabled as usize
        || entries > DCBAA_MAX_ENTRIES
    {
        return Err(Error::InvalidField);
    }
    if iova & (DCBAA_ALIGNMENT - 1) != 0 {
        return Err(Error::MisalignedIova);
    }
    iova.checked_add(entries as u64 * DCBAA_ENTRY_BYTES)
        .ok_or(Error::InvalidField)?;
    Ok(())
}

fn validate_single_erst(iova: u64) -> Result<(), Error> {
    if iova & (ERST_ALIGNMENT - 1) != 0 {
        return Err(Error::MisalignedIova);
    }
    iova.checked_add(ERST_ENTRY_BYTES)
        .ok_or(Error::InvalidField)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{RingSegment, TrbType};

    use super::*;

    const IOVAS: NoOpCommandIovas = NoOpCommandIovas {
        dcbaa: 0x1_0000,
        command_ring: 0x2_0000,
        event_ring_segment_table: 0x3_0000,
        event_ring: 0x4_0000,
    };

    #[test]
    fn noop_image_prepares_dma_memory_and_register_values() {
        let mut dcbaa = [0xfeed_face_cafe_beefu64; 9];
        let mut command_segment = RingSegment::<16>::new();
        let mut event_segment = RingSegment::<16>::new();
        let mut erst_entry = EventRingSegmentTableEntry::default();

        let image = prepare_noop_command_image(
            8,
            &mut dcbaa,
            command_segment.entries_mut(),
            event_segment.entries_mut(),
            &mut erst_entry,
            IOVAS,
        )
        .unwrap();

        assert!(dcbaa.iter().all(|entry| *entry == 0));
        assert_eq!(image.command_token.iova, IOVAS.command_ring);
        assert_eq!(image.command_ring.queued(), 1);
        assert_eq!(image.event_ring.dequeue_iova(), IOVAS.event_ring);
        assert_eq!(
            image.registers,
            NoOpRegisterConfig::single_segment(
                8,
                IOVAS.dcbaa,
                IOVAS.command_ring,
                IOVAS.event_ring_segment_table,
                IOVAS.event_ring
            )
        );

        let command = command_segment.entries()[0];
        assert_eq!(command.trb_type(), Some(TrbType::NoOpCommand));
        assert!(command.cycle());
        let link = command_segment.entries()[15];
        assert_eq!(link.trb_type(), Some(TrbType::Link));
        assert_eq!(link.pointer(), IOVAS.command_ring);
        assert!(!link.cycle());
        assert!(event_segment
            .entries()
            .iter()
            .all(|trb| *trb == Trb::zero()));
        assert_eq!(
            erst_entry.words(),
            [
                IOVAS.event_ring as u32,
                (IOVAS.event_ring >> 32) as u32,
                16,
                0
            ]
        );
    }

    #[test]
    fn noop_image_rejects_invalid_dcbaa_and_erst_inputs() {
        let mut dcbaa = [0u64; 8];
        let mut command_segment = RingSegment::<16>::new();
        let mut event_segment = RingSegment::<16>::new();
        let mut erst_entry = EventRingSegmentTableEntry::default();
        assert_eq!(
            prepare_noop_command_image(
                8,
                &mut dcbaa,
                command_segment.entries_mut(),
                event_segment.entries_mut(),
                &mut erst_entry,
                IOVAS
            ),
            Err(Error::InvalidField)
        );

        let mut dcbaa = [0u64; 9];
        assert_eq!(
            prepare_noop_command_image(
                8,
                &mut dcbaa,
                command_segment.entries_mut(),
                event_segment.entries_mut(),
                &mut erst_entry,
                NoOpCommandIovas {
                    dcbaa: IOVAS.dcbaa + 8,
                    ..IOVAS
                }
            ),
            Err(Error::MisalignedIova)
        );
        assert_eq!(
            prepare_noop_command_image(
                8,
                &mut dcbaa,
                command_segment.entries_mut(),
                event_segment.entries_mut(),
                &mut erst_entry,
                NoOpCommandIovas {
                    event_ring_segment_table: IOVAS.event_ring_segment_table + 16,
                    ..IOVAS
                }
            ),
            Err(Error::MisalignedIova)
        );
    }
}
