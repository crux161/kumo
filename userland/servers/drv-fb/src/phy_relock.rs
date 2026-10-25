//j609

//! R2's live device-window adapter. The CRU view is read-only; each reset is a checked
//! DeviceResetSet. All PHY data is the measured image, not a vendor init table. — KESTREL

use core::fmt::{self, Write};
use drv_fb::{probe_va::*, rk3588::Power, rk3588_hdmi as hdmi, rk3588_relock as relock};
use kumo_abi::{Handle, VmarFlags};
use kumo_rt::{
    clock_get, device_reset_set, handle_close, resource_mint_mmio, syscall_handle, vmar_map,
};

pub struct Grants {
    pub phy: Handle,
    pub grf: Handle,
}
impl Drop for Grants {
    fn drop(&mut self) {
        for h in [self.phy, self.grf] {
            if h.0 != 0 {
                let _ = handle_close(h);
            }
        }
    }
}

struct Line {
    bytes: [u8; 384],
    len: usize,
}
impl Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len.checked_add(s.len()).ok_or(fmt::Error)?;
        self.bytes
            .get_mut(self.len..end)
            .ok_or(fmt::Error)?
            .copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}
pub fn log(error: bool, args: fmt::Arguments<'_>) {
    let mut line = Line {
        bytes: [0; 384],
        len: 0,
    };
    if line.write_fmt(args).is_ok() {
        let _ = kumo_rt::log_write(if error { 3 } else { 5 }, &line.bytes[..line.len]);
    }
}

fn read(base: u64, offset: u32) -> u32 {
    unsafe { core::ptr::read_volatile((base + u64::from(offset)) as *const u32) }
}
fn put(base: u64, offset: u32, value: u32) {
    unsafe {
        core::ptr::write_volatile((base + u64::from(offset)) as *mut u32, value);
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}
struct Device {
    resource: Handle,
}
impl relock::Hardware for Device {
    fn phy_read(&mut self, offset: u32) -> u32 {
        assert!(offset % 4 == 0 && offset < hdmi::PHY_LEN as u32);
        read(PHY_R2, offset)
    }
    fn phy_write(&mut self, offset: u32, value: u32) {
        assert!(offset % 4 == 0 && offset < hdmi::PHY_LEN as u32);
        put(PHY_R2, offset, value);
    }
    fn grf_read(&mut self, offset: u32) -> u32 {
        assert!(offset == 0 || offset == 0x80);
        read(GRF_R2, offset)
    }
    fn grf_write(&mut self, offset: u32, value: u32) {
        assert_eq!(offset, 0);
        assert_eq!((value >> 16) & !0xe0, 0);
        put(GRF_R2, offset, value);
    }
    fn reset(&mut self, line: u32, asserted: bool) -> bool {
        device_reset_set(self.resource, hdmi::PHY_BASE, line, asserted) == 0
    }
    fn now_us(&mut self) -> u64 {
        clock_get() / 1000
    }
    fn delay_us(&mut self, us: u64) {
        let start = clock_get();
        while clock_get().saturating_sub(start) < us * 1000 {
            core::hint::spin_loop();
        }
    }
}

fn map(resource: Handle, base: u64, len: u64, va: u64) -> bool {
    let Some(vmo) = syscall_handle(resource_mint_mmio(resource, base, len)) else {
        return false;
    };
    let ok = vmar_map(
        Handle(0),
        vmo,
        0,
        va,
        len.next_multiple_of(4096),
        (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::DEVICE).0,
    ) == 0;
    let _ = handle_close(vmo);
    ok
}

pub fn prepare(grants: &Grants) -> bool {
    if grants.phy.0 == 0 || grants.grf.0 == 0 {
        log(
            true,
            format_args!("display: R2 unavailable: PHY/GRF grants absent\n"),
        );
        return false;
    }
    if !map(grants.phy, hdmi::PHY_BASE, hdmi::PHY_LEN, PHY_R2)
        || !map(grants.grf, hdmi::PHY_GRF_BASE, hdmi::PHY_GRF_LEN, GRF_R2)
    {
        log(
            true,
            format_args!("display: R2 unavailable: PHY/GRF mapping refused\n"),
        );
        return false;
    }
    let mut dev = Device {
        resource: grants.phy,
    };
    let different = relock::mismatches(&mut dev);
    let con = read(GRF_R2, 0);
    let status = read(GRF_R2, 0x80);
    if different != 0 || con & 0xce1 != 0xe0 || status & relock::READY != relock::READY {
        log(true, format_args!("display: R2 refused, no reset/write: {} PHY fields differ; GRF CON={:#x} STATUS={:#x}\n", different, con, status));
        return false;
    }
    log(false, format_args!("display: R2 measured PHY image matches 109 words, 108 MHz TMDS; entering standby for re-lock\n"));
    true
}

pub fn run(grants: &Grants) -> Option<relock::Locked> {
    let apb_before = read(CRU_D0C, 0xb20);
    let pmu_before = read(CRU_D0C, 0x30a0c);
    let result = relock::run(&mut Device {
        resource: grants.phy,
    });
    let apb_after = read(CRU_D0C, 0xb20);
    let pmu_after = read(CRU_D0C, 0x30a0c);
    let shared_unchanged = apb_before == apb_after && pmu_before == pmu_after;
    log(!shared_unchanged, format_args!("display: R2 reset readback CON72 {:#x}->{:#x}, PMU1 CON03 {:#x}->{:#x}, all lines released and neighbors unchanged={}\n", apb_before, apb_after, pmu_before, pmu_after, u8::from(shared_unchanged)));
    match result {
        Ok(locked) if shared_unchanged => {
            log(false, format_args!("display: R2 PHY re-locked{}: loss of lock observed; clock ready {} us, lanes/PLL locked {} us; 109 rewritten fields match\n", if locked.retried { " after one retry" } else { "" }, locked.clock_us, locked.lanes_us));
            Some(locked)
        }
        error => {
            log(
                true,
                format_args!(
                    "display: R2 FAILED {:?}; VP2 remains in standby, serial available\n",
                    error
                ),
            );
            None
        }
    }
}

struct Identity;
impl hdmi::ReadRegisters for Identity {
    fn hdmi(&mut self, offset: u32) -> u32 {
        read(HDMI_D0D, offset)
    }
    fn phy(&mut self, offset: u32) -> u32 {
        read(PHY_R2, offset)
    }
    fn grf(&mut self, offset: u32) -> u32 {
        read(GRF_R2, offset)
    }
}
pub fn verify(power: Power) -> bool {
    let Some(s) = hdmi::Snapshot::read(power, &mut Identity) else {
        return false;
    };
    let hz = s.pixel_hz();
    let ok = hz == Some(hdmi::D0A_PIXEL_HZ)
        && s.tx.timing() == Some(hdmi::D0A_TIMING)
        && s.pll_locked()
        && s.phy_ready()
        && s.phy_clk_ready()
        && s.tx.dvi();
    log(!ok, format_args!("display: R2 final PHY pixel clock {:?} Hz, locked/clock/lanes={}/{}/{}, TX mode match={}\n", hz, u8::from(s.pll_locked()), u8::from(s.phy_clk_ready()), u8::from(s.phy_ready()), u8::from(s.tx.timing() == Some(hdmi::D0A_TIMING))));
    ok
}
