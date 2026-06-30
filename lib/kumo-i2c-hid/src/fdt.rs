const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;
const MAX_DEPTH: usize = 32;
pub const MAX_I2C_HID_DEVICES: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GicInterrupt {
    pub kind: u32,
    pub number: u32,
    pub flags: u32,
}

impl GicInterrupt {
    pub const fn global_id(self) -> Option<u32> {
        match self.kind {
            0 => Some(self.number + 32),
            1 => Some(self.number + 16),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpioInterrupt {
    pub controller_phandle: u32,
    pub pin: u32,
    pub flags: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HidDeviceKind {
    Keyboard,
    Touchpad,
    Unknown,
}

impl HidDeviceKind {
    const fn from_node_name(name: &[u8]) -> Self {
        if starts_with(name, b"keyboard@") {
            Self::Keyboard
        } else if starts_with(name, b"touchpad@") {
            Self::Touchpad
        } else {
            Self::Unknown
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HidDeviceTopology {
    pub kind: HidDeviceKind,
    pub i2c_address: u8,
    pub hid_descriptor_register: u16,
    pub interrupt: GpioInterrupt,
}

impl HidDeviceTopology {
    const EMPTY: Self = Self {
        kind: HidDeviceKind::Unknown,
        i2c_address: 0,
        hid_descriptor_register: 0,
        interrupt: GpioInterrupt {
            controller_phandle: 0,
            pin: 0,
            flags: 0,
        },
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct I2cHidBusTopology {
    pub controller_mmio_base: u64,
    pub controller_mmio_length: u64,
    pub controller_interrupt: GicInterrupt,
    pub bus_frequency_hz: u32,
    devices: [HidDeviceTopology; MAX_I2C_HID_DEVICES],
    device_count: usize,
}

impl I2cHidBusTopology {
    pub fn devices(&self) -> &[HidDeviceTopology] {
        &self.devices[..self.device_count]
    }

    fn push(&mut self, device: HidDeviceTopology) -> bool {
        if self.device_count == self.devices.len() {
            return false;
        }
        self.devices[self.device_count] = device;
        self.device_count += 1;
        true
    }

    fn same_controller(self, other: Self) -> bool {
        self.controller_mmio_base == other.controller_mmio_base
            && self.controller_mmio_length == other.controller_mmio_length
            && self.controller_interrupt == other.controller_interrupt
            && self.bus_frequency_hz == other.bus_frequency_hz
    }

    pub fn keyboard(self) -> Option<KeyboardTopology> {
        let keyboard = self
            .devices()
            .iter()
            .find(|device| device.kind == HidDeviceKind::Keyboard)?;
        Some(KeyboardTopology {
            controller_mmio_base: self.controller_mmio_base,
            controller_mmio_length: self.controller_mmio_length,
            controller_interrupt: self.controller_interrupt,
            bus_frequency_hz: self.bus_frequency_hz,
            i2c_address: keyboard.i2c_address,
            hid_descriptor_register: keyboard.hid_descriptor_register,
            keyboard_interrupt: keyboard.interrupt,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyboardTopology {
    pub controller_mmio_base: u64,
    pub controller_mmio_length: u64,
    pub controller_interrupt: GicInterrupt,
    pub bus_frequency_hz: u32,
    pub i2c_address: u8,
    pub hid_descriptor_register: u16,
    pub keyboard_interrupt: GpioInterrupt,
}

#[derive(Clone, Copy, Default)]
struct Node {
    device_kind: Option<HidDeviceKind>,
    compatible_geni_i2c: bool,
    compatible_hid_i2c: bool,
    reg: [u32; 4],
    reg_len: u8,
    interrupts: [u32; 3],
    interrupts_len: u8,
    interrupts_extended: [u32; 3],
    interrupts_extended_len: u8,
    clock_frequency: u32,
    has_clock_frequency: bool,
    hid_descriptor_register: u32,
    has_hid_descriptor_register: bool,
}

pub fn discover_i2c_hid_bus(dtb: &[u8]) -> Option<I2cHidBusTopology> {
    parse(dtb).and_then(|topology| {
        if topology.device_count == 0 {
            None
        } else {
            Some(topology)
        }
    })
}

pub fn discover_keyboard(dtb: &[u8]) -> Option<KeyboardTopology> {
    discover_i2c_hid_bus(dtb)?.keyboard()
}

fn parse(dtb: &[u8]) -> Option<I2cHidBusTopology> {
    if be32(dtb, 0)? != FDT_MAGIC {
        return None;
    }
    let total = be32(dtb, 4)? as usize;
    let dtb = dtb.get(..total)?;
    let struct_off = be32(dtb, 8)? as usize;
    let strings_off = be32(dtb, 12)? as usize;
    let struct_len = be32(dtb, 36)? as usize;
    let strings_len = be32(dtb, 32)? as usize;
    let structures = dtb.get(struct_off..struct_off.checked_add(struct_len)?)?;
    let strings = dtb.get(strings_off..strings_off.checked_add(strings_len)?)?;
    let mut stack = [Node::default(); MAX_DEPTH];
    let mut depth = 0usize;
    let mut cursor = 0usize;
    let mut topology: Option<I2cHidBusTopology> = None;
    while cursor < structures.len() {
        let token = be32(structures, cursor)?;
        cursor += 4;
        match token {
            FDT_BEGIN_NODE => {
                if depth == MAX_DEPTH {
                    return None;
                }
                let end = structures
                    .get(cursor..)?
                    .iter()
                    .position(|byte| *byte == 0)?
                    + cursor;
                let name = structures.get(cursor..end)?;
                stack[depth] = Node {
                    device_kind: Some(HidDeviceKind::from_node_name(name)),
                    ..Node::default()
                };
                depth += 1;
                cursor = align4(end + 1)?;
            }
            FDT_END_NODE => {
                if depth == 0 {
                    return None;
                }
                let node = stack[depth - 1];
                if node.compatible_hid_i2c && depth >= 2 {
                    let mut controller = bus_topology(&stack[depth - 2])?;
                    let device = hid_device_topology(&node)?;
                    if let Some(topology) = topology.as_mut() {
                        if topology.same_controller(controller) {
                            let _ = topology.push(device);
                        }
                    } else {
                        let _ = controller.push(device);
                        topology = Some(controller);
                    }
                }
                depth -= 1;
            }
            FDT_PROP => {
                if depth == 0 {
                    return None;
                }
                let len = be32(structures, cursor)? as usize;
                let name_off = be32(structures, cursor + 4)? as usize;
                cursor += 8;
                let value = structures.get(cursor..cursor.checked_add(len)?)?;
                cursor = align4(cursor.checked_add(len)?)?;
                let name = property_name(strings, name_off)?;
                apply_property(&mut stack[depth - 1], name, value);
            }
            FDT_NOP => {}
            FDT_END => break,
            _ => return None,
        }
    }
    topology
}

fn bus_topology(controller: &Node) -> Option<I2cHidBusTopology> {
    if !controller.compatible_geni_i2c
        || controller.reg_len < 4
        || controller.interrupts_len < 3
        || !controller.has_clock_frequency
    {
        return None;
    }
    Some(I2cHidBusTopology {
        controller_mmio_base: cells64(controller.reg[0], controller.reg[1]),
        controller_mmio_length: cells64(controller.reg[2], controller.reg[3]),
        controller_interrupt: GicInterrupt {
            kind: controller.interrupts[0],
            number: controller.interrupts[1],
            flags: controller.interrupts[2],
        },
        bus_frequency_hz: controller.clock_frequency,
        devices: [HidDeviceTopology::EMPTY; MAX_I2C_HID_DEVICES],
        device_count: 0,
    })
}

fn hid_device_topology(device: &Node) -> Option<HidDeviceTopology> {
    if device.reg_len < 1
        || device.interrupts_extended_len < 3
        || !device.has_hid_descriptor_register
        || device.reg[0] > 0x7f
        || device.hid_descriptor_register > u16::MAX as u32
    {
        return None;
    }
    Some(HidDeviceTopology {
        kind: device.device_kind.unwrap_or(HidDeviceKind::Unknown),
        i2c_address: device.reg[0] as u8,
        hid_descriptor_register: device.hid_descriptor_register as u16,
        interrupt: GpioInterrupt {
            controller_phandle: device.interrupts_extended[0],
            pin: device.interrupts_extended[1],
            flags: device.interrupts_extended[2],
        },
    })
}

fn apply_property(node: &mut Node, name: &[u8], value: &[u8]) {
    match name {
        b"compatible" => {
            node.compatible_geni_i2c = string_list_contains(value, b"qcom,geni-i2c");
            node.compatible_hid_i2c = string_list_contains(value, b"hid-over-i2c");
        }
        b"reg" => node.reg_len = copy_cells(value, &mut node.reg),
        b"interrupts" => node.interrupts_len = copy_cells(value, &mut node.interrupts),
        b"interrupts-extended" => {
            node.interrupts_extended_len = copy_cells(value, &mut node.interrupts_extended)
        }
        b"clock-frequency" => {
            if let Some(value) = be32(value, 0) {
                node.clock_frequency = value;
                node.has_clock_frequency = true;
            }
        }
        b"hid-descr-addr" => {
            if let Some(value) = be32(value, 0) {
                node.hid_descriptor_register = value;
                node.has_hid_descriptor_register = true;
            }
        }
        _ => {}
    }
}

fn copy_cells<const N: usize>(value: &[u8], out: &mut [u32; N]) -> u8 {
    let count = (value.len() / 4).min(N);
    for (index, cell) in out.iter_mut().take(count).enumerate() {
        *cell = be32(value, index * 4).unwrap_or(0);
    }
    count as u8
}

fn string_list_contains(mut value: &[u8], expected: &[u8]) -> bool {
    while let Some(end) = value.iter().position(|byte| *byte == 0) {
        if &value[..end] == expected {
            return true;
        }
        value = &value[end + 1..];
    }
    value == expected
}

const fn starts_with(value: &[u8], prefix: &[u8]) -> bool {
    if prefix.len() > value.len() {
        return false;
    }
    let mut index = 0;
    while index < prefix.len() {
        if value[index] != prefix[index] {
            return false;
        }
        index += 1;
    }
    true
}

fn property_name(strings: &[u8], offset: usize) -> Option<&[u8]> {
    let rest = strings.get(offset..)?;
    let end = rest.iter().position(|byte| *byte == 0)?;
    rest.get(..end)
}

const fn cells64(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

fn be32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn align4(value: usize) -> Option<usize> {
    value.checked_add(3).map(|value| value & !3)
}

#[cfg(test)]
mod tests {
    use super::*;

    const X13S_DTB: &[u8] = include_bytes!("../../../sc8280xp-lenovo-thinkpad-x13s.dtb");

    #[test]
    fn discovers_the_real_x13s_i2c21_hid_bus_children() {
        let topology = discover_i2c_hid_bus(X13S_DTB).expect("X13s i2c21 HID bus");
        assert_eq!(topology.controller_mmio_base, 0x0089_4000);
        assert_eq!(topology.controller_mmio_length, 0x4000);
        assert_eq!(topology.controller_interrupt.number, 0x24b);
        assert_eq!(topology.controller_interrupt.global_id(), Some(0x26b));
        assert_eq!(topology.bus_frequency_hz, 400_000);

        let devices = topology.devices();
        assert_eq!(devices.len(), 3);
        assert_eq!(devices[0].kind, HidDeviceKind::Touchpad);
        assert_eq!(devices[0].i2c_address, 0x15);
        assert_eq!(devices[0].hid_descriptor_register, 1);
        assert_eq!(devices[0].interrupt.pin, 182);
        assert_eq!(devices[0].interrupt.flags, 8);

        assert_eq!(devices[1].kind, HidDeviceKind::Touchpad);
        assert_eq!(devices[1].i2c_address, 0x2c);
        assert_eq!(devices[1].hid_descriptor_register, 0x20);
        assert_eq!(devices[1].interrupt.pin, 182);
        assert_eq!(devices[1].interrupt.flags, 8);

        assert_eq!(devices[2].kind, HidDeviceKind::Keyboard);
        assert_eq!(devices[2].i2c_address, 0x68);
        assert_eq!(devices[2].hid_descriptor_register, 1);
        assert_eq!(devices[2].interrupt.pin, 104);
        assert_eq!(devices[2].interrupt.flags, 8);
    }

    #[test]
    fn discovers_the_real_x13s_internal_keyboard_path() {
        let topology = discover_keyboard(X13S_DTB).expect("X13s keyboard node");
        assert_eq!(topology.controller_mmio_base, 0x0089_4000);
        assert_eq!(topology.controller_mmio_length, 0x4000);
        assert_eq!(topology.controller_interrupt.number, 0x24b);
        assert_eq!(topology.controller_interrupt.global_id(), Some(0x26b));
        assert_eq!(topology.bus_frequency_hz, 400_000);
        assert_eq!(topology.i2c_address, 0x68);
        assert_eq!(topology.hid_descriptor_register, 1);
        assert_eq!(topology.keyboard_interrupt.pin, 104);
        assert_eq!(topology.keyboard_interrupt.flags, 8);
    }

    #[test]
    fn rejects_truncation_and_non_fdt_data() {
        assert_eq!(discover_keyboard(&X13S_DTB[..32]), None);
        assert_eq!(discover_keyboard(b"not a device tree"), None);
    }
}
