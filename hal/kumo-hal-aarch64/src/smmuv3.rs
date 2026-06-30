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
}
