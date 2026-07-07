//j426

use crate::XhciProbeConfig;

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;
const MAX_DEPTH: usize = 32;

pub const X13S_USB0_XHCI_MMIO_BASE: u64 = 0x0a60_0000;
pub const X13S_USB0_XHCI_MMIO_MIN_LEN: u64 = 0x800;
pub const X13S_USB0_XHCI_FIRST_LIGHT_MMIO_LEN: u64 = 0x1000;
pub const X13S_USB0_XHCI_STREAM_ID: u32 = 0x820;

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
pub struct XhciControllerTopology {
    pub mmio_base: u64,
    pub mmio_length: u64,
    pub interrupt: GicInterrupt,
    pub stream_id: u32,
}

impl XhciControllerTopology {
    pub fn probe_config(self) -> Option<XhciProbeConfig> {
        let irq = self.interrupt.global_id()?;
        Some(XhciProbeConfig::new(
            self.mmio_base,
            X13S_USB0_XHCI_FIRST_LIGHT_MMIO_LEN,
            irq,
            self.stream_id,
        ))
    }
}

#[derive(Clone, Copy, Default)]
struct Node {
    is_usb0_node: bool,
    compatible_dwc3: bool,
    reg: [u32; 4],
    reg_len: u8,
    interrupts: [u32; 3],
    interrupts_len: u8,
    iommus: [u32; 3],
    iommus_len: u8,
}

#[derive(Clone, Copy, Default)]
struct ParseResult {
    root_sc8280xp: bool,
    usb0: Option<XhciControllerTopology>,
}

pub fn discover_x13s_usb0_xhci(dtb: &[u8]) -> Option<XhciControllerTopology> {
    let parsed = parse(dtb)?;
    if parsed.root_sc8280xp {
        parsed.usb0
    } else {
        None
    }
}

fn parse(dtb: &[u8]) -> Option<ParseResult> {
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
    let mut parsed = ParseResult::default();
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
                    is_usb0_node: name == b"usb@a600000",
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
                if node.is_usb0_node && node.compatible_dwc3 {
                    if let Some(topology) = xhci_topology(&node) {
                        parsed.usb0 = Some(topology);
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
                if depth == 1 && name == b"compatible" {
                    parsed.root_sc8280xp = string_list_contains(value, b"qcom,sc8280xp");
                }
                apply_property(&mut stack[depth - 1], name, value);
            }
            FDT_NOP => {}
            FDT_END => break,
            _ => return None,
        }
    }
    Some(parsed)
}

fn xhci_topology(node: &Node) -> Option<XhciControllerTopology> {
    if node.reg_len < 4 || node.interrupts_len < 3 || node.iommus_len < 3 {
        return None;
    }
    let mmio_base = cells64(node.reg[0], node.reg[1]);
    let mmio_length = cells64(node.reg[2], node.reg[3]);
    // The Linux-derived plan and the staged X13s DTB disagree on the full dwc3 child range
    // (`0xd950` vs `0xcd00`). This slice reads only xHCI capability/operational/PORTSC registers,
    // so trust the DTB's range if it covers that first-light window. - KESTREL
    if mmio_base != X13S_USB0_XHCI_MMIO_BASE || mmio_length < X13S_USB0_XHCI_FIRST_LIGHT_MMIO_LEN {
        return None;
    }
    let stream_id = node.iommus[1];
    if stream_id != X13S_USB0_XHCI_STREAM_ID {
        return None;
    }
    Some(XhciControllerTopology {
        mmio_base,
        mmio_length,
        interrupt: GicInterrupt {
            kind: node.interrupts[0],
            number: node.interrupts[1],
            flags: node.interrupts[2],
        },
        stream_id,
    })
}

fn apply_property(node: &mut Node, name: &[u8], value: &[u8]) {
    match name {
        b"compatible" => node.compatible_dwc3 = string_list_contains(value, b"snps,dwc3"),
        b"reg" => node.reg_len = copy_cells(value, &mut node.reg),
        b"interrupts" => node.interrupts_len = copy_cells(value, &mut node.interrupts),
        b"iommus" => node.iommus_len = copy_cells(value, &mut node.iommus),
        _ => {}
    }
}

fn copy_cells(value: &[u8], out: &mut [u32]) -> u8 {
    let count = (value.len() / 4).min(out.len());
    for (index, slot) in out.iter_mut().take(count).enumerate() {
        *slot = be32(value, index * 4).unwrap_or(0);
    }
    count as u8
}

fn property_name(strings: &[u8], offset: usize) -> Option<&[u8]> {
    let end = strings.get(offset..)?.iter().position(|byte| *byte == 0)? + offset;
    strings.get(offset..end)
}

fn string_list_contains(list: &[u8], needle: &[u8]) -> bool {
    list.split(|byte| *byte == 0).any(|entry| entry == needle)
}

const fn cells64(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

fn be32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    Some(u32::from_be_bytes(bytes.get(offset..end)?.try_into().ok()?))
}

fn align4(value: usize) -> Option<usize> {
    value.checked_add(3).map(|value| value & !3)
}

#[cfg(test)]
mod tests {
    use super::*;

    const X13S_DTB: &[u8] = include_bytes!("../../../sc8280xp-lenovo-thinkpad-x13s.dtb");

    #[test]
    fn discovers_the_x13s_primary_usb_c_xhci_controller() {
        let topology = discover_x13s_usb0_xhci(X13S_DTB).expect("usb_0 xhci");
        assert_eq!(topology.mmio_base, X13S_USB0_XHCI_MMIO_BASE);
        assert_eq!(topology.mmio_length, 0xcd00);
        assert!(topology.mmio_length >= X13S_USB0_XHCI_MMIO_MIN_LEN);
        assert!(topology.mmio_length >= X13S_USB0_XHCI_FIRST_LIGHT_MMIO_LEN);
        assert_eq!(
            topology.interrupt,
            GicInterrupt {
                kind: 0,
                number: 803,
                flags: 4
            }
        );
        assert_eq!(topology.interrupt.global_id(), Some(835));
        assert_eq!(topology.stream_id, X13S_USB0_XHCI_STREAM_ID);
        assert_eq!(
            topology.probe_config(),
            Some(XhciProbeConfig::new(
                X13S_USB0_XHCI_MMIO_BASE,
                X13S_USB0_XHCI_FIRST_LIGHT_MMIO_LEN,
                835,
                X13S_USB0_XHCI_STREAM_ID
            ))
        );
    }

    #[test]
    fn rejects_non_dtb_or_truncated_inputs() {
        assert_eq!(discover_x13s_usb0_xhci(b"not a dtb"), None);
        assert_eq!(discover_x13s_usb0_xhci(&X13S_DTB[..32]), None);
    }
}
