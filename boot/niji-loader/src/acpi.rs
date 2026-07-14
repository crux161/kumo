//j441
//j443
//j444

//! Allocation-free ACPI root-table parsing.
//!
//! The caller owns physical-address translation. This module accepts bounded byte slices, validates
//! every checksum and declared length before exposing fields, and returns the physical addresses that
//! the next discovery layer may map. ACPI 6.3 §§5.2.5–5.2.8 are the governing format definitions.

pub mod madt;

pub const RSDP_V1_LEN: usize = 20;
pub const RSDP_V2_MIN_LEN: usize = 36;
pub const SDT_HEADER_LEN: usize = 36;

const RSDP_SIGNATURE: [u8; 8] = *b"RSD PTR ";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChecksumKind {
    RsdpLegacy,
    RsdpExtended,
    SystemDescriptionTable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcpiError {
    Truncated {
        needed: usize,
        available: usize,
    },
    InvalidRsdpSignature([u8; 8]),
    InvalidSdtSignature {
        expected: [u8; 4],
        found: [u8; 4],
    },
    InvalidLength {
        length: usize,
        minimum: usize,
    },
    InvalidChecksum(ChecksumKind),
    MissingRootAddress,
    MisalignedRootEntries {
        payload_len: usize,
        entry_len: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootTableKind {
    Rsdt,
    Xsdt,
}

impl RootTableKind {
    pub const fn signature(self) -> [u8; 4] {
        match self {
            Self::Rsdt => *b"RSDT",
            Self::Xsdt => *b"XSDT",
        }
    }

    const fn entry_len(self) -> usize {
        match self {
            Self::Rsdt => 4,
            Self::Xsdt => 8,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RootAddress {
    pub kind: RootTableKind,
    pub physical_address: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rsdp {
    pub revision: u8,
    pub length: usize,
    pub rsdt_address: u32,
    pub xsdt_address: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RsdpLocation {
    pub physical_address: u64,
    pub rsdp: Rsdp,
}

/// Search a bounded physical-memory window on the 16-byte boundaries required by ACPI 6.3 §5.2.5.1.
///
/// `physical_base` may itself be unaligned; candidate alignment is computed in physical-address
/// space rather than relative to the supplied slice.
pub fn find_rsdp(bytes: &[u8], physical_base: u64) -> Option<RsdpLocation> {
    let first = ((16 - (physical_base & 15)) & 15) as usize;
    let mut offset = first;
    while offset.checked_add(RSDP_V1_LEN)? <= bytes.len() {
        if bytes[offset..].starts_with(&RSDP_SIGNATURE) {
            if let Ok(rsdp) = Rsdp::parse(&bytes[offset..]) {
                return Some(RsdpLocation {
                    physical_address: physical_base.checked_add(offset as u64)?,
                    rsdp,
                });
            }
        }
        offset = offset.checked_add(16)?;
    }
    None
}

impl Rsdp {
    pub fn parse(bytes: &[u8]) -> Result<Self, AcpiError> {
        require_len(bytes, RSDP_V1_LEN)?;

        let signature = read_array_8(bytes, 0);
        if signature != RSDP_SIGNATURE {
            return Err(AcpiError::InvalidRsdpSignature(signature));
        }
        if !checksum_is_zero(&bytes[..RSDP_V1_LEN]) {
            return Err(AcpiError::InvalidChecksum(ChecksumKind::RsdpLegacy));
        }

        let revision = bytes[15];
        let rsdt_address = read_u32(bytes, 16);
        if revision < 2 {
            if rsdt_address == 0 {
                return Err(AcpiError::MissingRootAddress);
            }
            return Ok(Self {
                revision,
                length: RSDP_V1_LEN,
                rsdt_address,
                xsdt_address: None,
            });
        }

        require_len(bytes, 24)?;
        let length = read_u32(bytes, 20) as usize;
        if length < RSDP_V2_MIN_LEN {
            return Err(AcpiError::InvalidLength {
                length,
                minimum: RSDP_V2_MIN_LEN,
            });
        }
        require_len(bytes, length)?;
        if !checksum_is_zero(&bytes[..length]) {
            return Err(AcpiError::InvalidChecksum(ChecksumKind::RsdpExtended));
        }

        let xsdt = read_u64(bytes, 24);
        if xsdt == 0 && rsdt_address == 0 {
            return Err(AcpiError::MissingRootAddress);
        }
        Ok(Self {
            revision,
            length,
            rsdt_address,
            xsdt_address: (xsdt != 0).then_some(xsdt),
        })
    }

    /// Select the XSDT whenever firmware supplied one, as required by ACPI 6.3 §5.2.8.
    pub const fn root_address(self) -> RootAddress {
        match self.xsdt_address {
            Some(physical_address) => RootAddress {
                kind: RootTableKind::Xsdt,
                physical_address,
            },
            None => RootAddress {
                kind: RootTableKind::Rsdt,
                physical_address: self.rsdt_address as u64,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SdtHeader {
    pub signature: [u8; 4],
    pub length: usize,
    pub revision: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DescriptionTable<'a> {
    header: SdtHeader,
    bytes: &'a [u8],
}

impl<'a> DescriptionTable<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, AcpiError> {
        require_len(bytes, SDT_HEADER_LEN)?;
        let length = read_u32(bytes, 4) as usize;
        if length < SDT_HEADER_LEN {
            return Err(AcpiError::InvalidLength {
                length,
                minimum: SDT_HEADER_LEN,
            });
        }
        require_len(bytes, length)?;
        let bytes = &bytes[..length];
        if !checksum_is_zero(bytes) {
            return Err(AcpiError::InvalidChecksum(
                ChecksumKind::SystemDescriptionTable,
            ));
        }
        Ok(Self {
            header: SdtHeader {
                signature: read_array_4(bytes, 0),
                length,
                revision: bytes[8],
            },
            bytes,
        })
    }

    pub const fn header(self) -> SdtHeader {
        self.header
    }

    pub const fn bytes(self) -> &'a [u8] {
        self.bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RootTable<'a> {
    table: DescriptionTable<'a>,
    kind: RootTableKind,
}

impl<'a> RootTable<'a> {
    pub fn parse(bytes: &'a [u8], kind: RootTableKind) -> Result<Self, AcpiError> {
        let table = DescriptionTable::parse(bytes)?;
        let expected = kind.signature();
        let found = table.header().signature;
        if found != expected {
            return Err(AcpiError::InvalidSdtSignature { expected, found });
        }

        let payload_len = table.bytes().len() - SDT_HEADER_LEN;
        let entry_len = kind.entry_len();
        if payload_len % entry_len != 0 {
            return Err(AcpiError::MisalignedRootEntries {
                payload_len,
                entry_len,
            });
        }
        Ok(Self { table, kind })
    }

    pub const fn kind(self) -> RootTableKind {
        self.kind
    }

    pub fn entries(self) -> RootEntries<'a> {
        RootEntries {
            remaining: &self.table.bytes()[SDT_HEADER_LEN..],
            entry_len: self.kind.entry_len(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RootEntries<'a> {
    remaining: &'a [u8],
    entry_len: usize,
}

impl Iterator for RootEntries<'_> {
    type Item = u64;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let address = if self.entry_len == 4 {
            read_u32(self.remaining, 0) as u64
        } else {
            read_u64(self.remaining, 0)
        };
        self.remaining = &self.remaining[self.entry_len..];
        Some(address)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.remaining.len() / self.entry_len;
        (len, Some(len))
    }
}

impl ExactSizeIterator for RootEntries<'_> {}

fn require_len(bytes: &[u8], needed: usize) -> Result<(), AcpiError> {
    if bytes.len() < needed {
        Err(AcpiError::Truncated {
            needed,
            available: bytes.len(),
        })
    } else {
        Ok(())
    }
}

fn checksum_is_zero(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) == 0
}

fn read_array_4(bytes: &[u8], offset: usize) -> [u8; 4] {
    [
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]
}

fn read_array_8(bytes: &[u8], offset: usize) -> [u8; 8] {
    [
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ]
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(read_array_4(bytes, offset))
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(read_array_8(bytes, offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_checksum(bytes: &mut [u8], checksum_offset: usize) {
        bytes[checksum_offset] = 0;
        let sum = bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        bytes[checksum_offset] = 0u8.wrapping_sub(sum);
    }

    fn rsdp_v2() -> [u8; RSDP_V2_MIN_LEN] {
        let mut bytes = [0u8; RSDP_V2_MIN_LEN];
        bytes[..8].copy_from_slice(&RSDP_SIGNATURE);
        bytes[9..15].copy_from_slice(b"KUMO  ");
        bytes[15] = 2;
        bytes[16..20].copy_from_slice(&0x1122_3344u32.to_le_bytes());
        bytes[20..24].copy_from_slice(&(RSDP_V2_MIN_LEN as u32).to_le_bytes());
        bytes[24..32].copy_from_slice(&0x1234_5678_9abc_def0u64.to_le_bytes());
        set_checksum(&mut bytes[..RSDP_V1_LEN], 8);
        set_checksum(&mut bytes, 32);
        bytes
    }

    fn root_table<const N: usize>(signature: [u8; 4], entries: &[u8]) -> [u8; N] {
        assert_eq!(N, SDT_HEADER_LEN + entries.len());
        let mut bytes = [0u8; N];
        bytes[..4].copy_from_slice(&signature);
        bytes[4..8].copy_from_slice(&(N as u32).to_le_bytes());
        bytes[8] = 1;
        bytes[10..16].copy_from_slice(b"KUMO  ");
        bytes[SDT_HEADER_LEN..].copy_from_slice(entries);
        set_checksum(&mut bytes, 9);
        bytes
    }

    #[test]
    fn acpi_two_prefers_the_checksum_valid_xsdt() {
        let bytes = rsdp_v2();
        let rsdp = Rsdp::parse(&bytes).unwrap();
        assert_eq!(rsdp.revision, 2);
        assert_eq!(rsdp.rsdt_address, 0x1122_3344);
        assert_eq!(
            rsdp.root_address(),
            RootAddress {
                kind: RootTableKind::Xsdt,
                physical_address: 0x1234_5678_9abc_def0,
            }
        );
    }

    #[test]
    fn acpi_one_uses_the_legacy_rsdt() {
        let mut bytes = [0u8; RSDP_V1_LEN];
        bytes[..8].copy_from_slice(&RSDP_SIGNATURE);
        bytes[16..20].copy_from_slice(&0xfeed_c000u32.to_le_bytes());
        set_checksum(&mut bytes, 8);

        let rsdp = Rsdp::parse(&bytes).unwrap();
        assert_eq!(rsdp.length, RSDP_V1_LEN);
        assert_eq!(rsdp.xsdt_address, None);
        assert_eq!(
            rsdp.root_address(),
            RootAddress {
                kind: RootTableKind::Rsdt,
                physical_address: 0xfeed_c000,
            }
        );
    }

    #[test]
    fn rsdp_rejects_corruption_in_each_checksum_domain() {
        let mut legacy = rsdp_v2();
        legacy[9] ^= 1;
        assert_eq!(
            Rsdp::parse(&legacy),
            Err(AcpiError::InvalidChecksum(ChecksumKind::RsdpLegacy))
        );

        let mut extended = rsdp_v2();
        extended[33] = 1;
        assert_eq!(
            Rsdp::parse(&extended),
            Err(AcpiError::InvalidChecksum(ChecksumKind::RsdpExtended))
        );
    }

    #[test]
    fn xsdt_enumerates_full_width_physical_addresses() {
        let mut entries = [0u8; 16];
        entries[..8].copy_from_slice(&0x0000_0000_1234_5000u64.to_le_bytes());
        entries[8..].copy_from_slice(&0x1234_5678_9abc_d000u64.to_le_bytes());
        let bytes = root_table::<{ SDT_HEADER_LEN + 16 }>(*b"XSDT", &entries);

        let root = RootTable::parse(&bytes, RootTableKind::Xsdt).unwrap();
        let mut addresses = root.entries();
        assert_eq!(addresses.len(), 2);
        assert_eq!(addresses.next(), Some(0x0000_0000_1234_5000));
        assert_eq!(addresses.next(), Some(0x1234_5678_9abc_d000));
        assert_eq!(addresses.next(), None);
    }

    #[test]
    fn rsdt_zero_extends_its_physical_addresses() {
        let mut entries = [0u8; 8];
        entries[..4].copy_from_slice(&0x1234_5000u32.to_le_bytes());
        entries[4..].copy_from_slice(&0xfedc_b000u32.to_le_bytes());
        let bytes = root_table::<{ SDT_HEADER_LEN + 8 }>(*b"RSDT", &entries);

        let root = RootTable::parse(&bytes, RootTableKind::Rsdt).unwrap();
        let mut addresses = root.entries();
        assert_eq!(addresses.next(), Some(0x1234_5000));
        assert_eq!(addresses.next(), Some(0xfedc_b000));
        assert_eq!(addresses.next(), None);
    }

    #[test]
    fn root_table_rejects_wrong_signature_checksum_and_entry_shape() {
        let wrong_signature = root_table::<SDT_HEADER_LEN>(*b"APIC", &[]);
        assert_eq!(
            RootTable::parse(&wrong_signature, RootTableKind::Xsdt),
            Err(AcpiError::InvalidSdtSignature {
                expected: *b"XSDT",
                found: *b"APIC",
            })
        );

        let mut corrupt = root_table::<SDT_HEADER_LEN>(*b"XSDT", &[]);
        corrupt[10] ^= 1;
        assert_eq!(
            RootTable::parse(&corrupt, RootTableKind::Xsdt),
            Err(AcpiError::InvalidChecksum(
                ChecksumKind::SystemDescriptionTable
            ))
        );

        let malformed = root_table::<{ SDT_HEADER_LEN + 4 }>(*b"XSDT", &[0; 4]);
        assert_eq!(
            RootTable::parse(&malformed, RootTableKind::Xsdt),
            Err(AcpiError::MisalignedRootEntries {
                payload_len: 4,
                entry_len: 8,
            })
        );
    }

    #[test]
    fn declared_lengths_are_bounded_before_field_access() {
        let mut rsdp = rsdp_v2();
        rsdp[20..24].copy_from_slice(&35u32.to_le_bytes());
        set_checksum(&mut rsdp[..RSDP_V1_LEN], 8);
        assert_eq!(
            Rsdp::parse(&rsdp),
            Err(AcpiError::InvalidLength {
                length: 35,
                minimum: RSDP_V2_MIN_LEN,
            })
        );

        let mut sdt = root_table::<SDT_HEADER_LEN>(*b"XSDT", &[]);
        sdt[4..8].copy_from_slice(&64u32.to_le_bytes());
        assert_eq!(
            DescriptionTable::parse(&sdt),
            Err(AcpiError::Truncated {
                needed: 64,
                available: SDT_HEADER_LEN,
            })
        );
    }

    #[test]
    fn rsdp_search_uses_absolute_physical_alignment() {
        let candidate = rsdp_v2();
        let mut window = [0u8; 64];
        window[5..5 + RSDP_V2_MIN_LEN].copy_from_slice(&candidate);
        assert_eq!(find_rsdp(&window, 0xe0003), None);

        window.fill(0);
        window[13..13 + RSDP_V2_MIN_LEN].copy_from_slice(&candidate);
        let found = find_rsdp(&window, 0xe0003).unwrap();
        assert_eq!(found.physical_address, 0xe0010);
        assert_eq!(found.rsdp, Rsdp::parse(&candidate).unwrap());
    }

    #[test]
    fn rsdp_search_skips_a_corrupt_aligned_candidate() {
        let mut window = [0u8; 112];
        let mut corrupt = rsdp_v2();
        corrupt[8] ^= 1;
        window[13..13 + RSDP_V2_MIN_LEN].copy_from_slice(&corrupt);
        let valid = rsdp_v2();
        window[61..61 + RSDP_V2_MIN_LEN].copy_from_slice(&valid);

        let found = find_rsdp(&window, 0xe0003).unwrap();
        assert_eq!(found.physical_address, 0xe0040);
    }
}
