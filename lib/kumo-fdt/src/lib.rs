#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
//j598

//! Minimal, bounds-checked reader for flattened device trees (FDT/DTB).
//!
//! The live boot DTB is untrusted input: firmware hands the boot chain a blob whose offsets
//! and lengths are data, not promises. [`Fdt::new`] validates the 40-byte header and then
//! walks the whole structure block once — every token, node name, property, string offset and
//! alignment — before a decoder can see a node. Later reads re-check their bounds and return
//! `Option`; nothing here slices without a bound and nothing panics on malformed bytes.
//!
//! This is a structural reader only. Binding semantics (which `compatible` means a VOP, which
//! cells are a phandle) belong to the consumer, so host tests can exercise the two layers
//! separately. It is deliberately smaller than a general libfdt: KUMO consumers need
//! node/property/phandle traversal, not tree mutation. — TIMBERDOODLE

pub const FDT_MAGIC: u32 = 0xd00d_feed;
pub const FDT_BEGIN_NODE: u32 = 1;
pub const FDT_END_NODE: u32 = 2;
pub const FDT_PROP: u32 = 3;
pub const FDT_NOP: u32 = 4;
pub const FDT_END: u32 = 9;

/// Deepest nesting accepted. Real device trees nest well under ten levels; the bound keeps
/// the validator's depth counter honest against a hostile blob. — TIMBERDOODLE
const MAX_DEPTH: usize = 32;
const HEADER_LEN: usize = 40;
/// Oldest header version whose structure block this reader understands (v16 introduced the
/// current block layout). — TIMBERDOODLE
const MIN_VERSION: u32 = 16;

/// A validated device-tree blob. Construction is the only place that trusts a length, and
/// only after checking it against the caller's slice.
#[derive(Clone, Copy)]
pub struct Fdt<'a> {
    bytes: &'a [u8],
    structs: &'a [u8],
    strings: &'a [u8],
}

impl<'a> Fdt<'a> {
    /// Validate `bytes` as one complete FDT. `None` means "not a device tree I trust": bad
    /// magic or version, a totalsize outside the slice, offsets outside the blob, an
    /// unbalanced or truncated structure block, or a property name outside the strings
    /// block. Never panics.
    pub fn new(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < HEADER_LEN || be32(bytes, 0)? != FDT_MAGIC {
            return None;
        }
        let total = be32(bytes, 4)? as usize;
        if total < HEADER_LEN || total > bytes.len() || be32(bytes, 20)? < MIN_VERSION {
            return None;
        }
        let bytes = bytes.get(..total)?;
        let struct_off = be32(bytes, 8)? as usize;
        let strings_off = be32(bytes, 12)? as usize;
        let strings_len = be32(bytes, 32)? as usize;
        let struct_len = be32(bytes, 36)? as usize;
        if struct_len % 4 != 0 {
            return None;
        }
        let structs = bytes.get(struct_off..struct_off.checked_add(struct_len)?)?;
        let strings = bytes.get(strings_off..strings_off.checked_add(strings_len)?)?;
        validate(structs, strings)?;
        Some(Self {
            bytes,
            structs,
            strings,
        })
    }

    /// The validated blob, exactly `totalsize` bytes.
    pub const fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// The root node (the first node in the structure block).
    pub fn root(&self) -> Option<Node<'a>> {
        self.nodes().next()
    }

    /// Every node in depth-first order.
    pub fn nodes(&self) -> Nodes<'a> {
        Nodes {
            structs: self.structs,
            strings: self.strings,
            cursor: 0,
        }
    }

    /// The first node whose `compatible` list contains `needle`.
    pub fn find_by_compatible(&self, needle: &[u8]) -> Option<Node<'a>> {
        self.nodes().find(|node| {
            node.property(b"compatible")
                .is_some_and(|value| string_list_contains(value, needle))
        })
    }

    /// The node that owns `phandle`, accepting the legacy `linux,phandle` spelling.
    pub fn find_by_phandle(&self, phandle: u32) -> Option<Node<'a>> {
        self.nodes().find(|node| {
            node.property(b"phandle")
                .or_else(|| node.property(b"linux,phandle"))
                .and_then(|value| be32(value, 0))
                == Some(phandle)
        })
    }
}

/// One node in a validated blob. Copyable; it borrows the blob's slices.
#[derive(Clone, Copy)]
pub struct Node<'a> {
    structs: &'a [u8],
    strings: &'a [u8],
    begin: usize,
}

impl<'a> Node<'a> {
    /// The node's name, including its `@unit-address` suffix when present.
    pub fn name(&self) -> &'a [u8] {
        let start = self.begin.saturating_add(4);
        let end = name_end(self.structs, start).unwrap_or(start);
        self.structs.get(start..end).unwrap_or(&[])
    }

    /// The value of this node's own property `name` (not a descendant's), if present.
    pub fn property(&self, name: &[u8]) -> Option<&'a [u8]> {
        let name_start = self.begin.checked_add(4)?;
        let own_name_end = name_end(self.structs, name_start)?;
        let mut cursor = align4(own_name_end.checked_add(1)?)?;
        let mut depth = 0usize;
        loop {
            let token = be32(self.structs, cursor)?;
            cursor = cursor.checked_add(4)?;
            match token {
                FDT_BEGIN_NODE => {
                    depth = depth.checked_add(1)?;
                    let end = name_end(self.structs, cursor)?;
                    cursor = align4(end.checked_add(1)?)?;
                }
                FDT_END_NODE => {
                    if depth == 0 {
                        return None;
                    }
                    depth -= 1;
                }
                FDT_PROP => {
                    let len = be32(self.structs, cursor)? as usize;
                    let name_off = be32(self.structs, cursor.checked_add(4)?)? as usize;
                    cursor = cursor.checked_add(8)?;
                    let value_end = cursor.checked_add(len)?;
                    let value = self.structs.get(cursor..value_end)?;
                    let property_name = property_name(self.strings, name_off)?;
                    cursor = align4(value_end)?;
                    if depth == 0 && property_name == name {
                        return Some(value);
                    }
                }
                FDT_NOP => {}
                FDT_END => return None,
                _ => return None,
            }
        }
    }

    /// This node's direct children, in blob order.
    pub fn children(&self) -> Children<'a> {
        let cursor = self
            .begin
            .checked_add(4)
            .and_then(|start| name_end(self.structs, start))
            .and_then(|end| end.checked_add(1))
            .and_then(align4);
        let done = cursor.is_none();
        Children {
            structs: self.structs,
            strings: self.strings,
            cursor: cursor.unwrap_or(0),
            depth: 0,
            done,
        }
    }

    /// The first child named exactly `name`.
    pub fn child(&self, name: &[u8]) -> Option<Node<'a>> {
        self.children().find(|child| child.name() == name)
    }

    /// The first child whose name starts with `prefix` (e.g. `b"port"`, `b"endpoint"`).
    pub fn child_named(&self, prefix: &[u8]) -> Option<Node<'a>> {
        self.children()
            .find(|child| child.name().starts_with(prefix))
    }
}

/// Depth-first walk over every node of a validated blob.
pub struct Nodes<'a> {
    structs: &'a [u8],
    strings: &'a [u8],
    cursor: usize,
}

impl<'a> Iterator for Nodes<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Node<'a>> {
        loop {
            let token = be32(self.structs, self.cursor)?;
            let begin = self.cursor;
            self.cursor = self.cursor.checked_add(4)?;
            match token {
                FDT_BEGIN_NODE => {
                    let end = name_end(self.structs, self.cursor)?;
                    self.cursor = align4(end.checked_add(1)?)?;
                    return Some(Node {
                        structs: self.structs,
                        strings: self.strings,
                        begin,
                    });
                }
                FDT_END_NODE | FDT_NOP => {}
                FDT_PROP => {
                    let len = be32(self.structs, self.cursor)? as usize;
                    self.cursor = self.cursor.checked_add(8)?;
                    self.cursor = align4(self.cursor.checked_add(len)?)?;
                }
                FDT_END => return None,
                _ => return None,
            }
        }
    }
}

/// Direct-child walk over a validated node.
pub struct Children<'a> {
    structs: &'a [u8],
    strings: &'a [u8],
    cursor: usize,
    depth: usize,
    done: bool,
}

impl<'a> Iterator for Children<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Node<'a>> {
        loop {
            if self.done {
                return None;
            }
            let token = be32(self.structs, self.cursor)?;
            let begin = self.cursor;
            let next = self.cursor.checked_add(4)?;
            match token {
                FDT_BEGIN_NODE => {
                    let end = match name_end(self.structs, next) {
                        Some(end) => end,
                        None => {
                            self.done = true;
                            return None;
                        }
                    };
                    self.cursor = match align4(end.checked_add(1)?) {
                        Some(cursor) => cursor,
                        None => {
                            self.done = true;
                            return None;
                        }
                    };
                    if self.depth == 0 {
                        self.depth = 1;
                        return Some(Node {
                            structs: self.structs,
                            strings: self.strings,
                            begin,
                        });
                    }
                    self.depth = self.depth.saturating_add(1);
                }
                FDT_END_NODE => {
                    self.cursor = next;
                    if self.depth == 0 {
                        self.done = true;
                        return None;
                    }
                    self.depth -= 1;
                }
                FDT_PROP => {
                    let len = be32(self.structs, next)? as usize;
                    let after = next.checked_add(8)?.checked_add(len)?;
                    self.cursor = match align4(after) {
                        Some(cursor) => cursor,
                        None => {
                            self.done = true;
                            return None;
                        }
                    };
                }
                FDT_NOP => self.cursor = next,
                FDT_END => {
                    self.done = true;
                    return None;
                }
                _ => {
                    self.done = true;
                    return None;
                }
            }
        }
    }
}

/// One big-endian cell at `offset`, or `None` when the value is short.
pub fn be32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    Some(u32::from_be_bytes(bytes.get(offset..end)?.try_into().ok()?))
}

/// A string property with its trailing NUL removed.
pub fn string_value(value: &[u8]) -> &[u8] {
    value.strip_suffix(&[0]).unwrap_or(value)
}

/// True when the NUL-separated string list contains `needle`.
pub fn string_list_contains(list: &[u8], needle: &[u8]) -> bool {
    list.split(|byte| *byte == 0).any(|entry| entry == needle)
}

/// Count the non-empty entries of a NUL-separated string list (`clock-names`).
pub fn string_list_count(list: &[u8]) -> usize {
    list.split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .count()
}

/// Device-tree status semantics: absent means enabled; only `okay`/`ok` enable explicitly.
pub fn status_enabled(node: Node<'_>) -> bool {
    match node.property(b"status") {
        None => true,
        Some(value) => {
            let value = string_value(value);
            value == b"okay" || value == b"ok"
        }
    }
}

fn validate(structs: &[u8], strings: &[u8]) -> Option<()> {
    let mut cursor = 0usize;
    let mut depth = 0usize;
    let mut saw_end = false;
    while cursor < structs.len() {
        let token = be32(structs, cursor)?;
        cursor = cursor.checked_add(4)?;
        match token {
            FDT_BEGIN_NODE => {
                if depth == MAX_DEPTH {
                    return None;
                }
                let end = name_end(structs, cursor)?;
                cursor = align4(end.checked_add(1)?)?;
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
                let len = be32(structs, cursor)? as usize;
                let name_off = be32(structs, cursor.checked_add(4)?)? as usize;
                cursor = cursor.checked_add(8)?;
                let value_end = cursor.checked_add(len)?;
                structs.get(cursor..value_end)?;
                property_name(strings, name_off)?;
                cursor = align4(value_end)?;
            }
            FDT_NOP => {}
            FDT_END => {
                if depth != 0 {
                    return None;
                }
                saw_end = true;
                break;
            }
            _ => return None,
        }
    }
    saw_end.then_some(())
}

fn name_end(structs: &[u8], start: usize) -> Option<usize> {
    structs
        .get(start..)?
        .iter()
        .position(|byte| *byte == 0)
        .map(|offset| start + offset)
}

fn property_name(strings: &[u8], offset: usize) -> Option<&[u8]> {
    let end = name_end(strings, offset)?;
    strings.get(offset..end)
}

fn align4(value: usize) -> Option<usize> {
    value.checked_add(3).map(|value| value & !3)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    /// Assemble one complete FDT from structure-block bytes and string-block bytes.
    fn blob(structs: &[u8], strings: &[u8]) -> Vec<u8> {
        let total = HEADER_LEN + structs.len() + strings.len();
        let mut out = vec![0u8; total];
        out[0..4].copy_from_slice(&FDT_MAGIC.to_be_bytes());
        out[4..8].copy_from_slice(&(total as u32).to_be_bytes());
        out[8..12].copy_from_slice(&(HEADER_LEN as u32).to_be_bytes());
        out[12..16].copy_from_slice(&((HEADER_LEN + structs.len()) as u32).to_be_bytes());
        out[16..20].copy_from_slice(&(HEADER_LEN as u32).to_be_bytes());
        out[20..24].copy_from_slice(&17u32.to_be_bytes());
        out[24..28].copy_from_slice(&16u32.to_be_bytes());
        out[32..36].copy_from_slice(&(strings.len() as u32).to_be_bytes());
        out[36..40].copy_from_slice(&(structs.len() as u32).to_be_bytes());
        out[HEADER_LEN..HEADER_LEN + structs.len()].copy_from_slice(structs);
        out[HEADER_LEN + structs.len()..].copy_from_slice(strings);
        out
    }

    /// A tiny tree builder: tokens are appended, strings are interned by name.
    struct Tree {
        structs: Vec<u8>,
        strings: Vec<u8>,
    }

    impl Tree {
        fn new() -> Self {
            Self {
                structs: Vec::new(),
                strings: Vec::new(),
            }
        }

        fn token(&mut self, token: u32) {
            self.structs.extend_from_slice(&token.to_be_bytes());
        }

        fn begin(&mut self, name: &str) {
            self.token(FDT_BEGIN_NODE);
            self.structs.extend_from_slice(name.as_bytes());
            self.structs.push(0);
            while self.structs.len() % 4 != 0 {
                self.structs.push(0);
            }
        }

        fn end(&mut self) {
            self.token(FDT_END_NODE);
        }

        fn name_off(&mut self, name: &str) -> u32 {
            let off = self.strings.len() as u32;
            self.strings.extend_from_slice(name.as_bytes());
            self.strings.push(0);
            off
        }

        fn prop(&mut self, name: &str, value: &[u8]) {
            let name_off = self.name_off(name);
            self.prop_raw(name_off, value);
        }

        fn prop_raw(&mut self, name_off: u32, value: &[u8]) {
            self.token(FDT_PROP);
            self.structs
                .extend_from_slice(&(value.len() as u32).to_be_bytes());
            self.structs.extend_from_slice(&name_off.to_be_bytes());
            self.structs.extend_from_slice(value);
            while self.structs.len() % 4 != 0 {
                self.structs.push(0);
            }
        }

        fn prop_u32(&mut self, name: &str, value: u32) {
            self.prop(name, &value.to_be_bytes());
        }

        fn prop_str(&mut self, name: &str, value: &str) {
            let mut bytes = Vec::from(value.as_bytes());
            bytes.push(0);
            self.prop(name, &bytes);
        }

        fn bytes(self) -> Vec<u8> {
            blob(&self.structs, &self.strings)
        }

        fn finish(mut self) -> Vec<u8> {
            self.token(FDT_END);
            self.bytes()
        }
    }

    fn sample() -> Vec<u8> {
        let mut tree = Tree::new();
        tree.begin("");
        tree.prop_str("compatible", "rockchip,rk3588");
        tree.prop_u32("#address-cells", 2);
        tree.begin("vop@fdd90000");
        tree.prop_str("compatible", "rockchip,rk3588-vop");
        tree.prop(
            "reg",
            &[0, 0, 0, 0, 0xfd, 0xd9, 0, 0, 0, 0, 0, 0, 0, 0, 0x42, 0],
        );
        tree.prop_u32("phandle", 0x1b);
        tree.end();
        tree.begin("iommu@fdd97e00");
        tree.prop_str("compatible", "rockchip,rk3588-iommu");
        tree.prop_u32("phandle", 0x7f);
        tree.end();
        tree.end();
        tree.finish()
    }

    #[test]
    fn finds_nodes_properties_and_children() {
        let bytes = sample();
        let fdt = Fdt::new(&bytes).unwrap();
        let root = fdt.root().unwrap();
        assert_eq!(root.name(), b"");
        assert_eq!(
            root.property(b"compatible"),
            Some(&b"rockchip,rk3588\0"[..])
        );
        assert!(string_list_contains(
            root.property(b"compatible").unwrap(),
            b"rockchip,rk3588"
        ));
        let vop = fdt.find_by_compatible(b"rockchip,rk3588-vop").unwrap();
        assert_eq!(vop.name(), b"vop@fdd90000");
        assert_eq!(be32(vop.property(b"reg").unwrap(), 4), Some(0xfdd9_0000));
        assert_eq!(be32(vop.property(b"reg").unwrap(), 12), Some(0x4200));
        assert!(vop.property(b"status").is_none());
        assert!(status_enabled(vop));
        let phandle = fdt.find_by_phandle(0x7f).unwrap();
        assert_eq!(phandle.name(), b"iommu@fdd97e00");
        assert!(fdt.find_by_phandle(0x7e).is_none());
        assert_eq!(root.children().count(), 2);
        assert_eq!(root.child(b"vop@fdd90000").unwrap().name(), b"vop@fdd90000");
        assert_eq!(root.child_named(b"iom").unwrap().name(), b"iommu@fdd97e00");
        assert_eq!(vop.children().count(), 0);
        assert_eq!(string_list_count(b"a\0b\0c\0"), 3);
        assert_eq!(string_value(b"okay\0"), b"okay");
    }

    #[test]
    fn nested_children_and_status_are_not_confused_with_the_parent() {
        let mut tree = Tree::new();
        tree.begin("");
        tree.prop_str("status", "disabled");
        tree.begin("port@0");
        tree.begin("endpoint");
        tree.prop_u32("remote-endpoint", 0x90);
        tree.end();
        tree.end();
        tree.end();
        let bytes = tree.finish();
        let fdt = Fdt::new(&bytes).unwrap();
        let root = fdt.root().unwrap();
        assert!(!status_enabled(root));
        assert!(root.property(b"remote-endpoint").is_none());
        let port = root.child(b"port@0").unwrap();
        let endpoint = port.child(b"endpoint").unwrap();
        assert_eq!(
            be32(endpoint.property(b"remote-endpoint").unwrap(), 0),
            Some(0x90)
        );
        assert_eq!(port.children().count(), 1);
    }

    #[test]
    fn rejects_malformed_and_truncated_blobs() {
        let good = sample();
        assert!(Fdt::new(&good).is_some());

        let mut bad = good.clone();
        bad[0] = 0;
        assert!(Fdt::new(&bad).is_none());

        let mut bad = good.clone();
        bad[4..8].copy_from_slice(&(good.len() as u32 + 1).to_be_bytes());
        assert!(Fdt::new(&bad).is_none());

        let mut bad = good.clone();
        bad[4..8].copy_from_slice(&8u32.to_be_bytes());
        assert!(Fdt::new(&bad).is_none());

        let mut bad = good.clone();
        bad[8..12].copy_from_slice(&0xffff_fff0u32.to_be_bytes());
        assert!(Fdt::new(&bad).is_none());

        let mut bad = good.clone();
        bad[36..40].copy_from_slice(&0xffff_fffcu32.to_be_bytes());
        assert!(Fdt::new(&bad).is_none());

        let mut bad = good.clone();
        bad[36..40].copy_from_slice(&3u32.to_be_bytes());
        assert!(Fdt::new(&bad).is_none());

        let mut bad = good.clone();
        bad[20..24].copy_from_slice(&15u32.to_be_bytes());
        assert!(Fdt::new(&bad).is_none());

        assert!(Fdt::new(&good[..good.len() - 4]).is_none());
        assert!(Fdt::new(&[]).is_none());
        assert!(Fdt::new(&good[..20]).is_none());
    }

    #[test]
    fn rejects_unbalanced_or_unterminated_structure_blocks() {
        // A node left open when the blocks end.
        let mut tree = Tree::new();
        tree.begin("");
        tree.end();
        tree.begin("orphan");
        assert!(Fdt::new(&tree.bytes()).is_none());

        // FDT_END while a node is still open.
        let mut tree = Tree::new();
        tree.begin("");
        assert!(Fdt::new(&tree.finish()).is_none());

        // One END_NODE too many.
        let mut tree = Tree::new();
        tree.begin("");
        tree.end();
        tree.end();
        assert!(Fdt::new(&tree.finish()).is_none());

        // A property outside any node.
        let mut tree = Tree::new();
        tree.begin("");
        tree.end();
        tree.prop_u32("stray", 1);
        assert!(Fdt::new(&tree.finish()).is_none());

        // A property name offset outside the strings block.
        let mut tree = Tree::new();
        tree.begin("");
        tree.prop_raw(0xffff_fff0, &[0, 0, 0, 0]);
        tree.end();
        assert!(Fdt::new(&tree.finish()).is_none());
    }

    #[test]
    fn parses_the_tracked_orangepi_blob() {
        let bytes = include_bytes!("../../../rk3588-orangepi-5-plus.dtb");
        let fdt = Fdt::new(bytes).expect("tracked board DTB must validate");
        assert_eq!(fdt.bytes().len(), bytes.len());
        assert_eq!(fdt.root().unwrap().name(), b"");
        let nodes = fdt.nodes().count();
        assert!(nodes > 100, "expected a full board tree, found {nodes}");
        let compatible = fdt.root().unwrap().property(b"compatible").unwrap();
        assert!(string_list_contains(compatible, b"rockchip,rk3588"));
    }
}
