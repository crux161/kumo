use crate::object::KernelObject;
use kumo_abi::KoId;
use kumo_abi::ObjectKind;

#[derive(Clone, Debug)]
pub struct Resource {
    koid: KoId,
    // Note: For MVP, the Resource object represents root authority (can map any physical MMIO).
    // In the future, this would hold the specific range and kind (MMIO, IOPORT, IRQ).
}

impl Resource {
    pub fn new(object: KernelObject) -> Self {
        debug_assert_eq!(object.kind(), ObjectKind::Resource);
        Self {
            koid: object.koid(),
        }
    }

    pub fn koid(&self) -> KoId {
        self.koid
    }
}
