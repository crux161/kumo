//j621
//j629

//! DESIGN/022 D1d-b2f: identify the single VOP domain from the live DTB.
//! Binding facts: RESEARCH/015 and the pinned Orange Pi DTB. Discovery reads only
//! the blob: no MMIO, reservation, context admission or paging. — KESTREL

use kumo_fdt::{be32, status_enabled, string_list_contains, Fdt, Node};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayIoMmu {
    pub banks: [(u64, u64); 2],
    pub interrupt: u32,
}

impl DisplayIoMmu {
    pub fn matches_range(self, base: u64, len: u64) -> bool {
        base == self.banks[0].0
            && self.banks[0].0.checked_add(self.banks[0].1) == Some(self.banks[1].0)
            && self.banks[0].1.checked_add(self.banks[1].1) == Some(len)
    }
}

fn words(bytes: &[u8], expected: &[u32]) -> bool {
    bytes.len() == expected.len() * 4
        && expected
            .iter()
            .enumerate()
            .all(|(i, &word)| be32(bytes, i * 4) == Some(word))
}

fn property(node: Node<'_>, name: &[u8], expected: &[u32]) -> bool {
    node.property(name).is_some_and(|b| words(b, expected))
}

fn cell(node: Node<'_>, name: &[u8]) -> Option<u32> {
    let bytes = node.property(name)?;
    (bytes.len() == 4).then(|| be32(bytes, 0)).flatten()
}

fn compatible(node: Node<'_>, name: &[u8]) -> bool {
    node.property(b"compatible")
        .is_some_and(|b| b.last() == Some(&0) && string_list_contains(b, name))
}

fn same_node(a: Node<'_>, b: Node<'_>) -> bool {
    a.name().as_ptr() == b.name().as_ptr()
}

fn direct_child(parent: Node<'_>, child: Node<'_>) -> bool {
    parent.children().any(|node| same_node(node, child))
}

// Do not accept the FDT reader's first matching phandle as unique authority.
// Reject duplicate owners, malformed aliases and conflicting legacy aliases. — KESTREL
fn phandle(fdt: Fdt<'_>, value: u32) -> Option<Node<'_>> {
    if value == 0 || value == u32::MAX {
        return None;
    }
    let mut found = None;
    for node in fdt.nodes() {
        let native = node.property(b"phandle");
        let legacy = node.property(b"linux,phandle");
        if ![native, legacy]
            .into_iter()
            .flatten()
            .any(|b| be32(b, 0) == Some(value))
        {
            continue;
        }
        if [native, legacy]
            .into_iter()
            .flatten()
            .any(|b| !words(b, &[value]))
            || found.replace(node).is_some()
        {
            return None;
        }
    }
    found
}

fn root_device(root: Node<'_>, node: Node<'_>, compat: &[u8], reg: &[u32]) -> bool {
    direct_child(root, node)
        && status_enabled(node)
        && compatible(node, compat)
        && property(node, b"reg", reg)
}

fn interrupt_parent(root: Node<'_>, node: Node<'_>) -> Option<u32> {
    if node.property(b"interrupts-extended").is_some() {
        return None;
    }
    match node.property(b"interrupt-parent") {
        Some(_) => cell(node, b"interrupt-parent"),
        None => cell(root, b"interrupt-parent"),
    }
}

/// Recognize only the reviewed RK3588 VOP attachment. The root's two-cell address
/// convention applies only to direct children; never reinterpret a nested bus's
/// untranslated reg cells as physical MMIO. This is topology, not power/IRQ ownership.
/// Actual readiness, containment and shared-source handling remain separate gates. — KESTREL
pub fn discover(bytes: &[u8]) -> Option<DisplayIoMmu> {
    let fdt = Fdt::new(bytes)?;
    let root = fdt.root()?;
    if !status_enabled(root)
        || !compatible(root, b"rockchip,rk3588")
        || !property(root, b"#address-cells", &[2])
        || !property(root, b"#size-cells", &[2])
    {
        return None;
    }
    let mut vops = root
        .children()
        .filter(|&n| status_enabled(n) && compatible(n, b"rockchip,rk3588-vop"));
    let vop = vops.next()?;
    if vops.next().is_some()
        || !property(
            vop,
            b"reg",
            &[0, 0xfdd9_0000, 0, 0x4200, 0, 0xfdd9_5000, 0, 0x1000],
        )
        || vop.property(b"reg-names") != Some(b"vop\0gamma-lut\0".as_slice())
    {
        return None;
    }
    let mmu = phandle(fdt, cell(vop, b"iommus")?)?;
    if !root_device(
        root,
        mmu,
        b"rockchip,rk3588-iommu",
        &[0, 0xfdd9_7e00, 0, 0x100, 0, 0xfdd9_7f00, 0, 0x100],
    ) || !compatible(mmu, b"rockchip,rk3568-iommu")
        || !property(mmu, b"#iommu-cells", &[0])
    {
        return None;
    }

    // Another enabled MMU node must not describe either bank under a different
    // phandle. A duplicate phandle is rejected independently above. — KESTREL
    for other in root.children().filter(|&n| {
        !same_node(n, mmu)
            && status_enabled(n)
            && (compatible(n, b"rockchip,rk3588-iommu") || compatible(n, b"rockchip,rk3568-iommu"))
    }) {
        for span in other.property(b"reg")?.chunks_exact(16) {
            let base = (u64::from(be32(span, 0)?) << 32) | u64::from(be32(span, 4)?);
            let len = (u64::from(be32(span, 8)?) << 32) | u64::from(be32(span, 12)?);
            if len != 0 && base < 0xfdd9_8000 && base.checked_add(len)? > 0xfdd9_7e00 {
                return None;
            }
        }
    }

    let parent = interrupt_parent(root, vop)?;
    if interrupt_parent(root, mmu)? != parent {
        return None;
    }
    let gic = phandle(fdt, parent)?;
    if !root_device(
        root,
        gic,
        b"arm,gic-v3",
        &[0, 0xfe60_0000, 0, 0x10000, 0, 0xfe68_0000, 0, 0x100000],
    ) || gic.property(b"interrupt-controller") != Some(&[])
    {
        return None;
    }
    let irq: &[u32] = match cell(gic, b"#interrupt-cells")? {
        3 => &[0, 156, 4],
        4 => &[0, 156, 4, 0],
        _ => return None,
    };
    if !property(vop, b"interrupts", irq) || !property(mmu, b"interrupts", irq) {
        return None;
    }

    let domains = mmu.property(b"power-domains")?;
    let provider = be32(domains, 0)?;
    let controller = phandle(fdt, provider)?;
    let pmu = phandle(fdt, cell(vop, b"rockchip,pmu")?)?;
    if !words(domains, &[provider, 24])
        || vop.property(b"power-domains") != Some(domains)
        || !status_enabled(controller)
        || !compatible(controller, b"rockchip,rk3588-power-controller")
        || !property(controller, b"#power-domain-cells", &[1])
        || !direct_child(pmu, controller)
        || !root_device(
            root,
            pmu,
            b"rockchip,rk3588-pmu",
            &[0, 0xfd8d_8000, 0, 0x400],
        )
    {
        return None;
    }

    let clocks = mmu.property(b"clocks")?;
    let provider = be32(clocks, 0)?;
    let cru = phandle(fdt, provider)?;
    if !words(clocks, &[provider, 605, provider, 604])
        || mmu.property(b"clock-names") != Some(b"aclk\0iface\0".as_slice())
        || !vop
            .property(b"clocks")
            .is_some_and(|b| b.starts_with(clocks))
        || !vop
            .property(b"clock-names")
            .is_some_and(|b| b.starts_with(b"aclk\0hclk\0"))
        || !property(cru, b"#clock-cells", &[1])
        || !root_device(
            root,
            cru,
            b"rockchip,rk3588-cru",
            &[0, 0xfd7c_0000, 0, 0x5c000],
        )
    {
        return None;
    }
    Some(DisplayIoMmu {
        banks: [(0xfdd9_7e00, 0x100), (0xfdd9_7f00, 0x100)],
        interrupt: 188,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::{ops::Range, vec::Vec};

    const DTB: &[u8] = include_bytes!("../../../rk3588-orangepi-5-plus.dtb");
    const VOP: &[u8] = b"vop@fdd90000";
    const MMU: &[u8] = b"iommu@fdd97e00";
    const GIC: &[u8] = b"interrupt-controller@fe600000";
    const PMU: &[u8] = b"power-management@fd8d8000";
    const CRU: &[u8] = b"clock-controller@fd7c0000";
    const POWER: &[u8] = b"power-controller";

    fn node<'a>(blob: &'a [u8], name: &[u8]) -> Node<'a> {
        Fdt::new(blob)
            .unwrap()
            .nodes()
            .find(|n| n.name() == name)
            .unwrap()
    }

    fn put(blob: &mut [u8], offset: usize, value: u32) {
        blob[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }

    fn edit_structure(blob: &[u8], range: Range<usize>, replacement: &[u8]) -> Vec<u8> {
        let delta = replacement.len() as i64 - range.len() as i64;
        let mut out = blob.to_vec();
        out.splice(range.clone(), replacement.iter().copied());
        for offset in [8, 12, 16] {
            let old = be32(blob, offset).unwrap();
            if old as usize >= range.end {
                put(&mut out, offset, (i64::from(old) + delta) as u32);
            }
        }
        for offset in [4, 36] {
            put(
                &mut out,
                offset,
                (i64::from(be32(blob, offset).unwrap()) + delta) as u32,
            );
        }
        assert!(Fdt::new(&out).is_some());
        out
    }

    fn set(blob: &[u8], name: &[u8], key: &[u8], value: Option<&[u8]>) -> Vec<u8> {
        let n = node(blob, name);
        let (range, string_offset) = match n.property(key) {
            Some(old) => {
                let start = old.as_ptr() as usize - blob.as_ptr() as usize - 12;
                (
                    start..start + 12 + ((old.len() + 3) & !3),
                    be32(blob, start + 8).unwrap(),
                )
            }
            None => {
                let name_end =
                    n.name().as_ptr() as usize - blob.as_ptr() as usize + n.name().len() + 1;
                let start = (name_end + 3) & !3;
                (start..start, be32(blob, 32).unwrap())
            }
        };
        let mut replacement = Vec::new();
        if let Some(value) = value {
            for word in [kumo_fdt::FDT_PROP, value.len() as u32, string_offset] {
                replacement.extend_from_slice(&word.to_be_bytes());
            }
            replacement.extend_from_slice(value);
            while replacement.len() % 4 != 0 {
                replacement.push(0);
            }
        }
        // Insert the string first so intermediate blobs are structurally valid.
        let mut input = blob.to_vec();
        if n.property(key).is_none() && value.is_some() {
            let end = be32(blob, 12).unwrap() as usize + be32(blob, 32).unwrap() as usize;
            let mut entry = key.to_vec();
            entry.push(0);
            input.splice(end..end, entry.iter().copied());
            put(&mut input, 32, be32(blob, 32).unwrap() + entry.len() as u32);
            put(&mut input, 4, be32(blob, 4).unwrap() + entry.len() as u32);
        }
        edit_structure(&input, range, &replacement)
    }

    fn set_words(blob: &[u8], name: &[u8], key: &[u8], values: &[u32]) -> Vec<u8> {
        let bytes: Vec<_> = values.iter().flat_map(|v| v.to_be_bytes()).collect();
        set(blob, name, key, Some(&bytes))
    }

    fn subtree(blob: &[u8], name: &[u8]) -> Range<usize> {
        let start = node(blob, name).name().as_ptr() as usize - blob.as_ptr() as usize - 4;
        let mut cursor = start;
        let mut depth = 0;
        loop {
            let token = be32(blob, cursor).unwrap();
            cursor += 4;
            match token {
                kumo_fdt::FDT_BEGIN_NODE => {
                    depth += 1;
                    while blob[cursor] != 0 {
                        cursor += 1;
                    }
                    cursor = (cursor + 4) & !3;
                }
                kumo_fdt::FDT_END_NODE => {
                    depth -= 1;
                    if depth == 0 {
                        return start..cursor;
                    }
                }
                kumo_fdt::FDT_PROP => {
                    let len = be32(blob, cursor).unwrap() as usize;
                    cursor += 8 + ((len + 3) & !3);
                }
                kumo_fdt::FDT_NOP => {}
                _ => panic!("unexpected subtree token"),
            }
        }
    }

    #[test]
    fn board_blob_identifies_one_two_bank_vop_domain_and_shared_intid() {
        assert_eq!(
            discover(DTB),
            Some(DisplayIoMmu {
                banks: [(0xfdd9_7e00, 0x100), (0xfdd9_7f00, 0x100)],
                interrupt: 188,
            })
        );
        // A valid direct override and the standard three-cell GIC binding are
        // accepted; the vendor's fourth SPI cell must otherwise be zero.
        let mut blob = set_words(DTB, VOP, b"interrupt-parent", &[1]);
        blob = set_words(&blob, MMU, b"interrupt-parent", &[1]);
        assert!(discover(&blob).is_some());
        blob = set_words(&blob, GIC, b"#interrupt-cells", &[3]);
        for name in [VOP, MMU] {
            blob = set_words(&blob, name, b"interrupts", &[0, 156, 4]);
        }
        assert_eq!(discover(&blob), discover(DTB));
    }

    #[test]
    fn exact_apertures_cell_counts_and_single_attachment_are_required() {
        for (name, key) in [
            (VOP, b"reg".as_slice()),
            (MMU, b"reg"),
            (VOP, b"iommus"),
            (MMU, b"#iommu-cells"),
            (b"", b"#address-cells"),
            (b"", b"#size-cells"),
        ] {
            let value = node(DTB, name).property(key).unwrap();
            for len in 0..value.len() {
                assert!(discover(&set(DTB, name, key, Some(&value[..len]))).is_none());
            }
            let mut extra = value.to_vec();
            extra.extend_from_slice(&0u32.to_be_bytes());
            assert!(discover(&set(DTB, name, key, Some(&extra))).is_none());
            for index in 0..value.len() / 4 {
                let mut changed = value.to_vec();
                changed[index * 4 + 3] ^= 1;
                assert!(discover(&set(DTB, name, key, Some(&changed))).is_none());
            }
            assert!(discover(&set(DTB, name, key, None)).is_none());
        }
        for attachment in [0, u32::MAX, 0x21] {
            assert!(discover(&set_words(DTB, VOP, b"iommus", &[attachment])).is_none());
        }
    }

    #[test]
    fn disabled_wrong_compatible_and_nested_devices_never_identify_a_unit() {
        for name in [b"".as_slice(), VOP, MMU, GIC, PMU, POWER, CRU] {
            assert!(discover(&set(DTB, name, b"status", Some(b"disabled\0"))).is_none());
            assert!(discover(&set(DTB, name, b"compatible", Some(b"unrelated\0"))).is_none());
        }
        let range = subtree(DTB, MMU);
        let mut nested = Vec::new();
        nested.extend_from_slice(&kumo_fdt::FDT_BEGIN_NODE.to_be_bytes());
        nested.extend_from_slice(b"bus\0");
        nested.extend_from_slice(&DTB[range.clone()]);
        nested.extend_from_slice(&kumo_fdt::FDT_END_NODE.to_be_bytes());
        assert!(discover(&edit_structure(DTB, range, &nested)).is_none());
    }

    #[test]
    fn irq_parent_specifier_trigger_and_fourth_cell_must_all_agree() {
        for name in [VOP, MMU] {
            for irq in [
                &[1, 156, 4, 0][..],
                &[0, 155, 4, 0],
                &[0, 156, 1, 0],
                &[0, 156, 4, 1],
                &[0, 156, 4],
                &[0, 156, 4, 0, 0],
            ] {
                assert!(discover(&set_words(DTB, name, b"interrupts", irq)).is_none());
            }
            assert!(discover(&set_words(DTB, name, b"interrupt-parent", &[0x21])).is_none());
            assert!(discover(&set_words(
                DTB,
                name,
                b"interrupts-extended",
                &[1, 0, 156, 4, 0]
            ))
            .is_none());
        }
        for count in [0, 1, 2, 5] {
            assert!(discover(&set_words(DTB, GIC, b"#interrupt-cells", &[count])).is_none());
        }
        assert!(discover(&set_words(DTB, b"", b"interrupt-parent", &[0x21])).is_none());
        assert!(discover(&set(DTB, GIC, b"interrupt-controller", None)).is_none());
    }

    #[test]
    fn power_and_clock_providers_must_match_the_vop_and_reviewed_controllers() {
        for name in [VOP, MMU] {
            for domains in [&[0x22, 23][..], &[0x21, 24], &[0x22, 24, 0]] {
                assert!(discover(&set_words(DTB, name, b"power-domains", domains)).is_none());
            }
        }
        for (name, key) in [
            (MMU, b"clocks".as_slice()),
            (VOP, b"clocks"),
            (VOP, b"rockchip,pmu"),
            (PMU, b"reg"),
            (CRU, b"reg"),
            (GIC, b"reg"),
            (POWER, b"#power-domain-cells"),
            (CRU, b"#clock-cells"),
        ] {
            let mut bad = node(DTB, name).property(key).unwrap().to_vec();
            bad[3] ^= 1;
            assert!(discover(&set(DTB, name, key, Some(&bad))).is_none());
        }
        for name in [VOP, MMU] {
            assert!(discover(&set(DTB, name, b"clock-names", Some(b"aclk\0wrong\0"))).is_none());
        }
    }

    #[test]
    fn duplicate_owners_and_overlapping_banks_under_new_phandles_are_rejected() {
        for name in [VOP, MMU, GIC, PMU, CRU] {
            let range = subtree(DTB, name);
            let duplicate = edit_structure(DTB, range.end..range.end, &DTB[range]);
            assert!(discover(&duplicate).is_none());
        }
        let alias = set_words(DTB, MMU, b"phandle", &[0x7fff]);
        let range = subtree(&alias, MMU);
        let at = subtree(DTB, MMU).end;
        assert!(discover(&edit_structure(DTB, at..at, &alias[range])).is_none());
        let legacy = set_words(DTB, MMU, b"linux,phandle", &[0x7f]);
        assert_eq!(discover(&legacy), discover(DTB));
        assert!(discover(&set_words(DTB, MMU, b"linux,phandle", &[0x7fff])).is_none());
        assert!(discover(&set_words(DTB, MMU, b"phandle", &[0x7f, 0])).is_none());
    }

    #[test]
    fn malformed_and_unrelated_trees_have_no_display_unit() {
        for end in [0, 1, 7, 39, DTB.len() - 1] {
            assert!(discover(&DTB[..end]).is_none());
        }
        let mut bad = DTB.to_vec();
        bad[0] = 0;
        assert!(discover(&bad).is_none());
        let v1 = set(DTB, MMU, b"compatible", Some(b"rockchip,rk3588-iommu\0"));
        assert!(discover(&v1).is_none());
    }
    #[test]
    fn unit_capability_range_is_exactly_both_contiguous_banks() {
        let unit = discover(DTB).unwrap();
        assert!(unit.matches_range(0xfdd97e00, 0x200));
        for (base, len) in [
            (0xfdd97e00, 0x100),
            (0xfdd97f00, 0x100),
            (0xfdd97e00, 0x201),
            (0xfdd97000, 0x1000),
        ] {
            assert!(!unit.matches_range(base, len));
        }
    }
}
