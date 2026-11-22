//j805
//j933
//! NV8a policy: one reserved512B sector in explicitly disposable test media.
//! The 16-sector extent is outside J804's256MiB FAT volume. No filesystem write
//! service is enabled by this proof. D5 cooperative DMA, RoadC open. — TIMBERDOODLE
use crate::{
    admin::{Controller, Namespace},
    Error,
};
pub const RESERVED_START: u64 = 1_048_576;
pub const RESERVED_BLOCKS: u64 = 16;
pub const TEST_LBA: u64 = RESERVED_START + 8;
pub fn pattern() -> [u8; 512] {
    let mut b = [0; 512];
    for (i, x) in b.iter_mut().enumerate() {
        *x = (i as u8).wrapping_mul(37) ^ 0xa5;
    }
    let tag = b"KUMO NV8 J805 Write+Flush persistent sector v3\r\n";
    b[..tag.len()].copy_from_slice(tag);
    b
}
/// The exact SSD (J804/J805's identity gate).
fn exact_ssd(c: &Controller, n: &Namespace) -> bool {
    fn text(b: &[u8]) -> &str {
        core::str::from_utf8(b).unwrap_or("").trim_end_matches(' ')
    }
    c.vid == 0x144d
        && c.ssvid == 0x144d
        && text(&c.model) == "Samsung SSD 970 EVO Plus 500GB"
        && text(&c.serial) == "S58SNM0R302625Z"
        && text(&c.firmware) == "2B2QEXM7"
        && c.namespaces == 1
        && n.id == 1
        && n.size == 976773168
        && n.capacity == 976773168
        && n.lba_bytes == 512
        && n.plain_blocks()
}

/// j933 (SH8-g): write admission for J804 + the RedoxFS root, exactly (`kumo_gpt::sh8g`): this
/// SSD, a CRC-valid GPT of exactly two entries, J804's FAT partition at its LBAs, and the root
/// partition at its LBAs with its type and unique GUID. Admits writes on both volumes, each
/// bounded to its own partition by `volume_request`. Every other layout stays read-only.
pub struct RootLayout(());
impl RootLayout {
    pub fn new(
        c: &Controller,
        n: &Namespace,
        entries: usize,
        fat: &kumo_gpt::Partition,
        root: &kumo_gpt::Partition,
    ) -> Result<Self, Error> {
        use kumo_gpt::sh8g::{FAT_BLOCKS, FAT_FIRST, ROOT_BLOCKS, ROOT_FIRST, ROOT_PART_GUID};
        if !exact_ssd(c, n)
            || n.size != kumo_gpt::sh8g::NAMESPACE_BLOCKS
            || entries != 2
            || fat.first_lba != FAT_FIRST
            || fat.block_count() != FAT_BLOCKS
            || fat.type_guid != kumo_gpt::MICROSOFT_BASIC_DATA_TYPE_GUID
            || root.first_lba != ROOT_FIRST
            || root.block_count() != ROOT_BLOCKS
            || root.type_guid != kumo_gpt::KUMO_REDOXFS_TYPE_GUID
            || root.unique_guid != ROOT_PART_GUID
            || ROOT_FIRST <= RESERVED_START + RESERVED_BLOCKS
        {
            return Err(Error::Identity);
        }
        Ok(Self(()))
    }
}

pub struct TestExtent(());
impl TestExtent {
    /// Requires successfully CRC-validated GPT discovery and exact single partition.
    pub fn new(
        c: &Controller,
        n: &Namespace,
        entries: usize,
        first: u64,
        blocks: u64,
    ) -> Result<Self, Error> {
        fn text(b: &[u8]) -> &str {
            core::str::from_utf8(b).unwrap_or("").trim_end_matches(' ')
        }
        if c.vid != 0x144d
            || c.ssvid != 0x144d
            || text(&c.model) != "Samsung SSD 970 EVO Plus 500GB"
            || text(&c.serial) != "S58SNM0R302625Z"
            || text(&c.firmware) != "2B2QEXM7"
            || c.namespaces != 1
            || n.id != 1
            || n.size != 976773168
            || n.capacity != 976773168
            || n.lba_bytes != 512
            || !n.plain_blocks()
            || entries != 1
            || first != 2048
            || blocks != 524288
            || first + blocks > RESERVED_START
            || RESERVED_START + RESERVED_BLOCKS >= n.size - 33
        {
            return Err(Error::Identity);
        }
        Ok(Self(()))
    }
    pub fn authorize(&self, lba: u64, bytes: usize) -> Result<(), Error> {
        if lba != TEST_LBA || bytes != 512 {
            Err(Error::Range)
        } else {
            Ok(())
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn ssd_pair() -> (Controller, Namespace) {
        let mut c = Controller {
            vid: 0x144d,
            ssvid: 0x144d,
            serial: [b' '; 20],
            model: [b' '; 40],
            firmware: *b"2B2QEXM7",
            mdts: 9,
            version: 0x10300,
            namespaces: 1,
            vwc: true,
        };
        c.serial[..15].copy_from_slice(b"S58SNM0R302625Z");
        c.model[..30].copy_from_slice(b"Samsung SSD 970 EVO Plus 500GB");
        let n = Namespace {
            id: 1,
            size: 976773168,
            capacity: 976773168,
            used: 0,
            lba_bytes: 512,
            bytes: 500107862016,
            metadata: 0,
            protection: 0,
            format: 0,
        };
        (c, n)
    }

    #[test]
    fn the_root_layout_admits_exactly_j804_plus_the_sh8g_partition() {
        use kumo_gpt::sh8g::*;
        let (c, n) = ssd_pair();
        let part = |index, type_guid: kumo_gpt::Guid, unique: kumo_gpt::Guid, first, blocks| {
            kumo_gpt::Partition {
                index,
                type_guid,
                unique_guid: unique,
                first_lba: first,
                last_lba: first + blocks - 1,
                attributes: 0,
            }
        };
        let fat = part(
            0,
            kumo_gpt::MICROSOFT_BASIC_DATA_TYPE_GUID,
            kumo_gpt::Guid([1; 16]),
            FAT_FIRST,
            FAT_BLOCKS,
        );
        let root = part(
            1,
            kumo_gpt::KUMO_REDOXFS_TYPE_GUID,
            ROOT_PART_GUID,
            ROOT_FIRST,
            ROOT_BLOCKS,
        );
        assert!(RootLayout::new(&c, &n, 2, &fat, &root).is_ok());
        assert!(
            RootLayout::new(&c, &n, 3, &fat, &root).is_err(),
            "a third entry"
        );
        let shifted = kumo_gpt::Partition {
            first_lba: ROOT_FIRST - 2048,
            ..root
        };
        assert!(RootLayout::new(&c, &n, 2, &fat, &shifted).is_err());
        let foreign = kumo_gpt::Partition {
            unique_guid: kumo_gpt::Guid([9; 16]),
            ..root
        };
        assert!(RootLayout::new(&c, &n, 2, &fat, &foreign).is_err());
        let linux = kumo_gpt::Partition {
            type_guid: kumo_gpt::Guid([7; 16]),
            ..root
        };
        assert!(RootLayout::new(&c, &n, 2, &fat, &linux).is_err());
        let mut other = c;
        other.serial[0] = b'X';
        assert!(
            RootLayout::new(&other, &n, 2, &fat, &root).is_err(),
            "another SSD"
        );
        assert!(ROOT_FIRST > TEST_LBA, "the NV8 sector is outside the root");
    }

    #[test]
    fn reserved_sector_gate_is_exact_and_never_in_fat_or_gpt() {
        let mut c = Controller {
            vid: 0x144d,
            ssvid: 0x144d,
            serial: [b' '; 20],
            model: [b' '; 40],
            firmware: *b"2B2QEXM7",
            mdts: 9,
            version: 0x10300,
            namespaces: 1,
            vwc: true,
        };
        c.serial[..15].copy_from_slice(b"S58SNM0R302625Z");
        c.model[..30].copy_from_slice(b"Samsung SSD 970 EVO Plus 500GB");
        let mut n = Namespace {
            id: 1,
            size: 976773168,
            capacity: 976773168,
            used: 0,
            lba_bytes: 512,
            bytes: 500107862016,
            metadata: 0,
            protection: 0,
            format: 0,
        };
        let gate = TestExtent::new(&c, &n, 1, 2048, 524288).unwrap();
        assert!(gate.authorize(TEST_LBA, 512).is_ok());
        for (l, b) in [
            (TEST_LBA - 1, 512),
            (TEST_LBA + 1, 512),
            (0, 512),
            (1, 512),
            (2048, 512),
            (TEST_LBA, 0),
            (TEST_LBA, 1024),
            (u64::MAX, 512),
        ] {
            assert_eq!(gate.authorize(l, b), Err(Error::Range));
        }
        for (entries, first, blocks) in [
            (0, 2048, 524288),
            (2, 2048, 524288),
            (1, 2049, 524288),
            (1, 2048, 976771072),
        ] {
            assert!(TestExtent::new(&c, &n, entries, first, blocks).is_err());
        }
        c.serial[0] = b'X';
        assert!(TestExtent::new(&c, &n, 1, 2048, 524288).is_err());
        c.serial[0] = b'S';
        n.metadata = 8;
        assert!(TestExtent::new(&c, &n, 1, 2048, 524288).is_err());
        let p = pattern();
        assert_eq!(&p[..8], b"KUMO NV8");
        assert_ne!(p, [0; 512]);
    }
}
