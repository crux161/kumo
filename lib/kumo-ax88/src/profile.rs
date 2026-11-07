//j826
//! Conservative binding profile layered over the unchanged NET0 descriptor inventory.
//! Revision0100 is the first supported recipe; fixtures are not a hardware claim for
//! AX88178A or another host. NET4/NET5 must still prove transport/initialization.
//! — TIMBERDOODLE

use crate::inventory::{self, Endpoint, Family, Inventory};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Speed {
    Full,
    High,
    Super,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Inventory(inventory::Error),
    Unmatched,
    Revision,
    UsbVersion,
    Speed,
    Endpoint,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Profile {
    inventory: Inventory,
    speed: Speed,
    rx: Endpoint,
    tx: Endpoint,
    status: Endpoint,
}

impl Profile {
    pub fn from_descriptors(device: &[u8], config: &[u8], speed: Speed) -> Result<Self, Error> {
        let inventory = inventory::inspect(device, config, speed == Speed::Super)
            .map_err(Error::Inventory)?
            .ok_or(Error::Unmatched)?;
        if inventory.device_revision != 0x0100 {
            return Err(Error::Revision);
        }
        if !matches!(inventory.usb_revision, 0x0200 | 0x0210 | 0x0300) {
            return Err(Error::UsbVersion);
        }
        if speed == Speed::Super
            && (inventory.family != Family::Ax88179 || inventory.usb_revision < 0x0300)
        {
            return Err(Error::Speed);
        }
        if inventory.ep0_encoding != if speed == Speed::Super { 9 } else { 64 } {
            return Err(Error::Endpoint);
        }
        let bulk_size = match speed {
            Speed::Full => 64,
            Speed::High => 512,
            Speed::Super => 1024,
        };
        let mut rx = None;
        let mut tx = None;
        let mut status = None;
        for ep in inventory.endpoints {
            if ep.attributes != ep.attributes & 3 {
                return Err(Error::Endpoint);
            }
            if ep.attributes == 2 {
                if ep.max_packet_size != bulk_size
                    || ep.interval != 0
                    || ep.companion.is_some_and(|c| c.attributes != 0)
                {
                    return Err(Error::Endpoint);
                }
                if ep.address & 0x80 != 0 {
                    rx = Some(ep);
                } else {
                    tx = Some(ep);
                }
            } else {
                if ep.max_packet_size != 8
                    || ep.interval == 0
                    || (speed != Speed::Full && ep.interval > 16)
                    || ep.companion.is_some_and(|c| {
                        c.max_burst != 0 || c.attributes != 0 || c.bytes_per_interval != 8
                    })
                {
                    return Err(Error::Endpoint);
                }
                status = Some(ep);
            }
        }
        Ok(Self {
            inventory,
            speed,
            rx: rx.ok_or(Error::Endpoint)?,
            tx: tx.ok_or(Error::Endpoint)?,
            status: status.ok_or(Error::Endpoint)?,
        })
    }
    pub const fn inventory(&self) -> Inventory {
        self.inventory
    }
    pub const fn speed(&self) -> Speed {
        self.speed
    }
    pub const fn rx(&self) -> Endpoint {
        self.rx
    }
    pub const fn tx(&self) -> Endpoint {
        self.tx
    }
    pub const fn status(&self) -> Endpoint {
        self.status
    }
}

#[cfg(test)]
mod tests;
