#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]
//j893
//j894

//! j894: seven sensors, PWM12 fan curve and PWM13 tach. Shared IOC authority stays in Sora.
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};
use drv_thermal::{
    fan::{self, Controller, State, Tach},
    notice_due, pwm, start_auto, Clocks, Found, Line, Regs, Sample, Status, BOOT_CRU, BOOT_GO,
    BOOT_PWM, BOOT_TEST_CYCLE, BOOT_TEST_EXIT, BOOT_TEST_PANIC, CRU_BASE, CRU_LEN,
    FLAG_CLOCK_FAULT, FLAG_FAILSAFE, FLAG_FAN_DRIVEN, FLAG_SENSOR_OK, FLAG_STALL, PAGE_LEN,
    PWM_BASE, REPORT_EVERY_SAMPLES, SAMPLE_PERIOD_NS, STATUS_REQUEST, TSADC_BASE, TSADC_LEN,
};
use kumo_abi::sys::log_level;
use kumo_abi::{Handle, VmarFlags};

kumo_rt::entry!(main);

/// Fixed driver-private mappings, one page each, inside the child's 0..512 MiB VMAR.
const TSADC_VA: u64 = 0x1000_0000;
const CRU_VA: u64 = 0x1001_0000;
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
    proof: u8,
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
            BOOT_TEST_CYCLE => {
                grants.proof = 1;
                continue;
            }
            BOOT_TEST_EXIT => {
                grants.proof = 3;
                continue;
            }
            BOOT_TEST_PANIC => {
                grants.proof = 2;
                continue;
            }
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

struct Pwm;
impl Regs for Pwm {
    fn read(&mut self, offset: u32) -> u32 {
        read32(PWM_VA, offset)
    }
    fn write(&mut self, offset: u32, value: u32) {
        unsafe { ((PWM_VA + u64::from(offset)) as *mut u32).write_volatile(value) };
    }
}
static PWM_MAPPED: AtomicBool = AtomicBool::new(false);

fn full_on_exit() {
    if PWM_MAPPED.load(Ordering::Acquire) && Clocks::read(|o| read32(CRU_VA, o)).pwm3_live() {
        fan::set_duty(&mut Pwm, fan::FULL);
        let ok = read32(PWM_VA, pwm::duty(0)) == fan::PERIOD;
        let _ = kumo_rt::log_write(
            log_level::NOTICE,
            if ok {
                b"thermal: exit fail-safe 100%, PWM duty readback=960\n"
            } else {
                b"thermal: exit fail-safe PWM readback failed\n"
            },
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

fn temperatures_line(status: &Status) -> Line<240> {
    let mut line = Line::<240>::new();
    let _ = status.write_temperatures(&mut line);
    let state = if status.flags & FLAG_CLOCK_FAULT != 0 {
        "clock fault"
    } else if status.flags & FLAG_STALL != 0 {
        "stall"
    } else if status.flags & FLAG_FAILSAFE != 0 {
        "sensor fault"
    } else {
        "auto"
    };
    let _ = write!(
        line,
        " fan {}% {} rpm {state}",
        status.duty_permille / 10,
        status.rpm
    );
    line
}

struct Driver {
    channel: Option<Handle>,
    sensor: bool,
    cru: bool,
    pwm: bool,
    controller: Controller,
    proof: u8,
    status: Status,
    last_noticed: Option<i16>,
}

impl Driver {
    fn sample(&mut self) {
        let clocks = self.cru.then(|| Clocks::read(|o| read32(CRU_VA, o)));
        let sensor_live = self.sensor && clocks.is_some_and(|c| c.tsadc_ready());
        let sample = if sensor_live {
            Sample::read(&mut Tsadc)
        } else {
            Sample::default()
        };
        let running = sensor_live && Found::read(&mut Tsadc).usable();
        let pwm_live = self.pwm && clocks.is_some_and(|c| c.pwm3_live());
        let config_ok = pwm_live
            && read32(PWM_VA, pwm::ctrl(0)) == fan::CTRL_RUN
            && read32(PWM_VA, pwm::period(0)) == fan::PERIOD
            && read32(PWM_VA, pwm::ctrl(1)) == fan::CTRL_CAPTURE;
        let clocks_ok =
            sensor_live && config_ok && clocks.is_some_and(|c| c.pwm3_hz() == Some(fan::CLOCK_HZ));
        let tach = if pwm_live {
            fan::read_tach(&mut Pwm)
        } else {
            Tach::default()
        };
        self.status.samples = self.status.samples.wrapping_add(1);
        let n = self.status.samples;
        let old_duty = self.status.duty_permille;
        let old_flags = self.status.flags;
        self.status.set_sample(&sample);
        self.status.rpm = tach.rpm;
        let mut input = sample;
        let mut sensor_ok = running;
        let mut input_tach = tach;
        // Bounded proof-image inputs exercise the production controller, with real capture and
        // PWM readback. Real faults and >=75 C always override the proof. — KESTREL
        if self.proof == 1
            && n <= 55
            && clocks_ok
            && sample.complete()
            && sample.max().is_some_and(|m| m < fan::RAMP_END_MC)
        {
            let mc = match n {
                1..=4 => 75_000,
                5..=9 => 62_500,
                _ => 50_000,
            };
            input = Sample {
                millicelsius: [Some(mc); 7],
            };
            if (17..=20).contains(&n) {
                sensor_ok = false;
            }
            if (21..=25).contains(&n) {
                input_tach = Tach::default();
            }
        }
        if self.proof == 1 && n == 56 {
            self.controller = Controller::new(kumo_rt::clock_get());
        }
        let policy_clocks_ok = clocks_ok && !(self.proof == 1 && (30..=33).contains(&n));
        let output = self.controller.step(
            kumo_rt::clock_get(),
            input,
            sensor_ok,
            policy_clocks_ok,
            input_tach,
        );
        self.status.flags = if running && sample.complete() && output.state != State::SensorFault {
            FLAG_SENSOR_OK
        } else {
            0
        };
        if pwm_live {
            self.status.flags |= FLAG_FAN_DRIVEN;
        }
        if output.state != State::Auto {
            self.status.flags |= FLAG_FAILSAFE;
        }
        if output.state == State::Stall {
            self.status.flags |= FLAG_STALL;
        }
        if output.state == State::ClockFault {
            self.status.flags |= FLAG_CLOCK_FAULT;
        }
        self.status.duty_permille = output.duty;
        if pwm_live {
            fan::set_duty(&mut Pwm, output.duty);
            if read32(PWM_VA, pwm::duty(0)) != u32::from(output.duty) * fan::PERIOD / 1000 {
                fan::set_duty(&mut Pwm, fan::FULL);
                self.status.duty_permille = fan::FULL;
                self.status.flags |= FLAG_FAILSAFE | FLAG_CLOCK_FAULT;
            }
        }
        let changed =
            old_flags != self.status.flags || old_duty.abs_diff(self.status.duty_permille) >= 50;
        let max = self.status.max();
        if changed
            || output.retry
            || notice_due(self.last_noticed, max)
            || (self.proof != 0 && n <= 60)
            || n % REPORT_EVERY_SAMPLES == 0
        {
            let line = temperatures_line(&self.status);
            log(
                if changed || notice_due(self.last_noticed, max) {
                    log_level::NOTICE
                } else {
                    log_level::INFO
                },
                format_args!(
                    "thermal: {}\n",
                    core::str::from_utf8(line.as_bytes()).unwrap_or("?")
                ),
            );
            if self.proof != 0 && pwm_live {
                log(log_level::INFO, format_args!("thermal proof: sample={n} duty={} rpm={} state={} pwm_period={} pwm_duty={} capture_h={} capture_l={}\n",
                    self.status.duty_permille, self.status.rpm, output.state.name(),
                    read32(PWM_VA,pwm::period(0)),read32(PWM_VA,pwm::duty(0)),
                    read32(PWM_VA,pwm::period(1)),read32(PWM_VA,pwm::duty(1))));
            }
            if changed || notice_due(self.last_noticed, max) {
                self.last_noticed = max;
            }
        }
        if self.proof == 3 && n == 8 {
            kumo_rt::process_exit(0);
        }
        if self.proof == 2 && n == 8 {
            panic!("thermal fail-safe proof");
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
    let clocks = grants
        .cru
        .filter(|h| map(*h, CRU_BASE, CRU_LEN, CRU_VA, false))
        .map(|_| Clocks::read(|o| read32(CRU_VA, o)));
    let pwm = clocks.is_some_and(|c| c.pwm3_live() && c.pwm3_hz() == Some(fan::CLOCK_HZ))
        && grants
            .pwm
            .is_some_and(|h| map(h, PWM_BASE, PAGE_LEN, PWM_VA, true));
    if pwm {
        PWM_MAPPED.store(true, Ordering::Release);
        kumo_rt::set_exit_hook(Some(full_on_exit));
        fan::start_full(&mut Pwm);
        fan::start_capture(&mut Pwm);
        log(
            log_level::NOTICE,
            format_args!("thermal: PWM12 25kHz, startup 100%; PWM13 tach capture\n"),
        );
    }
    let sensor = clocks.is_some_and(|c| start_sensor(resource, &c));
    if !sensor {
        log(
            log_level::ERR,
            format_args!("drv-thermal: no live sensor; fan stays full\n"),
        );
    }
    if sensor {
        for _ in 0..FIRST_SAMPLE_POLLS {
            if Sample::read(&mut Tsadc).complete() {
                break;
            }
            let _ = kumo_rt::sleep_ns(FIRST_SAMPLE_POLL_NS);
        }
    }
    let mut driver = Driver {
        channel: Some(channel),
        sensor,
        cru: clocks.is_some(),
        pwm,
        controller: Controller::new(kumo_rt::clock_get()),
        proof: grants.proof,
        status: Status {
            duty_permille: fan::FULL,
            ..Status::default()
        },
        last_noticed: None,
    };
    driver.sample();
    if driver.status.flags & FLAG_SENSOR_OK != 0 {
        let line = temperatures_line(&driver.status);
        kumo_rt::boot_ok_fmt(format_args!(
            "drv-thermal: TS-ADC online, {}\n",
            core::str::from_utf8(line.as_bytes()).unwrap_or("?")
        ));
    }
    driver.serve()
}
