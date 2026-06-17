use crate::object::KernelObject;
use kumo_abi::KoId;
use kumo_abi::ObjectKind;

#[derive(Clone, Debug)]
pub struct Interrupt {
    koid: KoId,
    irq: u32,
}

impl Interrupt {
    pub fn new(object: KernelObject, irq: u32) -> Self {
        debug_assert_eq!(object.kind(), ObjectKind::Interrupt);
        Self {
            koid: object.koid(),
            irq,
        }
    }

    pub fn koid(&self) -> KoId {
        self.koid
    }

    pub fn irq(&self) -> u32 {
        self.irq
    }
}
