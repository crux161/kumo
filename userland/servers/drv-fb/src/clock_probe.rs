//j599
//j601

//! D0c live adapter: one read-only CRU mapping, one fixed sample, one NOTICE
//! inventory. There is no write path: [`Registers`] implements only
//! [`ReadCru::cru`], the mapping is `VmarFlags::READ`, and the child Resource
//! carries no IRQs.

use core::fmt::{self, Write};
use drv_fb::probe_va::CRU_D0C;
use drv_fb::rk3588_clock::{
    clock_name, decode, implied_ratio, parent_name, unknown_name, ClockId, ClockRecord, Grant,
    ImpliedRatio, Rate, ReadCru, Sample, CRU_LEN, FLAG_FRACTIONAL, KIND_PLL,
};
use kumo_abi::{Handle, VmarFlags};
use kumo_rt::{handle_close, resource_mint_mmio, syscall_handle, vmar_map};

struct Log {
    bytes: [u8; 384],
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
        bytes: [0; 384],
        len: 0,
    };
    if line.write_fmt(args).is_ok() {
        let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &line.bytes[..line.len]);
    }
}

struct Registers;

impl ReadCru for Registers {
    fn cru(&mut self, offset: u32) -> u32 {
        assert!(offset % 4 == 0 && u64::from(offset) + 4 <= CRU_LEN);
        unsafe { core::ptr::read_volatile((CRU_D0C + u64::from(offset)) as *const u32) }
    }
}

fn map(resource: Handle, base: u64, len: u64, va: u64) -> bool {
    if resource.0 == 0 {
        return false;
    }
    let raw = resource_mint_mmio(resource, base, len);
    let Some(vmo) = syscall_handle(raw) else {
        log(format_args!(
            "display: CRU mint failed (errno {})\n",
            raw as u32 as i32
        ));
        return false;
    };
    // Device memory is the default for an MMIO VMO; READ gives an EL0 read-only
    // mapping. The process never retains a writable mapping of the CRU.
    // — TIMBERDOODLE
    let ok = vmar_map(Handle(0), vmo, 0, va, len, VmarFlags::READ.0) == 0;
    let _ = handle_close(vmo);
    ok
}

fn log_record(record: &ClockRecord) {
    let gate = if !record.gate_watched() {
        "n/a"
    } else if record.gate_disabled() {
        "off"
    } else {
        "on"
    };
    let reset = if !record.reset_watched() {
        "n/a"
    } else if record.reset_asserted() {
        "asserted"
    } else {
        "released"
    };
    match record.rate() {
        Rate::Hz(hz) if record.kind == KIND_PLL => {
            log(format_args!(
                "display: clk {} parent={} div={} rate={} Hz gate={} reset={} M={} K={}{}\n",
                clock_name(record.id),
                parent_name(record.parent),
                record.divider,
                hz,
                gate,
                reset,
                record.detail & 0x3ff,
                (record.detail >> 10) & 0xffff,
                if record.flags & FLAG_FRACTIONAL != 0 {
                    " frac"
                } else {
                    ""
                }
            ));
        }
        Rate::Hz(hz) => {
            log(format_args!(
                "display: clk {} parent={} div={} rate={} Hz gate={} reset={} raw={:#010x}\n",
                clock_name(record.id),
                parent_name(record.parent),
                record.divider,
                hz,
                gate,
                reset,
                record.detail
            ));
        }
        Rate::Unknown(reason) => {
            log(format_args!(
                "display: clk {} parent={} div={} rate=unknown({}) gate={} reset={} raw={:#010x}\n",
                clock_name(record.id),
                parent_name(record.parent),
                record.divider,
                unknown_name(reason as u8),
                gate,
                reset,
                record.detail
            ));
        }
    }
}

/// Report the D0c inventory. `grant` is decoded before any graphics reads so a
/// malformed record can never select a mapping; `resource` is closed on every
/// path.
pub fn report_clock_probe(grant: Option<Grant>, resource: Handle) {
    let Some(grant) = grant else {
        log(format_args!(
            "display: D0c clock grant absent or malformed; zero CRU reads\n"
        ));
        if resource.0 != 0 {
            let _ = handle_close(resource);
        }
        return;
    };
    if resource.0 == 0 {
        log(format_args!(
            "display: D0c clock grant data without a Resource; zero CRU reads\n"
        ));
        return;
    }
    log(format_args!(
        "display: D0c CRU clock inventory base={:#x} len={:#x} (Device/READ; IRQs 0)\n",
        grant.base, grant.len
    ));
    if !map(
        resource,
        u64::from(grant.base),
        u64::from(grant.len),
        CRU_D0C,
    ) {
        log(format_args!(
            "display: D0c CRU mapping failed; probe skipped\n"
        ));
        let _ = handle_close(resource);
        return;
    }

    let inventory = decode(&Sample::read(&mut Registers));
    for record in &inventory.records {
        log_record(record);
    }

    match inventory.get(ClockId::DclkVp2).rate() {
        Rate::Hz(hz) => {
            log(format_args!(
                "display: pixel clock = {} Hz (derived dclk_vp2; not measured)\n",
                hz
            ));
            match implied_ratio(hz) {
                ImpliedRatio::Equal => log(format_args!(
                    "display: dclk_vp2 equals 108000000 Hz implied by 1600x900@60 (arithmetic only; refresh not measured)\n"
                )),
                ImpliedRatio::Times(n) => log(format_args!(
                    "display: dclk_vp2 is {}x the 108000000 Hz implied by 1600x900@60 (arithmetic only; refresh not measured)\n",
                    n
                )),
                ImpliedRatio::Divisor(n) => log(format_args!(
                    "display: dclk_vp2 is 1/{} of the 108000000 Hz implied by 1600x900@60 (arithmetic only; refresh not measured)\n",
                    n
                )),
                ImpliedRatio::Other => log(format_args!(
                    "display: dclk_vp2 has no integer ratio to the 108000000 Hz implied by 1600x900@60 (refresh not measured)\n"
                )),
            }
        }
        Rate::Unknown(reason) => log(format_args!(
            "display: pixel clock unknown ({}); refresh not measured\n",
            unknown_name(reason as u8)
        )),
    }

    let _ = handle_close(resource);
}
