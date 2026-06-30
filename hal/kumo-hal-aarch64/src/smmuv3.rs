const SMMU_IDR0: usize = 0x0;
const SMMU_CR0: usize = 0x20;
const SMMU_STRTAB_BASE: usize = 0x80;
const SMMU_STRTAB_BASE_CFG: usize = 0x88;
const SMMU_CMDQ_BASE: usize = 0x90;
const SMMU_CMDQ_PROD: usize = 0x98;
const SMMU_CMDQ_CONS: usize = 0x9C;
const SMMU_EVENTQ_BASE: usize = 0xA0;
const SMMU_EVENTQ_PROD: usize = 0xA8;
const SMMU_EVENTQ_CONS: usize = 0xAC;

const CR0_SMMUEN: u32 = 1 << 0;
const IOMMU_KIND_SMMUV3: u32 = 2;
const PAGE_SIZE: u64 = 4096;
const DMA_RIGHTS_READ: u32 = 1 << 2;
const DMA_RIGHTS_WRITE: u32 = 1 << 3;
const EVTQ_0_ID_MASK: u64 = 0xff;
const EVTQ_0_SSV: u64 = 1 << 11;
const EVTQ_0_SSID_SHIFT: u64 = 12;
const EVTQ_0_SSID_MASK: u64 = 0x000f_ffff;
const EVTQ_0_SID_SHIFT: u64 = 32;
const EVT_ID_TRANSLATION: u8 = 0x10;
const EVT_ID_ADDR_SIZE: u8 = 0x11;
const EVT_ID_ACCESS: u8 = 0x12;
const EVT_ID_PERMISSION: u8 = 0x13;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SmmuFaultEvent {
    pub event_id: u8,
    pub stream_id: u32,
    pub substream_id: Option<u32>,
    pub fault_record: u64,
    pub fault_addr: u64,
}

pub fn iommu_init(kind: u32, _phys_base: u64, _len: u64) -> bool {
    if kind != IOMMU_KIND_SMMUV3 {
        return false;
    }
    // TODO: Actually program SMMUv3 global registers here
    true
}

pub fn iommu_create_device_context(
    kind: u32,
    _phys_base: u64,
    _stream_id: u32,
    _pgd_phys: u64,
) -> bool {
    if kind != IOMMU_KIND_SMMUV3 {
        return false;
    }
    // TODO: Program STE and CD
    true
}

pub fn iommu_destroy_device_context(_kind: u32, _phys_base: u64, _stream_id: u32) {
    // TODO: Invalidate STE
}

pub fn iommu_map_device_page(
    kind: u32,
    _phys_base: u64,
    _stream_id: u32,
    pgd_phys: u64,
    iova: u64,
    phys: u64,
    rights: u32,
) -> bool {
    if kind != IOMMU_KIND_SMMUV3 {
        return false;
    }
    device_page_request_is_valid(pgd_phys, iova)
        && phys != 0
        && page_aligned(phys)
        && dma_rights_valid(rights)
}

pub fn iommu_unmap_device_range(
    kind: u32,
    _phys_base: u64,
    _stream_id: u32,
    pgd_phys: u64,
    iova: u64,
    len: u64,
) -> bool {
    if kind != IOMMU_KIND_SMMUV3 {
        return false;
    }
    device_page_request_is_valid(pgd_phys, iova) && len != 0 && page_aligned(len)
}

pub fn decode_smmuv3_fault_event(words: [u64; 4]) -> Option<SmmuFaultEvent> {
    let event_id = (words[0] & EVTQ_0_ID_MASK) as u8;
    if !smmuv3_event_is_device_fault(event_id) {
        return None;
    }
    let stream_id = (words[0] >> EVTQ_0_SID_SHIFT) as u32;
    let substream_id = if words[0] & EVTQ_0_SSV != 0 {
        Some(((words[0] >> EVTQ_0_SSID_SHIFT) & EVTQ_0_SSID_MASK) as u32)
    } else {
        None
    };
    Some(SmmuFaultEvent {
        event_id,
        stream_id,
        substream_id,
        fault_record: words[0],
        fault_addr: words[2],
    })
}

const fn page_aligned(value: u64) -> bool {
    value & (PAGE_SIZE - 1) == 0
}

const fn dma_rights_valid(rights: u32) -> bool {
    let allowed = DMA_RIGHTS_READ | DMA_RIGHTS_WRITE;
    rights != 0 && rights & !allowed == 0
}

const fn device_page_request_is_valid(pgd_phys: u64, iova: u64) -> bool {
    pgd_phys != 0 && page_aligned(pgd_phys) && iova != 0 && page_aligned(iova)
}

const fn smmuv3_event_is_device_fault(event_id: u8) -> bool {
    matches!(
        event_id,
        EVT_ID_TRANSLATION | EVT_ID_ADDR_SIZE | EVT_ID_ACCESS | EVT_ID_PERMISSION
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const IOMMU_KIND_VIRTIO: u32 = 1;

    #[test]
    fn smmuv3_init_accepts_smmuv3_kind() {
        assert!(iommu_init(IOMMU_KIND_SMMUV3, 0x1500_0000, 0x20_0000));
    }

    #[test]
    fn smmuv3_init_rejects_non_smmuv3_kind() {
        assert!(!iommu_init(IOMMU_KIND_VIRTIO, 0x1500_0000, 0x20_0000));
    }

    #[test]
    fn device_context_creation_uses_smmuv3_kind() {
        assert!(iommu_create_device_context(
            IOMMU_KIND_SMMUV3,
            0x1500_0000,
            0x42,
            0x8000_0000
        ));
        assert!(!iommu_create_device_context(
            IOMMU_KIND_VIRTIO,
            0x1500_0000,
            0x42,
            0x8000_0000
        ));
    }

    #[test]
    fn device_page_mapping_validates_smmuv3_request_shape() {
        assert!(iommu_map_device_page(
            IOMMU_KIND_SMMUV3,
            0x1500_0000,
            0x42,
            0x8000_0000,
            0x4000_0000,
            0x9000_0000,
            DMA_RIGHTS_READ | DMA_RIGHTS_WRITE,
        ));
        assert!(!iommu_map_device_page(
            IOMMU_KIND_VIRTIO,
            0x1500_0000,
            0x42,
            0x8000_0000,
            0x4000_0000,
            0x9000_0000,
            DMA_RIGHTS_READ,
        ));
        assert!(!iommu_map_device_page(
            IOMMU_KIND_SMMUV3,
            0x1500_0000,
            0x42,
            0,
            0x4000_0000,
            0x9000_0000,
            DMA_RIGHTS_READ,
        ));
        assert!(!iommu_map_device_page(
            IOMMU_KIND_SMMUV3,
            0x1500_0000,
            0x42,
            0x8000_0000,
            0x4000_0123,
            0x9000_0000,
            DMA_RIGHTS_READ,
        ));
        assert!(!iommu_map_device_page(
            IOMMU_KIND_SMMUV3,
            0x1500_0000,
            0x42,
            0x8000_0000,
            0x4000_0000,
            0x9000_0123,
            DMA_RIGHTS_READ,
        ));
        assert!(!iommu_map_device_page(
            IOMMU_KIND_SMMUV3,
            0x1500_0000,
            0x42,
            0x8000_0000,
            0x4000_0000,
            0x9000_0000,
            0,
        ));
    }

    #[test]
    fn device_range_unmap_validates_smmuv3_request_shape() {
        assert!(iommu_unmap_device_range(
            IOMMU_KIND_SMMUV3,
            0x1500_0000,
            0x42,
            0x8000_0000,
            0x4000_0000,
            PAGE_SIZE * 2,
        ));
        assert!(!iommu_unmap_device_range(
            IOMMU_KIND_VIRTIO,
            0x1500_0000,
            0x42,
            0x8000_0000,
            0x4000_0000,
            PAGE_SIZE,
        ));
        assert!(!iommu_unmap_device_range(
            IOMMU_KIND_SMMUV3,
            0x1500_0000,
            0x42,
            0x8000_0000,
            0,
            PAGE_SIZE,
        ));
        assert!(!iommu_unmap_device_range(
            IOMMU_KIND_SMMUV3,
            0x1500_0000,
            0x42,
            0x8000_0000,
            0x4000_0000,
            0,
        ));
        assert!(!iommu_unmap_device_range(
            IOMMU_KIND_SMMUV3,
            0x1500_0000,
            0x42,
            0x8000_0000,
            0x4000_0000,
            PAGE_SIZE + 1,
        ));
    }

    #[test]
    fn decodes_smmuv3_translation_fault_event() {
        let words = [
            (0x42_u64 << EVTQ_0_SID_SHIFT)
                | (0x12345_u64 << EVTQ_0_SSID_SHIFT)
                | EVTQ_0_SSV
                | EVT_ID_TRANSLATION as u64,
            0,
            0xdead_beef_cafe_f000,
            0,
        ];

        assert_eq!(
            decode_smmuv3_fault_event(words),
            Some(SmmuFaultEvent {
                event_id: EVT_ID_TRANSLATION,
                stream_id: 0x42,
                substream_id: Some(0x12345),
                fault_record: words[0],
                fault_addr: 0xdead_beef_cafe_f000,
            })
        );
    }

    #[test]
    fn decodes_smmuv3_permission_fault_without_substream() {
        let words = [
            (0x84_u64 << EVTQ_0_SID_SHIFT) | EVT_ID_PERMISSION as u64,
            0,
            0x4000_1000,
            0,
        ];

        assert_eq!(
            decode_smmuv3_fault_event(words),
            Some(SmmuFaultEvent {
                event_id: EVT_ID_PERMISSION,
                stream_id: 0x84,
                substream_id: None,
                fault_record: words[0],
                fault_addr: 0x4000_1000,
            })
        );
    }

    #[test]
    fn decodes_only_device_address_fault_events() {
        for event_id in [EVT_ID_ADDR_SIZE, EVT_ID_ACCESS] {
            let words = [
                (0x21_u64 << EVTQ_0_SID_SHIFT) | event_id as u64,
                0,
                0x8000_0000,
                0,
            ];
            assert_eq!(
                decode_smmuv3_fault_event(words).map(|event| event.event_id),
                Some(event_id)
            );
        }

        let config_fault = [(0x21_u64 << EVTQ_0_SID_SHIFT) | 0x02, 0, 0x8000_0000, 0];
        assert_eq!(decode_smmuv3_fault_event(config_fault), None);
    }
}
