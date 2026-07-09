//j427
//j428

//! ARM SMMUv2 / MMU-500 model + bring-up (the X13s `apps_smmu`).
//!
//! The SC8280XP's system MMU is a Qualcomm MMU-500 — an **SMMUv2** implementation
//! (`compatible = "qcom,sc8280xp-smmu-500", "arm,mmu-500"`, `#iommu-cells = <2>`), NOT the
//! SMMUv3 that [`crate::smmuv3`] scaffolds. SMMUv2 uses Stream Match Registers (`SMRn`) →
//! Stream-to-Context Registers (`S2CRn`) → per-context-bank translation, a wholly different
//! programming model from SMMUv3's stream table / STE / CD / command queue.
//!
//! This module lands the first brick of the real DMA-isolation stack for the X13s: put the
//! `apps_smmu` into a known **all-streams-bypass** state with the SMMU globally enabled — KUMO
//! owns the MMU-500, but every stream still bypasses translation, so the already-running display
//! DMA (and every other master) keeps working. Per-stream translation contexts (mapping USB DMA
//! behind stream `0x820`) are a later slice.
//!
//! The register logic is pure and host-provable over a [`SmmuRegisterIo`] mock (the `ReplayBus`
//! discipline PLAN_VI §2 asks for); the HAL supplies the real MMIO. Register layout and the
//! bring-up order mirror Linux `drivers/iommu/arm/arm-smmu/arm-smmu.c`
//! (`arm_smmu_device_reset`, `disable_bypass = false`). — CORVUS

// ---- Global register block (GR0) byte offsets (arm-smmu.h) -------------------------------------
const GR0_SCR0: usize = 0x0;
const GR0_ID0: usize = 0x20;
const GR0_ID1: usize = 0x24;
const GR0_SGFSR: usize = 0x48;
const GR0_TLBIALLNSNH: usize = 0x68;
const GR0_TLBIALLH: usize = 0x6c;
const GR0_STLBGSYNC: usize = 0x70;
const GR0_STLBGSTATUS: usize = 0x74;
const GR0_SMR_BASE: usize = 0x800;
const GR0_S2CR_BASE: usize = 0xc00;
const SME_STRIDE: usize = 4;

// ---- sCR0 fields ------------------------------------------------------------------------------
const SCR0_CLIENTPD: u32 = 1 << 0;
const SCR0_GFRE: u32 = 1 << 1;
const SCR0_GFIE: u32 = 1 << 2;
const SCR0_GCFGFRE: u32 = 1 << 4;
const SCR0_GCFGFIE: u32 = 1 << 5;
const SCR0_USFCFG: u32 = 1 << 10;
const SCR0_VMIDPNE: u32 = 1 << 11;
const SCR0_PTM: u32 = 1 << 12;
const SCR0_FB: u32 = 1 << 13;
const SCR0_BSU_MASK: u32 = 0b11 << 14;

// ---- ID0 / ID1 sizing fields ------------------------------------------------------------------
const ID0_NUMSMRG_MASK: u32 = 0xff;
const ID1_NUMCB_MASK: u32 = 0xff;

// ---- SMR / S2CR reset values ------------------------------------------------------------------
/// A reset SMR is invalid (`VALID`, bit 31, clear) so it matches no stream.
const SMR_INVALID: u32 = 0;
/// S2CR type field (bits [17:16]); `BYPASS = 1`.
const S2CR_TYPE_SHIFT: u32 = 16;
const S2CR_TYPE_BYPASS: u32 = 1;

// ---- TLB sync ---------------------------------------------------------------------------------
const STLBGSTATUS_GSACTIVE: u32 = 1 << 0;
/// Qualcomm requires a nonzero write to the invalidate/sync registers (`QCOM_DUMMY_VAL = -1`).
const QCOM_DUMMY_VAL: u32 = 0xffff_ffff;
/// Bound on the global-sync poll so a wedged SMMU cannot hang boot (best-effort, like Linux).
const TLB_SYNC_MAX_SPINS: u32 = 1_000_000;

/// A read/write port into one SMMU's global register block. `offset` is a byte offset from the
/// SMMU base. Abstracted so the bring-up sequence is host-provable against a mock bus.
pub trait SmmuRegisterIo {
    fn read(&mut self, offset: usize) -> u32;
    fn write(&mut self, offset: usize, value: u32);
}

/// Number of Stream Mapping Register Groups (`ID0.NUMSMRG`) — the count of `SMRn`/`S2CRn` pairs.
pub const fn num_stream_map_groups(id0: u32) -> u32 {
    id0 & ID0_NUMSMRG_MASK
}

/// Number of context banks (`ID1.NUMCB`).
pub const fn num_context_banks(id1: u32) -> u32 {
    id1 & ID1_NUMCB_MASK
}

/// The `S2CRn` word that makes a matched stream bypass translation.
pub const fn s2cr_bypass() -> u32 {
    S2CR_TYPE_BYPASS << S2CR_TYPE_SHIFT
}

/// Compute the `sCR0` value that engages the SMMU with **all streams bypassing**: enable client
/// access (`CLIENTPD` clear) with unmatched streams bypassing (`USFCFG` clear), turn on global +
/// config fault reporting, keep TLB maintenance private (`VMIDPNE | PTM`), and neither force
/// broadcast nor upgrade barriers. Mirrors `arm_smmu_device_reset` with `disable_bypass = false`.
pub const fn scr0_bypass(current: u32) -> u32 {
    let set = SCR0_GFRE | SCR0_GFIE | SCR0_GCFGFRE | SCR0_GCFGFIE | SCR0_VMIDPNE | SCR0_PTM;
    let clear = SCR0_CLIENTPD | SCR0_USFCFG | SCR0_FB | SCR0_BSU_MASK;
    (current | set) & !clear
}

/// What the bypass bring-up observed/programmed, for the boot report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SmmuBypassReport {
    pub base: u64,
    pub num_stream_map_groups: u32,
    pub num_context_banks: u32,
    pub scr0: u32,
}

/// Drive one MMU-500 into the all-streams-bypass, globally-enabled state.
///
/// Order follows `arm_smmu_device_reset`: clear the global fault status, mark every stream-map
/// group invalid/bypass, invalidate the TLB, then write `sCR0`. Context-bank teardown is omitted
/// deliberately — with every `SMRn` invalid and `USFCFG` clear no stream is ever routed to a
/// context bank, so their state is unreachable until the per-stream-context slice programs them.
///
/// **DO NOT call on the X13s / any Qualcomm MMU-500 yet.** This is the *generic* `arm-smmu` reset.
/// It is missing the Qualcomm impl (`arm-smmu-qcom.c` `qcom_smmu_cfg_probe`): on qcom the bootloader
/// leaves the **continuous-splash display** stream programmed in an `SMRn`, and the qcom driver
/// *preserves* those bootloader SMRs (plus reserves a context bank for the S2CR-bypass quirk).
/// Blindly invalidating every SMR here wipes the display stream, so flipping `SMMUEN` blanks the
/// panel and the board resets — this is exactly the j427 regression. Kept for host tests and as the
/// spine of the future qcom-aware bring-up; gate any metal caller behind that work. — CORVUS j428
pub fn global_bypass_init<IO: SmmuRegisterIo>(io: &mut IO, base: u64) -> SmmuBypassReport {
    // Clear any latched global fault (write-1-to-clear the bits we read).
    let gfsr = io.read(GR0_SGFSR);
    io.write(GR0_SGFSR, gfsr);

    let id0 = io.read(GR0_ID0);
    let id1 = io.read(GR0_ID1);
    let num_smr = num_stream_map_groups(id0);
    let num_cb = num_context_banks(id1);

    // Reset stream mapping: every SMR invalid, every S2CR bypass. MMU-500 always implements
    // stream matching, so the group count comes straight from ID0.NUMSMRG.
    for group in 0..num_smr as usize {
        io.write(GR0_SMR_BASE + group * SME_STRIDE, SMR_INVALID);
        io.write(GR0_S2CR_BASE + group * SME_STRIDE, s2cr_bypass());
    }

    // Invalidate the whole TLB (hyp + non-secure non-hyp), then wait for the global sync.
    io.write(GR0_TLBIALLH, QCOM_DUMMY_VAL);
    io.write(GR0_TLBIALLNSNH, QCOM_DUMMY_VAL);
    sync_global(io);

    // Push the button: engage clients in bypass.
    let scr0 = scr0_bypass(io.read(GR0_SCR0));
    io.write(GR0_SCR0, scr0);

    SmmuBypassReport {
        base,
        num_stream_map_groups: num_smr,
        num_context_banks: num_cb,
        scr0,
    }
}

fn sync_global<IO: SmmuRegisterIo>(io: &mut IO) {
    io.write(GR0_STLBGSYNC, QCOM_DUMMY_VAL);
    let mut spins = 0u32;
    while io.read(GR0_STLBGSTATUS) & STLBGSTATUS_GSACTIVE != 0 {
        spins += 1;
        if spins >= TLB_SYNC_MAX_SPINS {
            break;
        }
        core::hint::spin_loop();
    }
}

// ---- X13s apps_smmu DTB discovery -------------------------------------------------------------

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;
const MAX_DEPTH: usize = 32;

/// The MMU-500 the X13s exposes: `apps_smmu@15000000`, `reg = <0 0x15000000 0 0x100000>`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AppsSmmuTopology {
    pub base: u64,
    pub length: u64,
}

/// The fixed SC8280XP `apps_smmu` register base. The SoC exposes several `arm,mmu-500` instances
/// (the GPU's `adreno_smmu`, etc.); this base picks the peripheral SMMU that services USB DMA
/// (stream `0x820`), the one this bring-up targets. Trust-but-verify, as j426 did for `usb_0`.
pub const X13S_APPS_SMMU_BASE: u64 = 0x1500_0000;

/// Find the SC8280XP `apps_smmu` (the `arm,mmu-500` at [`X13S_APPS_SMMU_BASE`]) in the flattened
/// device tree and return its register window. Returns `None` on a device tree without that node
/// (e.g. QEMU `virt`, or the GPU-only SMMU), which is how the bring-up gates itself to the X13s.
pub fn discover_apps_smmu(dtb: &[u8]) -> Option<AppsSmmuTopology> {
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

    let mut depth = 0usize;
    // Per-depth candidate state: does this node claim `arm,mmu-500`, and its `reg` cells.
    let mut is_mmu500 = [false; MAX_DEPTH];
    let mut reg = [[0u32; 4]; MAX_DEPTH];
    let mut reg_len = [0u8; MAX_DEPTH];
    let mut cursor = 0usize;

    while cursor < structures.len() {
        let token = be32(structures, cursor)?;
        cursor += 4;
        match token {
            FDT_BEGIN_NODE => {
                if depth == MAX_DEPTH {
                    return None;
                }
                let end = structures.get(cursor..)?.iter().position(|b| *b == 0)? + cursor;
                is_mmu500[depth] = false;
                reg_len[depth] = 0;
                depth += 1;
                cursor = align4(end + 1)?;
            }
            FDT_END_NODE => {
                if depth == 0 {
                    return None;
                }
                let node = depth - 1;
                if is_mmu500[node] && reg_len[node] >= 4 {
                    let cells = &reg[node];
                    let base = ((cells[0] as u64) << 32) | cells[1] as u64;
                    let length = ((cells[2] as u64) << 32) | cells[3] as u64;
                    // Several MMU-500 instances exist; take only the peripheral `apps_smmu`.
                    if base == X13S_APPS_SMMU_BASE {
                        return Some(AppsSmmuTopology { base, length });
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
                let node = depth - 1;
                match name {
                    b"compatible" => {
                        if string_list_contains(value, b"arm,mmu-500") {
                            is_mmu500[node] = true;
                        }
                    }
                    b"reg" => {
                        let count = (value.len() / 4).min(4);
                        for (i, slot) in reg[node].iter_mut().take(count).enumerate() {
                            *slot = be32(value, i * 4).unwrap_or(0);
                        }
                        reg_len[node] = count as u8;
                    }
                    _ => {}
                }
            }
            FDT_NOP => {}
            FDT_END => break,
            _ => return None,
        }
    }
    None
}

fn property_name(strings: &[u8], offset: usize) -> Option<&[u8]> {
    let end = strings.get(offset..)?.iter().position(|b| *b == 0)? + offset;
    strings.get(offset..end)
}

fn string_list_contains(list: &[u8], needle: &[u8]) -> bool {
    list.split(|b| *b == 0).any(|entry| entry == needle)
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

    /// A mock GR0 bus that serves programmable read values and records every write in order.
    struct MockSmmu {
        id0: u32,
        id1: u32,
        writes: [(usize, u32); 32],
        write_len: usize,
    }

    impl MockSmmu {
        fn new(id0: u32, id1: u32) -> Self {
            MockSmmu {
                id0,
                id1,
                writes: [(0, 0); 32],
                write_len: 0,
            }
        }

        fn writes(&self) -> &[(usize, u32)] {
            &self.writes[..self.write_len]
        }
    }

    impl SmmuRegisterIo for MockSmmu {
        fn read(&mut self, offset: usize) -> u32 {
            match offset {
                GR0_ID0 => self.id0,
                GR0_ID1 => self.id1,
                GR0_SCR0 => SCR0_CLIENTPD, // firmware left clients bypassing globally
                GR0_STLBGSTATUS => 0,      // GSACTIVE clear -> global sync returns immediately
                _ => 0,
            }
        }
        fn write(&mut self, offset: usize, value: u32) {
            self.writes[self.write_len] = (offset, value);
            self.write_len += 1;
        }
    }

    #[test]
    fn scr0_bypass_engages_clients_without_translation() {
        // Firmware left the SMMU globally bypassing (CLIENTPD set); bypass init must clear it.
        let out = scr0_bypass(SCR0_CLIENTPD);
        assert_eq!(out & SCR0_CLIENTPD, 0, "clients enabled");
        assert_eq!(out & SCR0_USFCFG, 0, "unmatched streams bypass, not fault");
        assert_ne!(out & SCR0_GFRE, 0, "global fault reporting on");
        assert_ne!(out & SCR0_GCFGFRE, 0, "config fault reporting on");
        assert_ne!(
            out & (SCR0_VMIDPNE | SCR0_PTM),
            0,
            "TLB maintenance private"
        );
        assert_eq!(
            out & (SCR0_FB | SCR0_BSU_MASK),
            0,
            "no forced broadcast / barrier upgrade"
        );
    }

    #[test]
    fn stream_map_words_match_v2_encoding() {
        assert_eq!(s2cr_bypass(), 1 << 16);
        assert_eq!(num_stream_map_groups(0x0000_0080), 128);
        assert_eq!(num_context_banks(0x0000_0010), 16);
    }

    #[test]
    fn bypass_init_resets_streams_then_enables_in_order() {
        // Two stream-map groups, so the reset loop is observable.
        let mut smmu = MockSmmu::new(/* id0 NUMSMRG */ 2, /* id1 NUMCB */ 4);
        let report = global_bypass_init(&mut smmu, X13S_APPS_SMMU_BASE);

        assert_eq!(report.base, X13S_APPS_SMMU_BASE);
        assert_eq!(report.num_stream_map_groups, 2);
        assert_eq!(report.num_context_banks, 4);
        assert_eq!(report.scr0 & SCR0_CLIENTPD, 0);

        let w = smmu.writes();
        // sGFSR cleared first.
        assert_eq!(w[0], (GR0_SGFSR, 0));
        // Both groups reset: SMR invalid + S2CR bypass.
        assert_eq!(w[1], (GR0_SMR_BASE, SMR_INVALID));
        assert_eq!(w[2], (GR0_S2CR_BASE, s2cr_bypass()));
        assert_eq!(w[3], (GR0_SMR_BASE + 4, SMR_INVALID));
        assert_eq!(w[4], (GR0_S2CR_BASE + 4, s2cr_bypass()));
        // TLB invalidate, sync, then sCR0 — sCR0 is strictly last.
        assert_eq!(w[5], (GR0_TLBIALLH, QCOM_DUMMY_VAL));
        assert_eq!(w[6], (GR0_TLBIALLNSNH, QCOM_DUMMY_VAL));
        assert_eq!(w[7], (GR0_STLBGSYNC, QCOM_DUMMY_VAL));
        let last = *w.last().unwrap();
        assert_eq!(last.0, GR0_SCR0);
        assert_eq!(last.1, report.scr0);
    }

    #[test]
    fn discovers_the_x13s_apps_smmu_window() {
        let topo = discover_apps_smmu(X13S_DTB).expect("apps_smmu");
        assert_eq!(topo.base, X13S_APPS_SMMU_BASE);
        assert_eq!(topo.length, 0x0010_0000);
    }

    #[test]
    fn rejects_non_dtb_or_boards_without_an_mmu500() {
        assert_eq!(discover_apps_smmu(b"not a dtb"), None);
        assert_eq!(discover_apps_smmu(&X13S_DTB[..32]), None);
    }
}
