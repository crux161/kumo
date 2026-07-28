#![no_std]
//j520

//! Allocation-free USB Mass Storage Bulk-Only Transport and SCSI command framing.
//!
//! This crate owns no controller, DMA, or filesystem state. It turns configuration descriptor
//! bytes into a checked bulk endpoint pair and turns the small SCSI command set needed to identify
//! and read a disk into checked BOT wire packets.

const DESCRIPTOR_CONFIGURATION: u8 = 0x02;
const DESCRIPTOR_INTERFACE: u8 = 0x04;
const DESCRIPTOR_ENDPOINT: u8 = 0x05;
const ENDPOINT_TRANSFER_BULK: u8 = 0x02;
const ENDPOINT_DIRECTION_IN: u8 = 0x80;

pub const USB_CLASS_MASS_STORAGE: u8 = 0x08;
pub const USB_SUBCLASS_SCSI_TRANSPARENT: u8 = 0x06;
pub const USB_PROTOCOL_BULK_ONLY: u8 = 0x50;

pub const COMMAND_BLOCK_WRAPPER_BYTES: usize = 31;
pub const COMMAND_STATUS_WRAPPER_BYTES: usize = 13;
pub const STANDARD_INQUIRY_BYTES: u8 = 36;
pub const READ_CAPACITY_10_BYTES: u32 = 8;

const COMMAND_BLOCK_WRAPPER_SIGNATURE: u32 = 0x4342_5355;
const COMMAND_STATUS_WRAPPER_SIGNATURE: u32 = 0x5342_5355;

/// One non-control USB bulk endpoint from a configuration descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BulkEndpoint {
    pub address: u8,
    pub max_packet_size: u16,
}

impl BulkEndpoint {
    pub const fn endpoint_number(self) -> u8 {
        self.address & 0x0f
    }

    pub const fn direction_in(self) -> bool {
        self.address & ENDPOINT_DIRECTION_IN != 0
    }
}

/// A SCSI-transparent Bulk-Only interface and its two data endpoints.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MassStorageInterface {
    pub configuration_value: u8,
    pub interface_number: u8,
    pub alternate_setting: u8,
    pub bulk_in: BulkEndpoint,
    pub bulk_out: BulkEndpoint,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DescriptorError {
    MissingConfiguration,
    Malformed,
    InvalidEndpoint,
    DuplicateEndpoint,
    IncompleteInterface,
}

#[derive(Clone, Copy)]
struct InterfaceCandidate {
    interface_number: u8,
    alternate_setting: u8,
    bulk_in: Option<BulkEndpoint>,
    bulk_out: Option<BulkEndpoint>,
}

impl InterfaceCandidate {
    fn finish(self, configuration_value: u8) -> Option<MassStorageInterface> {
        Some(MassStorageInterface {
            configuration_value,
            interface_number: self.interface_number,
            alternate_setting: self.alternate_setting,
            bulk_in: self.bulk_in?,
            bulk_out: self.bulk_out?,
        })
    }

    fn add_bulk_endpoint(&mut self, endpoint: BulkEndpoint) -> Result<(), DescriptorError> {
        let slot = if endpoint.direction_in() {
            &mut self.bulk_in
        } else {
            &mut self.bulk_out
        };
        if slot.is_some() {
            return Err(DescriptorError::DuplicateEndpoint);
        }
        *slot = Some(endpoint);
        Ok(())
    }
}

/// Find the first complete SCSI-transparent Bulk-Only interface in `config`.
///
/// `config` may be a larger DMA buffer; only the configuration descriptor's `wTotalLength` bytes
/// are walked. Malformed descriptor lengths and malformed endpoints are rejected before their bytes
/// can reach controller setup.
pub fn find_bulk_only_scsi_interface(
    config: &[u8],
) -> Result<Option<MassStorageInterface>, DescriptorError> {
    if config.len() < 2 || config[1] != DESCRIPTOR_CONFIGURATION {
        return Err(DescriptorError::MissingConfiguration);
    }
    if config.len() < 9 || config[0] < 9 {
        return Err(DescriptorError::Malformed);
    }

    let total_length = u16::from_le_bytes([config[2], config[3]]) as usize;
    let first_length = config[0] as usize;
    if total_length < first_length || total_length > config.len() {
        return Err(DescriptorError::Malformed);
    }

    let configuration_value = config[5];
    let mut index = first_length;
    let mut candidate: Option<InterfaceCandidate> = None;
    let mut saw_incomplete_match = false;

    while index < total_length {
        if index + 2 > total_length {
            return Err(DescriptorError::Malformed);
        }
        let length = config[index] as usize;
        let descriptor_type = config[index + 1];
        if length < 2 || index + length > total_length {
            return Err(DescriptorError::Malformed);
        }

        match descriptor_type {
            DESCRIPTOR_INTERFACE => {
                if length < 9 {
                    return Err(DescriptorError::Malformed);
                }
                if let Some(previous) = candidate.take() {
                    if let Some(interface) = previous.finish(configuration_value) {
                        return Ok(Some(interface));
                    }
                    saw_incomplete_match = true;
                }

                if config[index + 5] == USB_CLASS_MASS_STORAGE
                    && config[index + 6] == USB_SUBCLASS_SCSI_TRANSPARENT
                    && config[index + 7] == USB_PROTOCOL_BULK_ONLY
                {
                    candidate = Some(InterfaceCandidate {
                        interface_number: config[index + 2],
                        alternate_setting: config[index + 3],
                        bulk_in: None,
                        bulk_out: None,
                    });
                }
            }
            DESCRIPTOR_ENDPOINT => {
                if length < 7 {
                    return Err(DescriptorError::Malformed);
                }
                if config[index + 3] & 0x03 == ENDPOINT_TRANSFER_BULK {
                    if let Some(current) = candidate.as_mut() {
                        let address = config[index + 2];
                        let raw_packet_size =
                            u16::from_le_bytes([config[index + 4], config[index + 5]]);
                        let packet_size = raw_packet_size & 0x07ff;
                        if address & 0x70 != 0
                            || address & 0x0f == 0
                            || raw_packet_size & !0x07ff != 0
                            || packet_size == 0
                        {
                            return Err(DescriptorError::InvalidEndpoint);
                        }
                        current.add_bulk_endpoint(BulkEndpoint {
                            address,
                            max_packet_size: packet_size,
                        })?;
                    }
                }
            }
            _ => {}
        }
        index += length;
    }

    if let Some(last) = candidate {
        if let Some(interface) = last.finish(configuration_value) {
            return Ok(Some(interface));
        }
        saw_incomplete_match = true;
    }
    if saw_incomplete_match {
        Err(DescriptorError::IncompleteInterface)
    } else {
        Ok(None)
    }
}

/// Class-specific setup packet for GET MAX LUN.
pub const fn get_max_lun_setup(interface_number: u8) -> [u8; 8] {
    [0xa1, 0xfe, 0, 0, interface_number, 0, 1, 0]
}

/// Class-specific setup packet for Bulk-Only Mass Storage Reset.
pub const fn bulk_only_reset_setup(interface_number: u8) -> [u8; 8] {
    [0x21, 0xff, 0, 0, interface_number, 0, 0, 0]
}

pub const fn decode_max_lun(value: u8) -> Result<u8, BotError> {
    if value <= 15 {
        Ok(value)
    } else {
        Err(BotError::InvalidLogicalUnit)
    }
}

/// Direction and presence of the data phase between a CBW and CSW.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataDirection {
    None,
    FromDevice,
    ToDevice,
}

impl DataDirection {
    const fn flag(self) -> u8 {
        match self {
            Self::FromDevice => 0x80,
            Self::None | Self::ToDevice => 0,
        }
    }
}

/// A checked SCSI command descriptor block plus its BOT data expectation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScsiCommand {
    cdb: [u8; 16],
    cdb_length: u8,
    direction: DataDirection,
    transfer_length: u32,
}

impl ScsiCommand {
    pub const fn test_unit_ready() -> Self {
        Self {
            cdb: [0; 16],
            cdb_length: 6,
            direction: DataDirection::None,
            transfer_length: 0,
        }
    }

    pub const fn inquiry(allocation_length: u8) -> Self {
        let mut cdb = [0; 16];
        cdb[0] = 0x12;
        cdb[4] = allocation_length;
        Self {
            cdb,
            cdb_length: 6,
            direction: if allocation_length == 0 {
                DataDirection::None
            } else {
                DataDirection::FromDevice
            },
            transfer_length: allocation_length as u32,
        }
    }

    pub const fn read_capacity_10() -> Self {
        let mut cdb = [0; 16];
        cdb[0] = 0x25;
        Self {
            cdb,
            cdb_length: 10,
            direction: DataDirection::FromDevice,
            transfer_length: READ_CAPACITY_10_BYTES,
        }
    }

    pub fn read_10(
        logical_block_address: u32,
        block_count: u16,
        logical_block_size: u32,
    ) -> Result<Self, ScsiError> {
        if logical_block_size == 0 {
            return Err(ScsiError::InvalidBlockSize);
        }
        if block_count != 0
            && logical_block_address
                .checked_add(block_count as u32 - 1)
                .is_none()
        {
            return Err(ScsiError::AddressRangeOverflow);
        }
        let transfer_length = (block_count as u32)
            .checked_mul(logical_block_size)
            .ok_or(ScsiError::TransferLengthOverflow)?;

        let mut cdb = [0; 16];
        cdb[0] = 0x28;
        cdb[2..6].copy_from_slice(&logical_block_address.to_be_bytes());
        cdb[7..9].copy_from_slice(&block_count.to_be_bytes());
        Ok(Self {
            cdb,
            cdb_length: 10,
            direction: if transfer_length == 0 {
                DataDirection::None
            } else {
                DataDirection::FromDevice
            },
            transfer_length,
        })
    }

    pub fn cdb(&self) -> &[u8] {
        &self.cdb[..self.cdb_length as usize]
    }

    pub const fn direction(&self) -> DataDirection {
        self.direction
    }

    pub const fn transfer_length(&self) -> u32 {
        self.transfer_length
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScsiError {
    InvalidBlockSize,
    AddressRangeOverflow,
    TransferLengthOverflow,
    ResponseTooShort,
    CapacityRequiresReadCapacity16,
}

/// Fixed fields from a 36-byte standard INQUIRY response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StandardInquiryData {
    pub peripheral_qualifier: u8,
    pub peripheral_device_type: u8,
    pub removable_media: bool,
    pub vendor_identification: [u8; 8],
    pub product_identification: [u8; 16],
    pub product_revision: [u8; 4],
}

impl StandardInquiryData {
    pub fn decode(data: &[u8]) -> Result<Self, ScsiError> {
        if data.len() < STANDARD_INQUIRY_BYTES as usize {
            return Err(ScsiError::ResponseTooShort);
        }
        let mut vendor_identification = [0; 8];
        vendor_identification.copy_from_slice(&data[8..16]);
        let mut product_identification = [0; 16];
        product_identification.copy_from_slice(&data[16..32]);
        let mut product_revision = [0; 4];
        product_revision.copy_from_slice(&data[32..36]);
        Ok(Self {
            peripheral_qualifier: data[0] >> 5,
            peripheral_device_type: data[0] & 0x1f,
            removable_media: data[1] & 0x80 != 0,
            vendor_identification,
            product_identification,
            product_revision,
        })
    }
}

/// Capacity returned by READ CAPACITY(10).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaCapacity {
    pub last_logical_block_address: u32,
    pub logical_block_size: u32,
}

impl MediaCapacity {
    pub fn decode_read_capacity_10(data: &[u8]) -> Result<Self, ScsiError> {
        if data.len() < READ_CAPACITY_10_BYTES as usize {
            return Err(ScsiError::ResponseTooShort);
        }
        let last_logical_block_address = u32::from_be_bytes(data[0..4].try_into().unwrap());
        if last_logical_block_address == u32::MAX {
            return Err(ScsiError::CapacityRequiresReadCapacity16);
        }
        let logical_block_size = u32::from_be_bytes(data[4..8].try_into().unwrap());
        if logical_block_size == 0 {
            return Err(ScsiError::InvalidBlockSize);
        }
        Ok(Self {
            last_logical_block_address,
            logical_block_size,
        })
    }

    pub const fn block_count(self) -> u64 {
        self.last_logical_block_address as u64 + 1
    }

    pub const fn byte_length(self) -> u64 {
        self.block_count() * self.logical_block_size as u64
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BotError {
    InvalidLogicalUnit,
    InvalidCommandLength,
    InvalidDataPhase,
    InvalidStatusLength,
    InvalidStatusSignature,
    StatusTagMismatch,
    InvalidResidue,
    InvalidStatus,
}

/// An exact 31-byte BOT Command Block Wrapper.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandBlockWrapper {
    bytes: [u8; COMMAND_BLOCK_WRAPPER_BYTES],
}

impl CommandBlockWrapper {
    pub fn for_scsi(tag: u32, logical_unit: u8, command: &ScsiCommand) -> Result<Self, BotError> {
        Self::new(
            tag,
            logical_unit,
            command.direction,
            command.transfer_length,
            command.cdb(),
        )
    }

    pub fn new(
        tag: u32,
        logical_unit: u8,
        direction: DataDirection,
        transfer_length: u32,
        command: &[u8],
    ) -> Result<Self, BotError> {
        if logical_unit > 15 {
            return Err(BotError::InvalidLogicalUnit);
        }
        if command.is_empty() || command.len() > 16 {
            return Err(BotError::InvalidCommandLength);
        }
        if (transfer_length == 0) != (direction == DataDirection::None) {
            return Err(BotError::InvalidDataPhase);
        }

        let mut bytes = [0; COMMAND_BLOCK_WRAPPER_BYTES];
        bytes[0..4].copy_from_slice(&COMMAND_BLOCK_WRAPPER_SIGNATURE.to_le_bytes());
        bytes[4..8].copy_from_slice(&tag.to_le_bytes());
        bytes[8..12].copy_from_slice(&transfer_length.to_le_bytes());
        bytes[12] = direction.flag();
        bytes[13] = logical_unit;
        bytes[14] = command.len() as u8;
        bytes[15..15 + command.len()].copy_from_slice(command);
        Ok(Self { bytes })
    }

    pub const fn as_bytes(&self) -> &[u8; COMMAND_BLOCK_WRAPPER_BYTES] {
        &self.bytes
    }

    pub const fn into_bytes(self) -> [u8; COMMAND_BLOCK_WRAPPER_BYTES] {
        self.bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandStatus {
    Passed,
    Failed,
    PhaseError,
}

/// A validated BOT Command Status Wrapper associated with one CBW tag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandStatusWrapper {
    pub tag: u32,
    pub residue: u32,
    pub transferred_bytes: u32,
    pub status: CommandStatus,
}

impl CommandStatusWrapper {
    pub fn decode(
        bytes: &[u8],
        expected_tag: u32,
        expected_transfer_length: u32,
    ) -> Result<Self, BotError> {
        if bytes.len() != COMMAND_STATUS_WRAPPER_BYTES {
            return Err(BotError::InvalidStatusLength);
        }
        if u32::from_le_bytes(bytes[0..4].try_into().unwrap()) != COMMAND_STATUS_WRAPPER_SIGNATURE {
            return Err(BotError::InvalidStatusSignature);
        }
        let tag = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        if tag != expected_tag {
            return Err(BotError::StatusTagMismatch);
        }
        let residue = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let transferred_bytes = expected_transfer_length
            .checked_sub(residue)
            .ok_or(BotError::InvalidResidue)?;
        let status = match bytes[12] {
            0 => CommandStatus::Passed,
            1 => CommandStatus::Failed,
            2 => CommandStatus::PhaseError,
            _ => return Err(BotError::InvalidStatus),
        };
        Ok(Self {
            tag,
            residue,
            transferred_bytes,
            status,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_scsi_bulk_only_interface_and_ignores_other_interfaces() {
        let config = [
            9, 2, 48, 0, 2, 5, 0, 0x80, 50, // configuration
            9, 4, 0, 0, 1, 3, 1, 1, 0, // HID interface
            7, 5, 0x81, 3, 8, 0, 10, // interrupt IN
            9, 4, 3, 1, 2, 8, 6, 0x50, 0, // SCSI transparent BOT
            7, 5, 2, 2, 0, 2, 0, // bulk OUT, 512 bytes
            7, 5, 0x83, 2, 0, 2, 0, // bulk IN, 512 bytes
        ];
        let found = find_bulk_only_scsi_interface(&config)
            .unwrap()
            .expect("mass storage interface");
        assert_eq!(found.configuration_value, 5);
        assert_eq!(found.interface_number, 3);
        assert_eq!(found.alternate_setting, 1);
        assert_eq!(found.bulk_out.endpoint_number(), 2);
        assert_eq!(found.bulk_in.endpoint_number(), 3);
        assert_eq!(found.bulk_in.max_packet_size, 512);
    }

    #[test]
    fn descriptor_walk_rejects_incomplete_and_malformed_interfaces() {
        let incomplete = [
            9, 2, 25, 0, 1, 1, 0, 0x80, 50, // configuration
            9, 4, 0, 0, 1, 8, 6, 0x50, 0, // BOT interface
            7, 5, 0x81, 2, 64, 0, 0, // only bulk IN
        ];
        assert_eq!(
            find_bulk_only_scsi_interface(&incomplete),
            Err(DescriptorError::IncompleteInterface)
        );

        let mut malformed_endpoint = incomplete;
        malformed_endpoint[20] = 0x91;
        assert_eq!(
            find_bulk_only_scsi_interface(&malformed_endpoint),
            Err(DescriptorError::InvalidEndpoint)
        );

        let mut invalid_packet_size = incomplete;
        invalid_packet_size[22] = 0;
        invalid_packet_size[23] = 8;
        assert_eq!(
            find_bulk_only_scsi_interface(&invalid_packet_size),
            Err(DescriptorError::InvalidEndpoint)
        );

        let mut truncated = incomplete;
        truncated[2] = 26;
        assert_eq!(
            find_bulk_only_scsi_interface(&truncated),
            Err(DescriptorError::Malformed)
        );
        assert_eq!(
            find_bulk_only_scsi_interface(&[9, 4]),
            Err(DescriptorError::MissingConfiguration)
        );
    }

    #[test]
    fn descriptor_walk_skips_other_mass_storage_protocols() {
        let uas = [
            9, 2, 18, 0, 1, 1, 0, 0x80, 50, // configuration
            9, 4, 0, 0, 0, 8, 6, 0x62, 0, // USB Attached SCSI, not BOT
        ];
        assert_eq!(find_bulk_only_scsi_interface(&uas), Ok(None));
    }

    #[test]
    fn class_specific_setup_packets_match_bulk_only_transport() {
        assert_eq!(get_max_lun_setup(3), [0xa1, 0xfe, 0, 0, 3, 0, 1, 0]);
        assert_eq!(bulk_only_reset_setup(3), [0x21, 0xff, 0, 0, 3, 0, 0, 0]);
        assert_eq!(decode_max_lun(15), Ok(15));
        assert_eq!(decode_max_lun(16), Err(BotError::InvalidLogicalUnit));
    }

    #[test]
    fn inquiry_cbw_has_exact_little_endian_wire_form() {
        let command = ScsiCommand::inquiry(STANDARD_INQUIRY_BYTES);
        let cbw = CommandBlockWrapper::for_scsi(0x1234_5678, 2, &command)
            .unwrap()
            .into_bytes();
        assert_eq!(&cbw[0..4], b"USBC");
        assert_eq!(&cbw[4..8], &[0x78, 0x56, 0x34, 0x12]);
        assert_eq!(&cbw[8..12], &[36, 0, 0, 0]);
        assert_eq!(&cbw[12..15], &[0x80, 2, 6]);
        assert_eq!(&cbw[15..21], &[0x12, 0, 0, 0, 36, 0]);
        assert_eq!(&cbw[21..], &[0; 10]);
    }

    #[test]
    fn cbw_rejects_invalid_fields_and_data_phase_mismatches() {
        assert_eq!(
            CommandBlockWrapper::new(0, 16, DataDirection::None, 0, &[0]),
            Err(BotError::InvalidLogicalUnit)
        );
        assert_eq!(
            CommandBlockWrapper::new(0, 0, DataDirection::None, 0, &[]),
            Err(BotError::InvalidCommandLength)
        );
        assert_eq!(
            CommandBlockWrapper::new(0, 0, DataDirection::None, 0, &[0; 17]),
            Err(BotError::InvalidCommandLength)
        );
        assert_eq!(
            CommandBlockWrapper::new(0, 0, DataDirection::FromDevice, 0, &[0]),
            Err(BotError::InvalidDataPhase)
        );
    }

    #[test]
    fn scsi_commands_encode_capacity_and_checked_reads() {
        assert_eq!(ScsiCommand::test_unit_ready().cdb(), &[0; 6]);

        let capacity = ScsiCommand::read_capacity_10();
        assert_eq!(capacity.cdb(), &[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(capacity.transfer_length(), 8);

        let read = ScsiCommand::read_10(0x1234_5678, 0x0203, 512).unwrap();
        assert_eq!(
            read.cdb(),
            &[0x28, 0, 0x12, 0x34, 0x56, 0x78, 0, 0x02, 0x03, 0]
        );
        assert_eq!(read.transfer_length(), 0x0004_0600);
        assert_eq!(read.direction(), DataDirection::FromDevice);

        let empty_read = ScsiCommand::read_10(9, 0, 512).unwrap();
        assert_eq!(empty_read.transfer_length(), 0);
        assert_eq!(empty_read.direction(), DataDirection::None);
        assert_eq!(
            ScsiCommand::read_10(u32::MAX, 2, 512),
            Err(ScsiError::AddressRangeOverflow)
        );
        assert_eq!(
            ScsiCommand::read_10(0, 2, u32::MAX),
            Err(ScsiError::TransferLengthOverflow)
        );
        assert_eq!(
            ScsiCommand::read_10(0, 1, 0),
            Err(ScsiError::InvalidBlockSize)
        );
    }

    #[test]
    fn csw_validation_checks_signature_tag_residue_and_status() {
        let passed = [
            b'U', b'S', b'B', b'S', 0x78, 0x56, 0x34, 0x12, 4, 0, 0, 0, 0,
        ];
        let decoded = CommandStatusWrapper::decode(&passed, 0x1234_5678, 36).unwrap();
        assert_eq!(decoded.residue, 4);
        assert_eq!(decoded.transferred_bytes, 32);
        assert_eq!(decoded.status, CommandStatus::Passed);

        let mut failed = passed;
        failed[12] = 1;
        assert_eq!(
            CommandStatusWrapper::decode(&failed, 0x1234_5678, 36)
                .unwrap()
                .status,
            CommandStatus::Failed
        );
        failed[12] = 2;
        assert_eq!(
            CommandStatusWrapper::decode(&failed, 0x1234_5678, 36)
                .unwrap()
                .status,
            CommandStatus::PhaseError
        );

        let mut invalid = passed;
        invalid[0] = 0;
        assert_eq!(
            CommandStatusWrapper::decode(&invalid, 0x1234_5678, 36),
            Err(BotError::InvalidStatusSignature)
        );
        assert_eq!(
            CommandStatusWrapper::decode(&passed, 7, 36),
            Err(BotError::StatusTagMismatch)
        );
        assert_eq!(
            CommandStatusWrapper::decode(&passed, 0x1234_5678, 3),
            Err(BotError::InvalidResidue)
        );
    }

    #[test]
    fn inquiry_and_capacity_responses_decode_without_allocation() {
        let mut inquiry = [0u8; 36];
        inquiry[0] = 0x00;
        inquiry[1] = 0x80;
        inquiry[8..16].copy_from_slice(b"KUMO    ");
        inquiry[16..32].copy_from_slice(b"USB DISK        ");
        inquiry[32..36].copy_from_slice(b"1.00");
        let decoded = StandardInquiryData::decode(&inquiry).unwrap();
        assert_eq!(decoded.peripheral_device_type, 0);
        assert!(decoded.removable_media);
        assert_eq!(&decoded.vendor_identification, b"KUMO    ");
        assert_eq!(&decoded.product_identification, b"USB DISK        ");

        let capacity =
            MediaCapacity::decode_read_capacity_10(&[0, 0, 0x0f, 0xff, 0, 0, 2, 0]).unwrap();
        assert_eq!(capacity.block_count(), 4096);
        assert_eq!(capacity.logical_block_size, 512);
        assert_eq!(capacity.byte_length(), 2 * 1024 * 1024);
        assert_eq!(
            MediaCapacity::decode_read_capacity_10(&[0xff, 0xff, 0xff, 0xff, 0, 0, 2, 0]),
            Err(ScsiError::CapacityRequiresReadCapacity16)
        );
    }
}
