use crate::Error;

/// Context stride selected by HCCPARAMS1.CSZ.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextSize {
    Bytes32,
    Bytes64,
}

impl ContextSize {
    pub const fn bytes(self) -> usize {
        match self {
            Self::Bytes32 => 32,
            Self::Bytes64 => 64,
        }
    }

    /// Offset of a DCI inside an Input Context (which begins with an Input Control Context).
    pub const fn input_offset(self, context_index: u8) -> Option<usize> {
        if context_index <= 31 {
            Some((context_index as usize + 1) * self.bytes())
        } else {
            None
        }
    }

    /// Offset of a DCI inside an Output Device Context (whose slot context is DCI 0).
    pub const fn device_offset(self, context_index: u8) -> Option<usize> {
        if context_index <= 31 {
            Some(context_index as usize * self.bytes())
        } else {
            None
        }
    }
}

/// PORTSC speed encoding used by a Slot Context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum UsbSpeed {
    Full = 1,
    Low = 2,
    High = 3,
    Super = 4,
    SuperPlus = 5,
}

/// Software-owned first 32 bytes of a Slot Context.
#[repr(C, align(32))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SlotContext {
    words: [u32; 8],
}

impl SlotContext {
    pub fn root_device(
        speed: UsbSpeed,
        root_hub_port: u8,
        context_entries: u8,
    ) -> Result<Self, Error> {
        if root_hub_port == 0 || !(1..=31).contains(&context_entries) {
            return Err(Error::InvalidField);
        }
        let mut context = Self::default();
        context.words[0] = ((speed as u32) << 20) | ((context_entries as u32) << 27);
        context.words[1] = (root_hub_port as u32) << 16;
        Ok(context)
    }

    pub fn set_route_string(&mut self, route: u32) -> Result<(), Error> {
        if route > 0x000f_ffff {
            return Err(Error::InvalidField);
        }
        self.words[0] = (self.words[0] & !0x000f_ffff) | route;
        Ok(())
    }

    pub fn set_interrupter_target(&mut self, target: u16) -> Result<(), Error> {
        if target > 0x03ff {
            return Err(Error::InvalidField);
        }
        self.words[2] = (self.words[2] & !(0x03ff << 22)) | ((target as u32) << 22);
        Ok(())
    }

    pub const fn words(self) -> [u32; 8] {
        self.words
    }
}

/// xHCI Endpoint Type encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum EndpointType {
    IsochronousOut = 1,
    BulkOut = 2,
    InterruptOut = 3,
    Control = 4,
    IsochronousIn = 5,
    BulkIn = 6,
    InterruptIn = 7,
}

/// Software-owned first 32 bytes of an Endpoint Context.
#[repr(C, align(32))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EndpointContext {
    words: [u32; 8],
}

impl EndpointContext {
    pub fn new(
        endpoint_type: EndpointType,
        max_packet_size: u16,
        dequeue_iova: u64,
    ) -> Result<Self, Error> {
        if max_packet_size == 0 {
            return Err(Error::InvalidField);
        }
        if dequeue_iova & 0xf != 0 {
            return Err(Error::MisalignedIova);
        }
        let mut context = Self::default();
        context.words[1] =
            (3 << 1) | ((endpoint_type as u32) << 3) | ((max_packet_size as u32) << 16);
        context.words[2] = (dequeue_iova as u32 & !0xf) | 1; // DCS starts at 1
        context.words[3] = (dequeue_iova >> 32) as u32;
        Ok(context)
    }

    pub fn control(max_packet_size: u16, dequeue_iova: u64) -> Result<Self, Error> {
        let mut context = Self::new(EndpointType::Control, max_packet_size, dequeue_iova)?;
        context.words[4] = 8; // xHCI 1.2 requires Average TRB Length = 8 for control endpoints.
        Ok(context)
    }

    pub fn interrupt_in(
        max_packet_size: u16,
        interval: u8,
        dequeue_iova: u64,
        average_trb_length: u16,
    ) -> Result<Self, Error> {
        if interval > 15 || average_trb_length == 0 {
            return Err(Error::InvalidField);
        }
        let mut context = Self::new(EndpointType::InterruptIn, max_packet_size, dequeue_iova)?;
        context.words[0] = (interval as u32) << 16;
        context.words[4] = average_trb_length as u32;
        Ok(context)
    }

    pub const fn words(self) -> [u32; 8] {
        self.words
    }

    pub const fn dequeue_iova(self) -> u64 {
        (self.words[2] as u64 & !0xf) | ((self.words[3] as u64) << 32)
    }
}

/// First context of an Input Context: selects contexts affected by a command.
#[repr(C, align(32))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputControlContext {
    words: [u32; 8],
}

impl InputControlContext {
    pub fn add(&mut self, context_index: u8) -> Result<(), Error> {
        if context_index > 31 {
            return Err(Error::InvalidField);
        }
        self.words[1] |= 1 << context_index;
        Ok(())
    }

    pub fn drop(&mut self, context_index: u8) -> Result<(), Error> {
        if !(2..=31).contains(&context_index) {
            return Err(Error::InvalidField);
        }
        self.words[0] |= 1 << context_index;
        Ok(())
    }

    pub const fn words(self) -> [u32; 8] {
        self.words
    }
}

/// Convert a USB endpoint number/direction to its Device Context Index (DCI).
pub const fn endpoint_context_index(endpoint_number: u8, direction_in: bool) -> Option<u8> {
    if endpoint_number == 0 {
        return Some(1);
    }
    if endpoint_number > 15 {
        return None;
    }
    Some(endpoint_number * 2 + direction_in as u8)
}

#[cfg(test)]
mod tests {
    use core::mem::{align_of, size_of};

    use super::*;

    #[test]
    fn slot_context_encodes_x13s_root_device_fields() {
        let mut slot = SlotContext::root_device(UsbSpeed::Full, 3, 3).unwrap();
        slot.set_interrupter_target(2).unwrap();
        let words = slot.words();
        assert_eq!((words[0] >> 20) & 0xf, UsbSpeed::Full as u32);
        assert_eq!((words[0] >> 27) & 0x1f, 3);
        assert_eq!((words[1] >> 16) & 0xff, 3);
        assert_eq!((words[2] >> 22) & 0x3ff, 2);
        assert_eq!(size_of::<SlotContext>(), 32);
        assert_eq!(align_of::<SlotContext>(), 32);
    }

    #[test]
    fn control_and_interrupt_contexts_encode_ring_and_endpoint_shape() {
        let control = EndpointContext::control(64, 0x1_0000).unwrap();
        assert_eq!((control.words()[1] >> 3) & 7, EndpointType::Control as u32);
        assert_eq!((control.words()[1] >> 16) & 0xffff, 64);
        assert_eq!(control.words()[4] & 0xffff, 8);
        assert_eq!(control.dequeue_iova(), 0x1_0000);

        let interrupt = EndpointContext::interrupt_in(8, 4, 0x2_0000, 8).unwrap();
        assert_eq!(
            (interrupt.words()[1] >> 3) & 7,
            EndpointType::InterruptIn as u32
        );
        assert_eq!((interrupt.words()[0] >> 16) & 0xff, 4);
        assert_eq!(interrupt.words()[4] & 0xffff, 8);
        assert_eq!(size_of::<EndpointContext>(), 32);
        assert_eq!(align_of::<EndpointContext>(), 32);
    }

    #[test]
    fn input_flags_and_context_strides_cover_32_and_64_byte_controllers() {
        let mut control = InputControlContext::default();
        control.add(0).unwrap();
        control.add(1).unwrap();
        control.add(3).unwrap();
        assert_eq!(control.words()[1], 0b1011);
        assert_eq!(ContextSize::Bytes32.input_offset(0), Some(32));
        assert_eq!(ContextSize::Bytes32.input_offset(3), Some(128));
        assert_eq!(ContextSize::Bytes64.input_offset(3), Some(256));
        assert_eq!(ContextSize::Bytes64.device_offset(3), Some(192));
    }

    #[test]
    fn endpoint_direction_maps_to_the_spec_dci() {
        assert_eq!(endpoint_context_index(0, false), Some(1));
        assert_eq!(endpoint_context_index(1, false), Some(2));
        assert_eq!(endpoint_context_index(1, true), Some(3));
        assert_eq!(endpoint_context_index(15, true), Some(31));
        assert_eq!(endpoint_context_index(16, true), None);
    }
}
