//j597
//j601
//j611

//! D0a live adapter: read-only mappings and serial evidence; no device programming.

use core::fmt::{self, Write};
use drv_fb::probe_va::VOP_D0A;
use drv_fb::rk3588::{self, Power, ReadRegisters, Snapshot};
use kumo_abi::{BootInfo, Handle, VmarFlags};
use kumo_rt::{handle_close, resource_mint_mmio, syscall_handle, vmar_map};

struct Log {
    bytes: [u8; 320],
    len: usize,
}

impl Write for Log {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len.checked_add(text.len()).ok_or(fmt::Error)?;
        let dst = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
        dst.copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

fn log(args: fmt::Arguments<'_>) {
    let mut line = Log {
        bytes: [0; 320],
        len: 0,
    };
    if line.write_fmt(args).is_ok() {
        let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &line.bytes[..line.len]);
    }
}

struct Registers;
impl ReadRegisters for Registers {
    fn vop(&mut self, offset: u32) -> u32 {
        assert!(u64::from(offset) + 4 <= rk3588::VOP_LEN && offset % 4 == 0);
        unsafe { core::ptr::read_volatile((VOP_D0A + u64::from(offset)) as *const u32) }
    }
}

fn map(resource: Handle, base: u64, len: u64, va: u64) -> bool {
    if resource.0 == 0 {
        return false;
    }
    let raw = resource_mint_mmio(resource, base, len);
    let Some(vmo) = syscall_handle(raw) else {
        log(format_args!(
            "display: VOP mint failed (errno {})\n",
            raw as u32 as i32
        ));
        return false;
    };
    // Device memory is the default for an MMIO VMO; READ gives an EL0 read-only mapping.
    // The process never retains a writable mapping of these registers. — TIMBERDOODLE
    let ok = vmar_map(Handle(0), vmo, 0, va, len, VmarFlags::READ.0) == 0;
    let _ = handle_close(vmo);
    ok
}

/// Returns whether D0a's read-only VOP window (including the MMU page) is mapped at
/// `VOP_D0A`; D1b reads the display IOMMU's interrupt registers through it (J611).
pub fn report(boot: &BootInfo, power: Option<Power>, vop: Handle) -> bool {
    let mut mapped = false;
    if rk3588::eligible(
        boot.board_id(),
        boot.has_framebuffer() && boot.framebuffer.is_plausible(),
    ) {
        if let Some(power) = power {
            mapped = probe(boot, power, vop);
        } else {
            log(format_args!(
                "display: no PMU observation; D0a probe skipped\n"
            ));
        }
    }
    if vop.0 != 0 {
        let _ = handle_close(vop);
    }
    mapped
}

fn probe(boot: &BootInfo, power: Power, vop: Handle) -> bool {
    log(format_args!(
        "display: D0a read-only firmware scanout probe\n"
    ));
    log(format_args!(
        "display: PMU gate={:#010x} idle request/ack/status={:#010x}/{:#010x}/{:#010x}\n",
        power.gate_status, power.idle_request, power.idle_ack, power.idle_status
    ));
    if !power.vop_accessible() {
        log(format_args!(
            "display: VOP down or bus idle; zero VOP reads\n"
        ));
        return false;
    }
    if !map(vop, rk3588::VOP_BASE, rk3588::VOP_LEN, VOP_D0A) {
        log(format_args!("display: VOP mapping failed; probe skipped\n"));
        return false;
    }
    let Some(snapshot) = Snapshot::read(power, &mut Registers) else {
        return true;
    };
    let s = snapshot.system;
    log(format_args!(
        "display: VOP version={:#010x} if_en/ctrl/pol={:#010x}/{:#010x}/{:#010x} power={:#010x}\n",
        s[0], s[1], s[2], s[3], s[4]
    ));
    log(format_args!(
        "display: overlay ctrl/layers/ports={:#010x}/{:#010x}/{:#010x}\n",
        s[5], s[6], s[7]
    ));
    for (index, port) in snapshot.ports.iter().enumerate() {
        let w = port.words;
        log(format_args!(
            "display: VP{} ctrl/clock={:#010x}/{:#010x} H={:#010x}/{:#010x} V={:#010x}/{:#010x}\n",
            index, w[0], w[1], w[2], w[3], w[4], w[5]
        ));
        if let Some((h, v)) = port.timing() {
            log(format_args!(
                "display: VP{} {}x{} H total/sync/start/end={}/{}/{}/{} V={}/{}/{}/{}\n",
                index,
                h.active(),
                v.active(),
                h.total,
                h.sync,
                h.start,
                h.end,
                v.total,
                v.sync,
                v.start,
                v.end
            ));
        }
    }
    for window in snapshot.windows.iter().filter(|w| w.enabled()) {
        let w = window.words;
        let (width, height) = window.dimensions();
        log(format_args!("display: window +{:#06x} ctrl={:#010x} format={} YRGB={:#010x} VIR={:#010x} ACT/DSP/POS={:#010x}/{:#010x}/{:#010x} {}x{}\n",
            window.offset, w[0], window.format(), w[1], w[2], w[3], w[4], w[5], width, height));
    }
    for (index, mmu) in snapshot.mmus.iter().enumerate() {
        log(format_args!(
            "display: MMU{} DTE={:#010x} status={:#010x} paging={} fault={}\n",
            index,
            mmu[0],
            mmu[1],
            mmu[1] & 1,
            (mmu[1] >> 1) & 1
        ));
    }
    let fb = boot.framebuffer;
    log(format_args!(
        "display: GOP phys={:#x} {}x{} stride={} len={:#x} format={}\n",
        fb.phys, fb.width, fb.height, fb.stride, fb.len, fb.format as u32
    ));
    match snapshot.matching_hdmi(fb.width, fb.height) {
        Some((hdmi, port)) => log(format_args!(
            "display: D0a geometry match: HDMI{} <- VP{}; firmware scanout preserved\n",
            hdmi, port
        )),
        None => log(format_args!(
            "display: D0a no unique progressive HDMI/GOP geometry match; raw snapshot retained\n"
        )),
    }
    true
}
