//j443
//j446

//! Multiple APIC Description Table records needed to route legacy interrupt sources.
//!
//! ACPI 6.3 §§5.2.12, 5.2.12.3, and 5.2.12.5 define the bounded records decoded here.

use super::{AcpiError, DescriptionTable, SDT_HEADER_LEN};

pub const MADT_HEADER_LEN: usize = SDT_HEADER_LEN + 8;

const IO_APIC_ENTRY_TYPE: u8 = 1;
const IO_APIC_ENTRY_LEN: usize = 12;
const SOURCE_OVERRIDE_ENTRY_TYPE: u8 = 2;
const SOURCE_OVERRIDE_ENTRY_LEN: usize = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MadtError {
    Table(AcpiError),
    InvalidSignature([u8; 4]),
    HeaderTooShort {
        length: usize,
    },
    EntryHeaderTruncated {
        offset: usize,
        remaining: usize,
    },
    EntryTooShort {
        offset: usize,
        entry_type: u8,
        length: usize,
    },
    EntryTruncated {
        offset: usize,
        entry_type: u8,
        length: usize,
        remaining: usize,
    },
    InvalidKnownEntryLength {
        entry_type: u8,
        length: usize,
        expected: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyIrqRouteError {
    NonIsaBus { source: u8, bus: u8 },
    DuplicateOverride { source: u8 },
    ReservedFlagBits { source: u8, flags: u16 },
    ReservedPolarity { source: u8 },
    ReservedTriggerMode { source: u8 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LegacyIrqRoute {
    pub isa_irq: u8,
    pub global_system_interrupt: u32,
    pub polarity: InterruptPolarity,
    pub trigger_mode: InterruptTriggerMode,
    pub overridden: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Madt<'a> {
    table: DescriptionTable<'a>,
}

impl<'a> Madt<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, MadtError> {
        let table = DescriptionTable::parse(bytes).map_err(MadtError::Table)?;
        if table.header().signature != *b"APIC" {
            return Err(MadtError::InvalidSignature(table.header().signature));
        }
        if table.bytes().len() < MADT_HEADER_LEN {
            return Err(MadtError::HeaderTooShort {
                length: table.bytes().len(),
            });
        }
        validate_entries(&table.bytes()[MADT_HEADER_LEN..])?;
        Ok(Self { table })
    }

    pub fn local_interrupt_controller_address(self) -> u32 {
        read_u32(self.table.bytes(), SDT_HEADER_LEN)
    }

    pub fn flags(self) -> u32 {
        read_u32(self.table.bytes(), SDT_HEADER_LEN + 4)
    }

    pub fn pcat_compatible(self) -> bool {
        self.flags() & 1 != 0
    }

    pub fn entries(self) -> MadtEntries<'a> {
        MadtEntries {
            remaining: &self.table.bytes()[MADT_HEADER_LEN..],
        }
    }

    /// Resolve one ISA IRQ through its optional MADT override.
    ///
    /// ACPI 6.3 §5.2.12.5 defines absent overrides as identity mappings. MPS flag values that
    /// conform to ISA resolve to active-high, edge-triggered inputs.
    pub fn legacy_irq_route(self, source: u8) -> Result<LegacyIrqRoute, LegacyIrqRouteError> {
        let mut selected = None;
        for entry in self.entries() {
            let MadtEntry::InterruptSourceOverride(override_entry) = entry else {
                continue;
            };
            if override_entry.source != source {
                continue;
            }
            if override_entry.bus != 0 {
                return Err(LegacyIrqRouteError::NonIsaBus {
                    source,
                    bus: override_entry.bus,
                });
            }
            if selected.replace(override_entry).is_some() {
                return Err(LegacyIrqRouteError::DuplicateOverride { source });
            }
        }

        let Some(override_entry) = selected else {
            return Ok(LegacyIrqRoute {
                isa_irq: source,
                global_system_interrupt: u32::from(source),
                polarity: InterruptPolarity::ActiveHigh,
                trigger_mode: InterruptTriggerMode::Edge,
                overridden: false,
            });
        };
        if !override_entry.flags.reserved_bits_are_zero() {
            return Err(LegacyIrqRouteError::ReservedFlagBits {
                source,
                flags: override_entry.flags.0,
            });
        }
        let polarity = match override_entry.flags.polarity() {
            InterruptPolarity::Conforms | InterruptPolarity::ActiveHigh => {
                InterruptPolarity::ActiveHigh
            }
            InterruptPolarity::ActiveLow => InterruptPolarity::ActiveLow,
            InterruptPolarity::Reserved => {
                return Err(LegacyIrqRouteError::ReservedPolarity { source });
            }
        };
        let trigger_mode = match override_entry.flags.trigger_mode() {
            InterruptTriggerMode::Conforms | InterruptTriggerMode::Edge => {
                InterruptTriggerMode::Edge
            }
            InterruptTriggerMode::Level => InterruptTriggerMode::Level,
            InterruptTriggerMode::Reserved => {
                return Err(LegacyIrqRouteError::ReservedTriggerMode { source });
            }
        };
        Ok(LegacyIrqRoute {
            isa_irq: source,
            global_system_interrupt: override_entry.global_system_interrupt,
            polarity,
            trigger_mode,
            overridden: true,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoApic {
    pub id: u8,
    pub address: u32,
    pub global_system_interrupt_base: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterruptSourceOverride {
    pub bus: u8,
    pub source: u8,
    pub global_system_interrupt: u32,
    pub flags: MpsIntiFlags,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MadtEntry<'a> {
    IoApic(IoApic),
    InterruptSourceOverride(InterruptSourceOverride),
    Other { entry_type: u8, bytes: &'a [u8] },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MadtEntries<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for MadtEntries<'a> {
    type Item = MadtEntry<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let entry_type = self.remaining[0];
        let length = self.remaining[1] as usize;
        let bytes = &self.remaining[..length];
        self.remaining = &self.remaining[length..];

        Some(match entry_type {
            IO_APIC_ENTRY_TYPE => MadtEntry::IoApic(IoApic {
                id: bytes[2],
                address: read_u32(bytes, 4),
                global_system_interrupt_base: read_u32(bytes, 8),
            }),
            SOURCE_OVERRIDE_ENTRY_TYPE => {
                MadtEntry::InterruptSourceOverride(InterruptSourceOverride {
                    bus: bytes[2],
                    source: bytes[3],
                    global_system_interrupt: read_u32(bytes, 4),
                    flags: MpsIntiFlags(read_u16(bytes, 8)),
                })
            }
            _ => MadtEntry::Other { entry_type, bytes },
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MpsIntiFlags(pub u16);

impl MpsIntiFlags {
    pub const fn polarity(self) -> InterruptPolarity {
        match self.0 & 0b11 {
            0 => InterruptPolarity::Conforms,
            1 => InterruptPolarity::ActiveHigh,
            2 => InterruptPolarity::Reserved,
            _ => InterruptPolarity::ActiveLow,
        }
    }

    pub const fn trigger_mode(self) -> InterruptTriggerMode {
        match (self.0 >> 2) & 0b11 {
            0 => InterruptTriggerMode::Conforms,
            1 => InterruptTriggerMode::Edge,
            2 => InterruptTriggerMode::Reserved,
            _ => InterruptTriggerMode::Level,
        }
    }

    pub const fn reserved_bits_are_zero(self) -> bool {
        self.0 & !0x000f == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptPolarity {
    Conforms,
    ActiveHigh,
    Reserved,
    ActiveLow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptTriggerMode {
    Conforms,
    Edge,
    Reserved,
    Level,
}

fn validate_entries(mut bytes: &[u8]) -> Result<(), MadtError> {
    let mut offset = MADT_HEADER_LEN;
    while !bytes.is_empty() {
        if bytes.len() < 2 {
            return Err(MadtError::EntryHeaderTruncated {
                offset,
                remaining: bytes.len(),
            });
        }
        let entry_type = bytes[0];
        let length = bytes[1] as usize;
        if length < 2 {
            return Err(MadtError::EntryTooShort {
                offset,
                entry_type,
                length,
            });
        }
        if length > bytes.len() {
            return Err(MadtError::EntryTruncated {
                offset,
                entry_type,
                length,
                remaining: bytes.len(),
            });
        }

        let expected = match entry_type {
            IO_APIC_ENTRY_TYPE => Some(IO_APIC_ENTRY_LEN),
            SOURCE_OVERRIDE_ENTRY_TYPE => Some(SOURCE_OVERRIDE_ENTRY_LEN),
            _ => None,
        };
        if let Some(expected) = expected {
            if length != expected {
                return Err(MadtError::InvalidKnownEntryLength {
                    entry_type,
                    length,
                    expected,
                });
            }
        }

        bytes = &bytes[length..];
        offset += length;
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_checksum(bytes: &mut [u8]) {
        bytes[9] = 0;
        let sum = bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        bytes[9] = 0u8.wrapping_sub(sum);
    }

    fn madt<const N: usize>(entries: &[u8]) -> [u8; N] {
        assert_eq!(N, MADT_HEADER_LEN + entries.len());
        let mut bytes = [0u8; N];
        bytes[..4].copy_from_slice(b"APIC");
        bytes[4..8].copy_from_slice(&(N as u32).to_le_bytes());
        bytes[8] = 5;
        bytes[10..16].copy_from_slice(b"KUMO  ");
        bytes[36..40].copy_from_slice(&0xfee0_0000u32.to_le_bytes());
        bytes[40..44].copy_from_slice(&1u32.to_le_bytes());
        bytes[MADT_HEADER_LEN..].copy_from_slice(entries);
        set_checksum(&mut bytes);
        bytes
    }

    #[test]
    fn decodes_io_apic_and_legacy_source_override() {
        let mut entries = [0u8; IO_APIC_ENTRY_LEN + SOURCE_OVERRIDE_ENTRY_LEN];
        entries[..12].copy_from_slice(&[1, 12, 7, 0, 0x00, 0x00, 0xc0, 0xfe, 0, 0, 0, 0]);
        entries[12..].copy_from_slice(&[2, 10, 0, 0, 2, 0, 0, 0, 0x0f, 0]);
        let bytes =
            madt::<{ MADT_HEADER_LEN + IO_APIC_ENTRY_LEN + SOURCE_OVERRIDE_ENTRY_LEN }>(&entries);
        let table = Madt::parse(&bytes).unwrap();

        assert_eq!(table.local_interrupt_controller_address(), 0xfee0_0000);
        assert!(table.pcat_compatible());
        assert_eq!(
            table.legacy_irq_route(0),
            Ok(LegacyIrqRoute {
                isa_irq: 0,
                global_system_interrupt: 2,
                polarity: InterruptPolarity::ActiveLow,
                trigger_mode: InterruptTriggerMode::Level,
                overridden: true,
            })
        );
        let mut entries = table.entries();
        assert_eq!(
            entries.next(),
            Some(MadtEntry::IoApic(IoApic {
                id: 7,
                address: 0xfec0_0000,
                global_system_interrupt_base: 0,
            }))
        );
        assert_eq!(
            entries.next(),
            Some(MadtEntry::InterruptSourceOverride(
                InterruptSourceOverride {
                    bus: 0,
                    source: 0,
                    global_system_interrupt: 2,
                    flags: MpsIntiFlags(0x000f),
                }
            ))
        );
        assert_eq!(entries.next(), None);
    }

    #[test]
    fn decodes_mps_polarity_and_trigger_fields() {
        let conforms = MpsIntiFlags(0);
        assert_eq!(conforms.polarity(), InterruptPolarity::Conforms);
        assert_eq!(conforms.trigger_mode(), InterruptTriggerMode::Conforms);
        assert!(conforms.reserved_bits_are_zero());

        let low_level = MpsIntiFlags(0x000f);
        assert_eq!(low_level.polarity(), InterruptPolarity::ActiveLow);
        assert_eq!(low_level.trigger_mode(), InterruptTriggerMode::Level);
        assert!(low_level.reserved_bits_are_zero());

        assert!(!MpsIntiFlags(0x0010).reserved_bits_are_zero());
    }

    #[test]
    fn resolves_identity_and_conforming_isa_routes() {
        let identity_bytes = madt::<MADT_HEADER_LEN>(&[]);
        let identity = Madt::parse(&identity_bytes).unwrap();
        assert_eq!(
            identity.legacy_irq_route(0),
            Ok(LegacyIrqRoute {
                isa_irq: 0,
                global_system_interrupt: 0,
                polarity: InterruptPolarity::ActiveHigh,
                trigger_mode: InterruptTriggerMode::Edge,
                overridden: false,
            })
        );

        let bytes = madt::<{ MADT_HEADER_LEN + SOURCE_OVERRIDE_ENTRY_LEN }>(&[
            2, 10, 0, 0, 2, 0, 0, 0, 0, 0,
        ]);
        let overridden = Madt::parse(&bytes).unwrap();
        assert_eq!(
            overridden.legacy_irq_route(0),
            Ok(LegacyIrqRoute {
                isa_irq: 0,
                global_system_interrupt: 2,
                polarity: InterruptPolarity::ActiveHigh,
                trigger_mode: InterruptTriggerMode::Edge,
                overridden: true,
            })
        );
    }

    #[test]
    fn rejects_ambiguous_or_reserved_legacy_routes() {
        let duplicate = madt::<{ MADT_HEADER_LEN + SOURCE_OVERRIDE_ENTRY_LEN * 2 }>(&[
            2, 10, 0, 0, 2, 0, 0, 0, 0, 0, 2, 10, 0, 0, 2, 0, 0, 0, 0, 0,
        ]);
        assert_eq!(
            Madt::parse(&duplicate).unwrap().legacy_irq_route(0),
            Err(LegacyIrqRouteError::DuplicateOverride { source: 0 })
        );

        let reserved = madt::<{ MADT_HEADER_LEN + SOURCE_OVERRIDE_ENTRY_LEN }>(&[
            2, 10, 0, 0, 2, 0, 0, 0, 2, 0,
        ]);
        assert_eq!(
            Madt::parse(&reserved).unwrap().legacy_irq_route(0),
            Err(LegacyIrqRouteError::ReservedPolarity { source: 0 })
        );
    }

    #[test]
    fn preserves_unknown_records_for_later_controller_slices() {
        let bytes = madt::<{ MADT_HEADER_LEN + 8 }>(&[0, 8, 3, 9, 1, 0, 0, 0]);
        let table = Madt::parse(&bytes).unwrap();
        assert_eq!(
            table.entries().next(),
            Some(MadtEntry::Other {
                entry_type: 0,
                bytes: &[0, 8, 3, 9, 1, 0, 0, 0],
            })
        );
    }

    #[test]
    fn rejects_truncated_and_wrong_size_records_before_iteration() {
        let truncated = madt::<{ MADT_HEADER_LEN + 4 }>(&[1, 12, 0, 0]);
        assert_eq!(
            Madt::parse(&truncated),
            Err(MadtError::EntryTruncated {
                offset: MADT_HEADER_LEN,
                entry_type: 1,
                length: 12,
                remaining: 4,
            })
        );

        let wrong_size = madt::<{ MADT_HEADER_LEN + 8 }>(&[2, 8, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            Madt::parse(&wrong_size),
            Err(MadtError::InvalidKnownEntryLength {
                entry_type: 2,
                length: 8,
                expected: SOURCE_OVERRIDE_ENTRY_LEN,
            })
        );
    }

    #[test]
    fn requires_the_apic_signature_and_fixed_header() {
        let mut wrong_signature = madt::<MADT_HEADER_LEN>(&[]);
        wrong_signature[..4].copy_from_slice(b"FACP");
        set_checksum(&mut wrong_signature);
        assert_eq!(
            Madt::parse(&wrong_signature),
            Err(MadtError::InvalidSignature(*b"FACP"))
        );

        let mut short = [0u8; SDT_HEADER_LEN];
        short[..4].copy_from_slice(b"APIC");
        short[4..8].copy_from_slice(&(SDT_HEADER_LEN as u32).to_le_bytes());
        set_checksum(&mut short);
        assert_eq!(
            Madt::parse(&short),
            Err(MadtError::HeaderTooShort {
                length: SDT_HEADER_LEN,
            })
        );
    }
}
