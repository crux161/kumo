use crate::Error;

const CYCLE: u32 = 1;
const INTERRUPT_ON_SHORT_PACKET: u32 = 1 << 2;
const CHAIN: u32 = 1 << 4;
const INTERRUPT_ON_COMPLETION: u32 = 1 << 5;
const IMMEDIATE_DATA: u32 = 1 << 6;
const TYPE_SHIFT: u32 = 10;

/// xHCI TRB type IDs (xHCI 1.2, table 6-91).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TrbType {
    Normal = 1,
    SetupStage = 2,
    DataStage = 3,
    StatusStage = 4,
    Isochronous = 5,
    Link = 6,
    EventData = 7,
    NoOpTransfer = 8,
    EnableSlotCommand = 9,
    DisableSlotCommand = 10,
    AddressDeviceCommand = 11,
    ConfigureEndpointCommand = 12,
    EvaluateContextCommand = 13,
    ResetEndpointCommand = 14,
    StopEndpointCommand = 15,
    SetTransferRingDequeueCommand = 16,
    ResetDeviceCommand = 17,
    ForceEventCommand = 18,
    NegotiateBandwidthCommand = 19,
    SetLatencyToleranceCommand = 20,
    GetPortBandwidthCommand = 21,
    ForceHeaderCommand = 22,
    NoOpCommand = 23,
    GetExtendedPropertyCommand = 24,
    SetExtendedPropertyCommand = 25,
    TransferEvent = 32,
    CommandCompletionEvent = 33,
    PortStatusChangeEvent = 34,
    BandwidthRequestEvent = 35,
    DoorbellEvent = 36,
    HostControllerEvent = 37,
    DeviceNotificationEvent = 38,
    MfindexWrapEvent = 39,
}

impl TrbType {
    pub const fn from_raw(raw: u8) -> Option<Self> {
        Some(match raw {
            1 => Self::Normal,
            2 => Self::SetupStage,
            3 => Self::DataStage,
            4 => Self::StatusStage,
            5 => Self::Isochronous,
            6 => Self::Link,
            7 => Self::EventData,
            8 => Self::NoOpTransfer,
            9 => Self::EnableSlotCommand,
            10 => Self::DisableSlotCommand,
            11 => Self::AddressDeviceCommand,
            12 => Self::ConfigureEndpointCommand,
            13 => Self::EvaluateContextCommand,
            14 => Self::ResetEndpointCommand,
            15 => Self::StopEndpointCommand,
            16 => Self::SetTransferRingDequeueCommand,
            17 => Self::ResetDeviceCommand,
            18 => Self::ForceEventCommand,
            19 => Self::NegotiateBandwidthCommand,
            20 => Self::SetLatencyToleranceCommand,
            21 => Self::GetPortBandwidthCommand,
            22 => Self::ForceHeaderCommand,
            23 => Self::NoOpCommand,
            24 => Self::GetExtendedPropertyCommand,
            25 => Self::SetExtendedPropertyCommand,
            32 => Self::TransferEvent,
            33 => Self::CommandCompletionEvent,
            34 => Self::PortStatusChangeEvent,
            35 => Self::BandwidthRequestEvent,
            36 => Self::DoorbellEvent,
            37 => Self::HostControllerEvent,
            38 => Self::DeviceNotificationEvent,
            39 => Self::MfindexWrapEvent,
            _ => return None,
        })
    }

    pub const fn is_command(self) -> bool {
        matches!(self as u8, 9..=25)
    }

    pub const fn is_transfer(self) -> bool {
        matches!(self as u8, 1..=8) && !matches!(self, Self::Link)
    }
}

/// One hardware-visible 16-byte Transfer Request Block.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Trb {
    words: [u32; 4],
}

impl Trb {
    pub const fn zero() -> Self {
        Self { words: [0; 4] }
    }

    pub const fn from_words(words: [u32; 4]) -> Self {
        Self { words }
    }

    pub const fn words(self) -> [u32; 4] {
        self.words
    }

    pub const fn cycle(self) -> bool {
        self.words[3] & CYCLE != 0
    }

    pub const fn trb_type_raw(self) -> u8 {
        ((self.words[3] >> TYPE_SHIFT) & 0x3f) as u8
    }

    pub const fn trb_type(self) -> Option<TrbType> {
        TrbType::from_raw(self.trb_type_raw())
    }

    pub const fn pointer(self) -> u64 {
        self.words[0] as u64 | ((self.words[1] as u64) << 32)
    }

    pub const fn completion_code(self) -> u8 {
        (self.words[2] >> 24) as u8
    }

    pub const fn slot_id(self) -> u8 {
        (self.words[3] >> 24) as u8
    }

    pub const fn endpoint_id(self) -> u8 {
        ((self.words[3] >> 16) & 0x1f) as u8
    }

    pub const fn with_cycle(mut self, cycle: bool) -> Self {
        self.words[3] = (self.words[3] & !CYCLE) | cycle as u32;
        self
    }

    pub const fn no_op_command() -> Self {
        typed(TrbType::NoOpCommand)
    }

    pub fn enable_slot(slot_type: u8) -> Result<Self, Error> {
        if slot_type > 31 {
            return Err(Error::InvalidField);
        }
        let mut trb = typed(TrbType::EnableSlotCommand);
        trb.words[3] |= (slot_type as u32) << 16;
        Ok(trb)
    }

    pub fn address_device(
        input_context_iova: u64,
        slot_id: u8,
        block_set_address: bool,
    ) -> Result<Self, Error> {
        command_with_context(
            TrbType::AddressDeviceCommand,
            input_context_iova,
            slot_id,
            block_set_address,
        )
    }

    pub fn configure_endpoint(input_context_iova: u64, slot_id: u8) -> Result<Self, Error> {
        command_with_context(
            TrbType::ConfigureEndpointCommand,
            input_context_iova,
            slot_id,
            false,
        )
    }

    pub fn normal_transfer(config: NormalTransfer) -> Result<Self, Error> {
        if config.length > 0x1ffff || config.td_size > 31 || config.interrupter_target > 0x3ff {
            return Err(Error::InvalidField);
        }
        validate_buffer(config.buffer_iova, config.length)?;

        let mut trb = typed(TrbType::Normal);
        set_pointer(&mut trb, config.buffer_iova);
        trb.words[2] = config.length
            | ((config.td_size as u32) << 17)
            | ((config.interrupter_target as u32) << 22);
        if config.interrupt_on_short_packet {
            trb.words[3] |= INTERRUPT_ON_SHORT_PACKET;
        }
        trb.words[3] |= (config.chain as u32) << 4;
        if config.interrupt_on_completion {
            trb.words[3] |= INTERRUPT_ON_COMPLETION;
        }
        Ok(trb)
    }

    pub fn setup_stage(
        packet: [u8; 8],
        transfer_type: SetupTransferType,
        interrupter_target: u16,
    ) -> Result<Self, Error> {
        if interrupter_target > 0x3ff {
            return Err(Error::InvalidField);
        }
        let mut trb = typed(TrbType::SetupStage);
        trb.words[0] = u32::from_le_bytes(packet[..4].try_into().expect("four bytes"));
        trb.words[1] = u32::from_le_bytes(packet[4..].try_into().expect("four bytes"));
        trb.words[2] = 8 | ((interrupter_target as u32) << 22);
        trb.words[3] |= IMMEDIATE_DATA | ((transfer_type as u32) << 16);
        Ok(trb)
    }

    pub fn data_stage(
        buffer_iova: u64,
        length: u32,
        td_size: u8,
        interrupter_target: u16,
        direction_in: bool,
    ) -> Result<Self, Error> {
        if length > 0x1ffff || td_size > 31 || interrupter_target > 0x3ff {
            return Err(Error::InvalidField);
        }
        validate_buffer(buffer_iova, length)?;
        let mut trb = typed(TrbType::DataStage);
        set_pointer(&mut trb, buffer_iova);
        trb.words[2] = length | ((td_size as u32) << 17) | ((interrupter_target as u32) << 22);
        trb.words[3] |= CHAIN | ((direction_in as u32) << 16);
        Ok(trb)
    }

    pub fn status_stage(
        interrupter_target: u16,
        direction_in: bool,
        interrupt_on_completion: bool,
    ) -> Result<Self, Error> {
        if interrupter_target > 0x3ff {
            return Err(Error::InvalidField);
        }
        let mut trb = typed(TrbType::StatusStage);
        trb.words[2] = (interrupter_target as u32) << 22;
        trb.words[3] |= (direction_in as u32) << 16;
        if interrupt_on_completion {
            trb.words[3] |= INTERRUPT_ON_COMPLETION;
        }
        Ok(trb)
    }

    pub fn decode_event(self) -> Option<Event> {
        match self.trb_type()? {
            TrbType::TransferEvent => Some(Event::Transfer {
                trb_iova: self.pointer() & !0xf,
                remaining: self.words[2] & 0x00ff_ffff,
                completion_code: self.completion_code(),
                endpoint_id: self.endpoint_id(),
                slot_id: self.slot_id(),
            }),
            TrbType::CommandCompletionEvent => Some(Event::CommandCompletion {
                command_iova: self.pointer() & !0xf,
                parameter: self.words[2] & 0x00ff_ffff,
                completion_code: self.completion_code(),
                slot_id: self.slot_id(),
            }),
            TrbType::PortStatusChangeEvent => Some(Event::PortStatusChange {
                port_id: (self.words[0] >> 24) as u8,
                completion_code: self.completion_code(),
            }),
            TrbType::HostControllerEvent => Some(Event::HostController {
                completion_code: self.completion_code(),
            }),
            _ => None,
        }
    }

    pub(crate) fn link(base_iova: u64, cycle: bool) -> Self {
        let mut trb = typed(TrbType::Link);
        set_pointer(&mut trb, base_iova);
        trb.words[3] |= 1 << 1; // Toggle Cycle
        trb.with_cycle(cycle)
    }
}

/// Parameters for one Normal TRB, used by the HID interrupt-IN transfer ring.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NormalTransfer {
    pub buffer_iova: u64,
    pub length: u32,
    pub td_size: u8,
    pub interrupter_target: u16,
    pub interrupt_on_short_packet: bool,
    pub interrupt_on_completion: bool,
    pub chain: bool,
}

impl NormalTransfer {
    pub const fn interrupt_in(buffer_iova: u64, length: u32) -> Self {
        Self {
            buffer_iova,
            length,
            td_size: 0,
            interrupter_target: 0,
            interrupt_on_short_packet: true,
            interrupt_on_completion: true,
            chain: false,
        }
    }
}

/// Setup Stage TRB TRT field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum SetupTransferType {
    NoData = 0,
    Out = 2,
    In = 3,
}

/// Parsed subset of event TRBs needed for controller bring-up and HID transfers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    Transfer {
        trb_iova: u64,
        remaining: u32,
        completion_code: u8,
        endpoint_id: u8,
        slot_id: u8,
    },
    CommandCompletion {
        command_iova: u64,
        parameter: u32,
        completion_code: u8,
        slot_id: u8,
    },
    PortStatusChange {
        port_id: u8,
        completion_code: u8,
    },
    HostController {
        completion_code: u8,
    },
}

const fn typed(kind: TrbType) -> Trb {
    let mut trb = Trb::zero();
    trb.words[3] = (kind as u32) << TYPE_SHIFT;
    trb
}

fn command_with_context(
    kind: TrbType,
    input_context_iova: u64,
    slot_id: u8,
    bit_nine: bool,
) -> Result<Trb, Error> {
    if input_context_iova & 0xf != 0 || slot_id == 0 {
        return Err(if input_context_iova & 0xf != 0 {
            Error::MisalignedIova
        } else {
            Error::InvalidField
        });
    }
    let mut trb = typed(kind);
    set_pointer(&mut trb, input_context_iova);
    trb.words[3] |= ((bit_nine as u32) << 9) | ((slot_id as u32) << 24);
    Ok(trb)
}

fn set_pointer(trb: &mut Trb, pointer: u64) {
    trb.words[0] = pointer as u32;
    trb.words[1] = (pointer >> 32) as u32;
}

fn validate_buffer(iova: u64, length: u32) -> Result<(), Error> {
    if length != 0 && (iova & 0xffff) + length as u64 > 0x1_0000 {
        return Err(Error::BufferCrosses64K);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use core::mem::{align_of, size_of};

    use super::*;

    #[test]
    fn trb_layout_and_standard_type_table_are_complete() {
        assert_eq!(size_of::<Trb>(), 16);
        assert_eq!(align_of::<Trb>(), 16);
        for raw in 1..=8 {
            assert!(TrbType::from_raw(raw).is_some());
        }
        for raw in 9..=25 {
            assert!(TrbType::from_raw(raw).is_some());
        }
        for raw in 32..=39 {
            assert!(TrbType::from_raw(raw).is_some());
        }
        assert_eq!(TrbType::from_raw(0), None);
        assert_eq!(TrbType::from_raw(31), None);
    }

    #[test]
    fn command_and_transfer_layouts_match_spec_fields() {
        let enable = Trb::enable_slot(5).unwrap();
        assert_eq!(enable.trb_type(), Some(TrbType::EnableSlotCommand));
        assert_eq!((enable.words()[3] >> 16) & 0x1f, 5);

        let normal = Trb::normal_transfer(NormalTransfer::interrupt_in(0x1234_8000, 8)).unwrap();
        assert_eq!(normal.pointer(), 0x1234_8000);
        assert_eq!(normal.words()[2] & 0x1ffff, 8);
        assert_eq!(normal.words()[3] & INTERRUPT_ON_SHORT_PACKET, 1 << 2);
        assert_eq!(normal.words()[3] & INTERRUPT_ON_COMPLETION, 1 << 5);
    }

    #[test]
    fn control_transfer_trbs_encode_direction_and_immediate_setup() {
        let packet = [0x80, 6, 0, 1, 0, 0, 8, 0];
        let setup = Trb::setup_stage(packet, SetupTransferType::In, 0).unwrap();
        assert_eq!(setup.words()[0].to_le_bytes(), packet[..4]);
        assert_eq!(setup.words()[1].to_le_bytes(), packet[4..]);
        assert_ne!(setup.words()[3] & IMMEDIATE_DATA, 0);
        assert_eq!((setup.words()[3] >> 16) & 3, 3);

        let data = Trb::data_stage(0x8000, 8, 0, 0, true).unwrap();
        assert_ne!(data.words()[3] & (1 << 16), 0);
        let status = Trb::status_stage(0, false, true).unwrap();
        assert_ne!(status.words()[3] & INTERRUPT_ON_COMPLETION, 0);
    }

    #[test]
    fn transfer_builder_rejects_a_64k_crossing() {
        assert_eq!(
            Trb::normal_transfer(NormalTransfer::interrupt_in(0x1_fffc, 8)),
            Err(Error::BufferCrosses64K)
        );
    }

    #[test]
    fn decodes_command_and_transfer_events() {
        let command = Trb::from_words([
            0x4000,
            0,
            (1 << 24) | 7,
            (TrbType::CommandCompletionEvent as u32) << 10 | (3 << 24) | 1,
        ]);
        assert_eq!(
            command.decode_event(),
            Some(Event::CommandCompletion {
                command_iova: 0x4000,
                parameter: 7,
                completion_code: 1,
                slot_id: 3,
            })
        );

        let transfer = Trb::from_words([
            0x5000,
            0,
            (13 << 24) | 4,
            (TrbType::TransferEvent as u32) << 10 | (3 << 16) | (2 << 24) | 1,
        ]);
        assert_eq!(
            transfer.decode_event(),
            Some(Event::Transfer {
                trb_iova: 0x5000,
                remaining: 4,
                completion_code: 13,
                endpoint_id: 3,
                slot_id: 2,
            })
        );
    }
}
