#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

const PDC_IRQ_ENABLE_BANK: u64 = 0x10;
const PDC_IRQ_CFG: u64 = 0x110;
const PDC_VERSION: u64 = 0x1000;
const PDC_VERSION_3_2: u32 = 0x0003_0200;
const PDC_IRQ_CFG_TYPE_MASK: u32 = 0b111;
const PDC_IRQ_CFG_ENABLE: u32 = 1 << 3;
pub const PDC_TYPE_LEVEL_LOW: u32 = 0b000;
pub const PDC_TYPE_EDGE_FALLING: u32 = 0b010;
pub const PDC_TYPE_LEVEL_HIGH: u32 = 0b100;
pub const PDC_TYPE_EDGE_RISING: u32 = 0b110;
pub const PDC_TYPE_EDGE_DUAL: u32 = 0b111;
const GIC_SPI_INTID_BASE: u32 = 32;
const IRQ_TYPE_EDGE_RISING: u32 = 1;
const IRQ_TYPE_EDGE_FALLING: u32 = 2;
const IRQ_TYPE_EDGE_BOTH: u32 = 3;
const IRQ_TYPE_LEVEL_HIGH: u32 = 4;
const IRQ_TYPE_LEVEL_LOW: u32 = 8;
const MAX_PDC_RANGES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PdcRoute {
    pub port: u32,
    pub gic_intid: u32,
    pub cfg_offset: u64,
    pub cfg_type: u32,
    pub enable_bank_offset: u64,
    pub enable_bit: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PdcRange {
    pub pdc_port_base: u32,
    pub gic_spi_base: u32,
    pub count: u32,
}

#[derive(Clone, Debug)]
pub struct PdcConfig {
    pub base: u64,
    ranges: [Option<PdcRange>; MAX_PDC_RANGES],
}

impl PdcConfig {
    pub fn new() -> Self {
        Self {
            base: 0,
            ranges: [None; MAX_PDC_RANGES],
        }
    }

    pub fn push_range(&mut self, range: PdcRange) {
        for slot in &mut self.ranges {
            if slot.is_none() {
                *slot = Some(range);
                return;
            }
        }
    }

    pub fn route_for_port(&self, port: u32, cfg_type: u32) -> Option<PdcRoute> {
        for slot in &self.ranges {
            if let Some(range) = slot {
                if port >= range.pdc_port_base && port < range.pdc_port_base + range.count {
                    let spi = range.gic_spi_base + (port - range.pdc_port_base);
                    return Some(PdcRoute {
                        port,
                        gic_intid: GIC_SPI_INTID_BASE + spi,
                        cfg_offset: PDC_IRQ_CFG + port as u64 * 4,
                        cfg_type,
                        enable_bank_offset: PDC_IRQ_ENABLE_BANK + (port as u64 >> 5) * 4,
                        enable_bit: 1u32 << (port & 31),
                    });
                }
            }
        }
        None
    }
}

impl Default for PdcConfig {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PdcEnableStyle {
    EnableBank,
    ConfigBit,
}

fn pdc_enable_style(version: u32) -> PdcEnableStyle {
    if version >= PDC_VERSION_3_2 {
        PdcEnableStyle::ConfigBit
    } else {
        PdcEnableStyle::EnableBank
    }
}

pub fn pdc_cfg_value(existing: u32, route: PdcRoute, enabled: bool) -> u32 {
    let enable = if enabled { PDC_IRQ_CFG_ENABLE } else { 0 };
    (existing & !(PDC_IRQ_CFG_TYPE_MASK | PDC_IRQ_CFG_ENABLE)) | route.cfg_type | enable
}

pub fn pdc_enable_bank_value(existing: u32, route: PdcRoute, enabled: bool) -> u32 {
    if enabled {
        existing | route.enable_bit
    } else {
        existing & !route.enable_bit
    }
}

/// Translate Linux/DT IRQ trigger flags to the PDC's IRQ_i_CFG type bits.
///
/// This follows Linux `qcom_pdc_gic_set_type`: the PDC itself records low/falling polarity while the
/// parent GIC line is treated as high/rising after PDC conversion.
pub const fn pdc_type_for_irq_flags(flags: u32) -> Option<u32> {
    match flags {
        IRQ_TYPE_EDGE_RISING => Some(PDC_TYPE_EDGE_RISING),
        IRQ_TYPE_EDGE_FALLING => Some(PDC_TYPE_EDGE_FALLING),
        IRQ_TYPE_EDGE_BOTH => Some(PDC_TYPE_EDGE_DUAL),
        IRQ_TYPE_LEVEL_HIGH => Some(PDC_TYPE_LEVEL_HIGH),
        IRQ_TYPE_LEVEL_LOW => Some(PDC_TYPE_LEVEL_LOW),
        _ => None,
    }
}

/// Applies the enable/disable PDC settings using the provided MMIO callbacks.
/// This abstracts the physical MMIO implementation so `kumo-pdc` can remain `no_std` and decoupled from the HAL MMIO.
pub unsafe fn set_pin_enabled<R, W>(
    base: u64,
    route: PdcRoute,
    enabled: bool,
    read32: R,
    write32: W,
) where
    R: Fn(u64) -> u32,
    W: Fn(u64, u32),
{
    let version = read32(base + PDC_VERSION);
    let cfg = base + route.cfg_offset;
    match pdc_enable_style(version) {
        PdcEnableStyle::ConfigBit => {
            let existing = read32(cfg);
            write32(cfg, pdc_cfg_value(existing, route, enabled));
        }
        PdcEnableStyle::EnableBank => {
            if enabled {
                write32(cfg, route.cfg_type);
            }
            let bank = base + route.enable_bank_offset;
            let existing = read32(bank);
            write32(bank, pdc_enable_bank_value(existing, route, enabled));
        }
    }
}

// ---- DTB Parser -------------------------------------------------------------

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

fn read_be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    if end > bytes.len() {
        return None;
    }
    Some(u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]))
}

fn checked_end(start: usize, len: usize, limit: usize) -> Option<usize> {
    let end = start.checked_add(len)?;
    if end <= limit {
        Some(end)
    } else {
        None
    }
}

fn align4(value: usize) -> Option<usize> {
    value.checked_add(3).map(|value| value & !3)
}

fn nul_terminated_len(bytes: &[u8], start: usize, limit: usize) -> Option<usize> {
    if start >= limit || limit > bytes.len() {
        return None;
    }
    bytes[start..limit].iter().position(|byte| *byte == 0)
}

fn read_string(strings: &[u8], offset: usize) -> Option<&str> {
    if offset >= strings.len() {
        return None;
    }
    let len = strings[offset..].iter().position(|byte| *byte == 0)?;
    core::str::from_utf8(&strings[offset..offset + len]).ok()
}

fn compatible_has(data: &[u8], wanted: &[u8]) -> bool {
    let mut start = 0;
    while start < data.len() {
        let Some(rel_end) = data[start..].iter().position(|byte| *byte == 0) else {
            return false;
        };
        let end = start + rel_end;
        if &data[start..end] == wanted {
            return true;
        }
        start = end + 1;
    }
    false
}

/// Parses the FDT to locate a PDC node and extracts `qcom,pdc-ranges`.
pub fn pdc_from_dtb_bytes(bytes: &[u8]) -> Option<PdcConfig> {
    let magic = read_be_u32(bytes, 0)?;
    if magic != FDT_MAGIC {
        return None;
    }

    let total_size = read_be_u32(bytes, 4)? as usize;
    let off_dt_struct = read_be_u32(bytes, 8)? as usize;
    let off_dt_strings = read_be_u32(bytes, 12)? as usize;
    let size_dt_strings = read_be_u32(bytes, 32)? as usize;
    let size_dt_struct = read_be_u32(bytes, 36)? as usize;
    if total_size > bytes.len() {
        return None;
    }
    let struct_end = checked_end(off_dt_struct, size_dt_struct, total_size)?;
    let strings_end = checked_end(off_dt_strings, size_dt_strings, total_size)?;
    let strings = &bytes[off_dt_strings..strings_end];

    #[derive(Clone, Copy)]
    struct NodeState {
        compatible_pdc: bool,
    }

    let mut stack = [NodeState {
        compatible_pdc: false,
    }; 32];
    let mut depth = 0usize;
    let mut cursor = off_dt_struct;
    let mut pdc_config = None;

    while cursor < struct_end {
        let token = read_be_u32(bytes, cursor)?;
        cursor = cursor.checked_add(4)?;
        match token {
            FDT_BEGIN_NODE => {
                if depth == stack.len() {
                    return None;
                }
                let name_len = nul_terminated_len(bytes, cursor, struct_end)?;
                cursor = align4(cursor.checked_add(name_len)?.checked_add(1)?)?;
                stack[depth] = NodeState {
                    compatible_pdc: false,
                };
                depth += 1;
            }
            FDT_END_NODE => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
            }
            FDT_PROP => {
                if depth == 0 {
                    return None;
                }
                let len = read_be_u32(bytes, cursor)? as usize;
                cursor = cursor.checked_add(4)?;
                let name_offset = read_be_u32(bytes, cursor)? as usize;
                cursor = cursor.checked_add(4)?;
                let data_end = checked_end(cursor, len, struct_end)?;
                let name = read_string(strings, name_offset)?;
                let data = &bytes[cursor..data_end];
                let state = &mut stack[depth - 1];

                if name == "compatible" && compatible_has(data, b"qcom,sc8280xp-pdc") {
                    state.compatible_pdc = true;
                    if pdc_config.is_none() {
                        pdc_config = Some(PdcConfig::new());
                    }
                }

                if state.compatible_pdc && name == "qcom,pdc-ranges" {
                    if let Some(config) = &mut pdc_config {
                        let mut offset = 0;
                        while offset + 12 <= data.len() {
                            let pdc_port_base = read_be_u32(data, offset).unwrap();
                            let gic_spi_base = read_be_u32(data, offset + 4).unwrap();
                            let count = read_be_u32(data, offset + 8).unwrap();
                            config.push_range(PdcRange {
                                pdc_port_base,
                                gic_spi_base,
                                count,
                            });
                            offset += 12;
                        }
                    }
                }

                if state.compatible_pdc && name == "reg" {
                    // Extract PDC base address if present in the `reg` property.
                    if let Some(config) = &mut pdc_config {
                        if data.len() >= 16 {
                            // Assume 2-cell address, 2-cell size for now like gicv3.
                            // PDC base address.
                            let base = ((read_be_u32(data, 0).unwrap_or(0) as u64) << 32)
                                | read_be_u32(data, 4).unwrap_or(0) as u64;
                            config.base = base;
                        } else if data.len() >= 8 {
                            let base = read_be_u32(data, 0).unwrap_or(0) as u64;
                            config.base = base;
                        }
                    }
                }

                cursor = align4(data_end)?;
            }
            FDT_NOP => {}
            FDT_END => break,
            _ => return None,
        }
    }

    pdc_config
}

#[cfg(test)]
mod tests {
    use super::*;

    const X13S_DTB: &[u8] = include_bytes!("../../../sc8280xp-lenovo-thinkpad-x13s.dtb");

    #[test]
    fn pdc_enable_style_switches_at_v3_2() {
        assert_eq!(pdc_enable_style(0x0003_01ff), PdcEnableStyle::EnableBank);
        assert_eq!(pdc_enable_style(0x0003_0200), PdcEnableStyle::ConfigBit);
        assert_eq!(pdc_enable_style(0x0004_0000), PdcEnableStyle::ConfigBit);
    }

    #[test]
    fn pdc_cfg_bit_enable_preserves_non_type_bits() {
        let route = PdcRoute {
            port: 216,
            gic_intid: 678,
            cfg_offset: 0x470,
            cfg_type: PDC_TYPE_LEVEL_HIGH,
            enable_bank_offset: 0x28,
            enable_bit: 1 << 24,
        };
        let existing = 0xa5a5_a5af;
        assert_eq!(
            pdc_cfg_value(existing, route, true),
            (existing & !(PDC_IRQ_CFG_TYPE_MASK | PDC_IRQ_CFG_ENABLE))
                | route.cfg_type
                | PDC_IRQ_CFG_ENABLE
        );
        assert_eq!(
            pdc_cfg_value(existing, route, false),
            (existing & !(PDC_IRQ_CFG_TYPE_MASK | PDC_IRQ_CFG_ENABLE)) | route.cfg_type
        );
    }

    #[test]
    fn pdc_enable_bank_sets_and_clears_only_the_route_bit() {
        let route = PdcRoute {
            port: 216,
            gic_intid: 678,
            cfg_offset: 0x470,
            cfg_type: PDC_TYPE_LEVEL_HIGH,
            enable_bank_offset: 0x28,
            enable_bit: 1 << 24,
        };
        assert_eq!(pdc_enable_bank_value(0, route, true), route.enable_bit);
        assert_eq!(
            pdc_enable_bank_value(u32::MAX, route, false),
            u32::MAX & !route.enable_bit
        );
    }

    #[test]
    fn pdc_irq_type_translation_matches_linux_qcom_pdc() {
        assert_eq!(pdc_type_for_irq_flags(1), Some(PDC_TYPE_EDGE_RISING));
        assert_eq!(pdc_type_for_irq_flags(2), Some(PDC_TYPE_EDGE_FALLING));
        assert_eq!(pdc_type_for_irq_flags(3), Some(PDC_TYPE_EDGE_DUAL));
        assert_eq!(pdc_type_for_irq_flags(4), Some(PDC_TYPE_LEVEL_HIGH));
        assert_eq!(pdc_type_for_irq_flags(8), Some(PDC_TYPE_LEVEL_LOW));
        assert_eq!(pdc_type_for_irq_flags(0), None);
    }

    #[test]
    fn pdc_config_routes_ports() {
        let mut config = PdcConfig::new();
        config.push_range(PdcRange {
            pdc_port_base: 216,
            gic_spi_base: 646,
            count: 5,
        });

        let route = config.route_for_port(216, PDC_TYPE_LEVEL_HIGH).unwrap();
        assert_eq!(route.port, 216);
        assert_eq!(route.gic_intid, 678);
        assert_eq!(route.cfg_offset, 0x470);
        assert_eq!(route.cfg_type, PDC_TYPE_LEVEL_HIGH);
        assert_eq!(route.enable_bank_offset, 0x28);
        assert_eq!(route.enable_bit, 1 << 24);

        assert_eq!(config.route_for_port(215, PDC_TYPE_LEVEL_HIGH), None);
        assert_eq!(config.route_for_port(221, PDC_TYPE_LEVEL_HIGH), None);
    }

    #[test]
    fn pdc_parser_keeps_real_x13s_keyboard_and_touchpad_wake_ranges() {
        let config = pdc_from_dtb_bytes(X13S_DTB).expect("X13s PDC node");
        assert_eq!(config.base, 0x0b22_0000);

        let keyboard = config.route_for_port(216, PDC_TYPE_LEVEL_LOW).unwrap();
        assert_eq!(keyboard.gic_intid, 678);
        assert_eq!(keyboard.cfg_type, PDC_TYPE_LEVEL_LOW);

        let touchpad = config.route_for_port(240, PDC_TYPE_LEVEL_LOW).unwrap();
        assert_eq!(touchpad.gic_intid, 398);
        assert_eq!(touchpad.cfg_type, PDC_TYPE_LEVEL_LOW);
    }
}
