#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]
//j893

//! drv-thermal T0 (j893): read the RK3588 TS-ADC once a second, log it, and answer the shell's
//! `thermal` word. x0 is the TS-ADC child Resource (WRITE: the start sequence writes three
//! hi-word-masked words); x1 is the bootstrap channel, whose tagged messages carry the read-only
//! CRU, pad and PWM views and which then carries status requests. The fan is not touched; the
//! pad and PWM reads are diagnostics for T1. — TEAL

use core::fmt::{self, Write};
use drv_thermal::{
    notice_due, pwm, start_auto, Clocks, Found, Line, Regs, Sample, Status, BOOT_BUS_IOC, BOOT_CRU,
    BOOT_GO, BOOT_PWM, BOOT_VCCIO3_5_IOC, BUS_IOC_BASE, BUS_IOC_GPIO3B_IOMUX_SEL_H, CRU_BASE,
    CRU_LEN, FLAG_CLOCK_FAULT, FLAG_SENSOR_OK, PAGE_LEN, PWM_BASE, REPORT_EVERY_SAMPLES,
    SAMPLE_PERIOD_NS, STATUS_REQUEST, TSADC_BASE, TSADC_LEN, VCCIO3_5_IOC_BASE,
    VCCIO3_5_IOC_GPIO3B_P,
};
use kumo_abi::sys::log_level;
use kumo_abi::{Handle, VmarFlags};

kumo_rt::entry!(main);

/// Fixed driver-private mappings, one page each, inside the child's 0..512 MiB VMAR.
const TSADC_VA: u64 = 0x1000_0000;
const CRU_VA: u64 = 0x1001_0000;
const BUS_IOC_VA: u64 = 0x1002_0000;
const VCCIO_VA: u64 = 0x1003_0000;
const PWM_VA: u64 = 0x1004_0000;

/// After the start sequence the first round needs seven conversions; poll for it this often,
/// this many times, before giving up on a complete first reading.
const FIRST_SAMPLE_POLL_NS: u64 = 100_000_000;
const FIRST_SAMPLE_POLLS: u32 = 20;

fn valid_handle(raw: u64) -> Option<Handle> {
    kumo_rt::syscall_handle(raw)
}

fn log(level: u8, args: fmt::Arguments<'_>) {
    let mut line = Line::<256>::new();
    let _ = line.write_fmt(args);
    let _ = kumo_rt::log_write(level, line.as_bytes());
}

/// Mint `[base, base + len)` from `resource` and map it at `va` as device memory.
fn map(resource: Handle, base: u64, len: u64, va: u64, writable: bool) -> bool {
    let raw = kumo_rt::resource_mint_mmio(resource, base, len);
    let Some(vmo) = valid_handle(raw) else {
        log(
            log_level::ERR,
            format_args!(
                "drv-thermal: MMIO mint base=0x{base:x} len=0x{len:x} errno {}\n",
                raw as u32 as i32
            ),
        );
        return false;
    };
    let mut flags = VmarFlags::READ | VmarFlags::DEVICE;
    if writable {
        flags = flags | VmarFlags::WRITE;
    }
    let status = kumo_rt::vmar_map(Handle(0), vmo, 0, va, len, flags.0);
    let _ = kumo_rt::handle_close(vmo);
    if status != 0 {
        log(
            log_level::ERR,
            format_args!(
                "drv-thermal: MMIO map base=0x{base:x} va=0x{va:x} len=0x{len:x} errno {}\n",
                status as u32 as i32
            ),
        );
    }
    status == 0
}

fn read32(va: u64, offset: u32) -> u32 {
    // SAFETY: `va` is one of this driver's device mappings and `offset` lies inside its page.
    unsafe { core::ptr::read_volatile((va + u64::from(offset)) as *const u32) }
}

struct Tsadc;

impl Regs for Tsadc {
    fn read(&mut self, offset: u32) -> u32 {
        read32(TSADC_VA, offset)
    }

    fn write(&mut self, offset: u32, value: u32) {
        // SAFETY: the TS-ADC page is mapped writable at TSADC_VA; offsets come from the lib's
        // register table.
        unsafe { core::ptr::write_volatile((TSADC_VA + u64::from(offset)) as *mut u32, value) };
    }
}

/// The grants Sora queued on the bootstrap channel, read up to `BOOT_GO`.
#[derive(Default)]
struct Grants {
    cru: Option<Handle>,
    bus_ioc: Option<Handle>,
    vccio: Option<Handle>,
    pwm: Option<Handle>,
}

fn read_grants(channel: Handle) -> Option<Grants> {
    let mut grants = Grants::default();
    loop {
        let mut tag = [0u8; 1];
        let (len, raw) = kumo_rt::channel_read_with_handle(channel, tag.as_mut_ptr(), 1);
        if len != 1 {
            return None;
        }
        let handle = valid_handle(raw);
        let slot = match tag[0] {
            BOOT_GO => return Some(grants),
            BOOT_CRU => &mut grants.cru,
            BOOT_BUS_IOC => &mut grants.bus_ioc,
            BOOT_VCCIO3_5_IOC => &mut grants.vccio,
            BOOT_PWM => &mut grants.pwm,
            _ => {
                if let Some(handle) = handle {
                    let _ = kumo_rt::handle_close(handle);
                }
                continue;
            }
        };
        *slot = handle;
    }
}

/// Log what T1 will change, as found: the fan pins' mux and pulls and PWM12/13's registers. The
/// PWM block is read only when the CRU says its clocks run (a gated APB block can hang the bus).
fn report_fan_side(grants: &Grants, clocks: &Clocks) {
    let iomux = grants
        .bus_ioc
        .filter(|h| map(*h, BUS_IOC_BASE, PAGE_LEN, BUS_IOC_VA, false))
        .map(|_| read32(BUS_IOC_VA, BUS_IOC_GPIO3B_IOMUX_SEL_H));
    let pull = grants
        .vccio
        .filter(|h| map(*h, VCCIO3_5_IOC_BASE, PAGE_LEN, VCCIO_VA, false))
        .map(|_| read32(VCCIO_VA, VCCIO3_5_IOC_GPIO3B_P));
    match (iomux, pull) {
        (Some(iomux), Some(pull)) => log(
            log_level::INFO,
            format_args!(
                "drv-thermal: fan pads GPIO3B_IOMUX_SEL_H=0x{iomux:08x} (B5 func {} B6 func {}) \
                 GPIO3B_P=0x{pull:08x} (B6 pull {})\n",
                (iomux >> 4) & 0xf,
                (iomux >> 8) & 0xf,
                match (pull >> 12) & 0b11 {
                    0b11 => "up",
                    0b01 => "down",
                    _ => "off",
                }
            ),
        ),
        _ => log(
            log_level::INFO,
            format_args!("drv-thermal: fan pads not readable\n"),
        ),
    }
    if !clocks.pwm3_live() {
        log(
            log_level::INFO,
            format_args!("drv-thermal: PWM block 3 clocks off; PWM12/13 not read\n"),
        );
        return;
    }
    if grants
        .pwm
        .is_some_and(|h| map(h, PWM_BASE, PAGE_LEN, PWM_VA, false))
    {
        log(
            log_level::INFO,
            format_args!(
                "drv-thermal: PWM version=0x{:08x} pwm12 ctrl=0x{:08x} period={} duty={} \
                 pwm13 ctrl=0x{:08x} intsts=0x{:08x}\n",
                read32(PWM_VA, pwm::VERSION),
                read32(PWM_VA, pwm::ctrl(0)),
                read32(PWM_VA, pwm::period(0)),
                read32(PWM_VA, pwm::duty(0)),
                read32(PWM_VA, pwm::ctrl(1)),
                read32(PWM_VA, pwm::INTSTS),
            ),
        );
    }
}

/// Bring the TS-ADC up; `false` leaves it untouched (clock fault or no mapping).
fn start_sensor(resource: Handle, clocks: &Clocks) -> bool {
    log(
        log_level::INFO,
        format_args!(
            "drv-thermal: cru clksel41=0x{:08x} ({} Hz) gate12=0x{:08x} softrst12=0x{:08x} \
             clksel60=0x{:08x} gate15=0x{:08x} softrst15=0x{:08x} glb_rst=0x{:08x} \
             (tshut->reset {})\n",
            clocks.clksel41,
            clocks.tsadc_hz().unwrap_or(0),
            clocks.gate12,
            clocks.softrst12,
            clocks.clksel60,
            clocks.gate15,
            clocks.softrst15,
            clocks.glb_rst,
            if clocks.tshut_reset_armed() {
                "armed"
            } else {
                "not armed"
            },
        ),
    );
    if !clocks.tsadc_ready() {
        log(
            log_level::ERR,
            format_args!("drv-thermal: TS-ADC clock not ready; sensor left untouched\n"),
        );
        return false;
    }
    if !map(resource, TSADC_BASE, TSADC_LEN, TSADC_VA, true) {
        log(
            log_level::ERR,
            format_args!("drv-thermal: TS-ADC map failed\n"),
        );
        return false;
    }
    let found = Found::read(&mut Tsadc);
    let programmed = start_auto(&mut Tsadc, &found);
    log(
        log_level::INFO,
        format_args!(
            "drv-thermal: tsadc found auto_con=0x{:08x} status=0x{:08x} src=0x{:08x} \
             gpio_en=0x{:08x} cru_en=0x{:08x} period=0x{:08x} shut0=0x{:08x}; {}\n",
            found.auto_con,
            found.auto_status,
            found.auto_src,
            found.gpio_en,
            found.cru_en,
            found.period,
            found.shut0,
            if programmed {
                "auto mode started on 7 channels"
            } else {
                "already converting, kept"
            },
        ),
    );
    true
}

fn temperatures_line(status: &Status) -> Line<200> {
    let mut line = Line::<200>::new();
    let _ = status.write_temperatures(&mut line);
    line
}

struct Driver {
    channel: Option<Handle>,
    sensor: bool,
    status: Status,
    last_noticed: Option<i16>,
}

impl Driver {
    fn sample(&mut self) {
        if !self.sensor {
            return;
        }
        let sample = Sample::read(&mut Tsadc);
        self.status.set_sample(&sample);
        self.status.samples = self.status.samples.wrapping_add(1);
        if sample.complete() {
            self.status.flags |= FLAG_SENSOR_OK;
        } else {
            self.status.flags &= !FLAG_SENSOR_OK;
        }
        let max = self.status.max();
        if notice_due(self.last_noticed, max) {
            self.last_noticed = max;
            let line = temperatures_line(&self.status);
            log(
                log_level::NOTICE,
                format_args!(
                    "thermal: {}\n",
                    core::str::from_utf8(line.as_bytes()).unwrap_or("?")
                ),
            );
        } else if self.status.samples % REPORT_EVERY_SAMPLES == 0 {
            let line = temperatures_line(&self.status);
            log(
                log_level::INFO,
                format_args!(
                    "thermal: {}\n",
                    core::str::from_utf8(line.as_bytes()).unwrap_or("?")
                ),
            );
        }
    }

    /// One request off the channel; `false` when the channel is gone.
    fn answer(&mut self, channel: Handle) -> bool {
        let mut request = [0u8; 8];
        let n = kumo_rt::channel_read(channel, request.as_mut_ptr(), request.len());
        if n == 0 || n > request.len() as u64 {
            return false;
        }
        if n == 1 && request[0] == STATUS_REQUEST {
            let record = self.status.encode();
            let _ = kumo_rt::channel_write(channel, record.as_ptr(), record.len());
        }
        true
    }

    fn serve(mut self) -> ! {
        let Some(port) = valid_handle(kumo_rt::port_create()) else {
            log(log_level::ERR, format_args!("drv-thermal: no port\n"));
            kumo_rt::process_exit(1);
        };
        let mut channel_koid = u64::MAX;
        if let Some(channel) = self.channel {
            if kumo_rt::port_bind(port, channel) == 0 {
                channel_koid = kumo_rt::handle_koid(channel);
            } else {
                self.channel = None;
            }
        }
        loop {
            let Some(timer) = valid_handle(kumo_rt::timer_create(SAMPLE_PERIOD_NS)) else {
                log(log_level::ERR, format_args!("drv-thermal: no timer\n"));
                kumo_rt::process_exit(1);
            };
            let timer_koid = kumo_rt::handle_koid(timer);
            if kumo_rt::port_bind(port, timer) != 0 {
                log(
                    log_level::ERR,
                    format_args!("drv-thermal: timer bind failed\n"),
                );
                kumo_rt::process_exit(1);
            }
            loop {
                let source = kumo_rt::port_wait(port);
                if source == timer_koid {
                    break;
                }
                if source == channel_koid {
                    if let Some(channel) = self.channel {
                        if !self.answer(channel) {
                            let _ = kumo_rt::port_unbind(port, channel);
                            let _ = kumo_rt::handle_close(channel);
                            self.channel = None;
                            channel_koid = u64::MAX;
                        }
                    }
                }
            }
            let _ = kumo_rt::port_unbind(port, timer);
            let _ = kumo_rt::handle_close(timer);
            self.sample();
        }
    }
}

#[no_mangle]
extern "C" fn main(resource: u64, bootstrap: u64) -> ! {
    let resource = Handle(resource as u32);
    let Some(channel) = valid_handle(bootstrap) else {
        log(
            log_level::ERR,
            format_args!("drv-thermal: no bootstrap channel\n"),
        );
        kumo_rt::process_exit(1);
    };
    let Some(grants) = read_grants(channel) else {
        log(
            log_level::ERR,
            format_args!("drv-thermal: bootstrap ended early\n"),
        );
        kumo_rt::process_exit(1);
    };
    let mut status = Status::default();
    let clocks = grants
        .cru
        .filter(|h| map(*h, CRU_BASE, CRU_LEN, CRU_VA, false))
        .map(|_| Clocks::read(|offset| read32(CRU_VA, offset)));
    let sensor = match clocks {
        Some(clocks) => {
            let up = start_sensor(resource, &clocks);
            report_fan_side(&grants, &clocks);
            up
        }
        None => {
            log(
                log_level::ERR,
                format_args!("drv-thermal: no CRU view; sensor left untouched\n"),
            );
            false
        }
    };
    if !sensor {
        status.flags |= FLAG_CLOCK_FAULT;
    }
    let mut driver = Driver {
        channel: Some(channel),
        sensor,
        status,
        last_noticed: None,
    };
    if sensor {
        let mut first = Sample::read(&mut Tsadc);
        let mut polls = 0;
        while !first.complete() && polls < FIRST_SAMPLE_POLLS {
            let _ = kumo_rt::sleep_ns(FIRST_SAMPLE_POLL_NS);
            first = Sample::read(&mut Tsadc);
            polls += 1;
        }
        driver.status.set_sample(&first);
        driver.status.samples = 1;
        let line = temperatures_line(&driver.status);
        let text = core::str::from_utf8(line.as_bytes()).unwrap_or("?");
        if first.complete() {
            driver.status.flags |= FLAG_SENSOR_OK;
            kumo_rt::boot_ok_fmt(format_args!("drv-thermal: TS-ADC online, {text}\n"));
        } else {
            log(
                log_level::ERR,
                format_args!("drv-thermal: TS-ADC incomplete after start: {text}\n"),
            );
        }
        driver.last_noticed = driver.status.max();
    }
    driver.serve()
}
