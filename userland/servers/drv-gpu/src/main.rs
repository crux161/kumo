#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]
//j572
//j574

use core::fmt::{self, Write};
use drv_gpu::{
    probe, soft_reset, Completion, ProbeError, ProbeIo, ResetIo, ResetPath, APERTURE_BASE,
    APERTURE_LEN, GPU_IRQ,
};
use kumo_abi::{sys::log_level, DevicePowerStatus, Errno, Handle, Status, VmarFlags};

kumo_rt::entry!(main);

const MMIO_VA: u64 = 0x1100_0000;

/// From G2a on the aperture is mapped writable Device memory: the reset writes GPU_CONTROL.
struct Device {
    resource: Handle,
    interrupt: Option<Handle>,
    port: Option<Handle>,
    timer: Option<Handle>,
}

/// Handle-returning syscalls report failure as a negative status; 0 is never a valid handle.
fn valid_handle(raw: u64) -> Option<Handle> {
    if raw == 0 || (raw as i64) < 0 {
        None
    } else {
        Some(Handle(raw as u32))
    }
}

// SAFETY: every caller runs only after the aperture's Device mapping succeeded, and only uses
// fixed GPU_CONTROL words inside the 2 MiB grant. — TIMBERDOODLE
fn read_aperture(offset: u32) -> u32 {
    unsafe { ((MMIO_VA + u64::from(offset)) as *const u32).read_volatile() }
}

// SAFETY: as `read_aperture`. The dsb orders each store before the next MMIO access, matching
// the kernel's `PhysPowerIo` convention for device registers. — TIMBERDOODLE
fn write_aperture(offset: u32, value: u32) {
    unsafe { ((MMIO_VA + u64::from(offset)) as *mut u32).write_volatile(value) };
    dsb();
}

#[inline(always)]
fn dsb() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}

impl ProbeIo for Device {
    fn power_up(&mut self) -> Status {
        kumo_rt::device_power_up(self.resource, APERTURE_BASE)
    }

    fn power_status(&mut self) -> Result<DevicePowerStatus, Status> {
        let mut record = DevicePowerStatus::default();
        let status = kumo_rt::device_power_status(self.resource, APERTURE_BASE, &mut record);
        log(
            log_level::INFO,
            format_args!(
                "drv-gpu: power status={} version={} flags=0x{:02x}\n",
                status, record.version, record.flags
            ),
        );
        if status == 0 {
            Ok(record)
        } else {
            Err(status)
        }
    }

    fn map_aperture(&mut self) -> Result<(), ProbeError> {
        let raw = kumo_rt::resource_mint_mmio(self.resource, APERTURE_BASE, APERTURE_LEN);
        let Some(vmo) = valid_handle(raw) else {
            return Err(ProbeError::Mint);
        };
        let status = kumo_rt::vmar_map(
            Handle(0),
            vmo,
            0,
            MMIO_VA,
            APERTURE_LEN,
            (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::DEVICE).0,
        ) as Status;
        let _ = kumo_rt::handle_close(vmo);
        if status == 0 {
            Ok(())
        } else {
            Err(ProbeError::Map(status))
        }
    }

    fn read32(&mut self, offset: u32) -> u32 {
        read_aperture(offset)
    }
}

impl ResetIo for Device {
    fn read32(&mut self, offset: u32) -> u32 {
        read_aperture(offset)
    }

    fn write32(&mut self, offset: u32, value: u32) {
        write_aperture(offset, value);
    }

    fn arm_completion(&mut self, timeout_ns: u64) -> Result<(), Status> {
        // Bind the port wait BEFORE the caller issues GPU_CMD: a completion that fired between
        // the command and the bind would mask the SPI with no waiter left to complete it, the
        // same arm-after-assert race drv-xhci lost on metal. — TIMBERDOODLE
        let raw = kumo_rt::interrupt_create(self.resource, GPU_IRQ);
        let Some(interrupt) = valid_handle(raw) else {
            return Err(raw as u32 as i32);
        };
        let Some(port) = valid_handle(kumo_rt::port_create()) else {
            let _ = kumo_rt::handle_close(interrupt);
            return Err(Errno::Internal.status());
        };
        let timer_raw = kumo_rt::timer_create(timeout_ns);
        let Some(timer) = valid_handle(timer_raw) else {
            let _ = kumo_rt::handle_close(port);
            let _ = kumo_rt::handle_close(interrupt);
            return Err(Errno::Internal.status());
        };
        if kumo_rt::port_bind(port, interrupt) != 0 || kumo_rt::port_bind(port, timer) != 0 {
            let _ = kumo_rt::port_unbind(port, interrupt);
            let _ = kumo_rt::port_unbind(port, timer);
            let _ = kumo_rt::handle_close(timer);
            let _ = kumo_rt::handle_close(port);
            let _ = kumo_rt::handle_close(interrupt);
            return Err(Errno::Internal.status());
        }
        self.interrupt = Some(interrupt);
        self.port = Some(port);
        self.timer = Some(timer);
        Ok(())
    }

    fn wait_completion(&mut self) -> Completion {
        let (Some(port), Some(timer), Some(interrupt)) = (self.port, self.timer, self.interrupt)
        else {
            return Completion::TimedOut;
        };
        let timer_koid = kumo_rt::handle_koid(timer);
        let source = kumo_rt::port_wait(port);
        if source == 0 || source == timer_koid {
            return Completion::TimedOut;
        }
        let _ = kumo_rt::interrupt_wait(interrupt);
        Completion::Fired
    }

    fn complete(&mut self) {
        if let Some(port) = self.port.take() {
            if let Some(interrupt) = self.interrupt {
                let _ = kumo_rt::port_unbind(port, interrupt);
            }
            if let Some(timer) = self.timer.take() {
                let _ = kumo_rt::port_unbind(port, timer);
                let _ = kumo_rt::handle_close(timer);
            }
            let _ = kumo_rt::handle_close(port);
        }
        if let Some(interrupt) = self.interrupt.take() {
            let _ = kumo_rt::interrupt_complete(interrupt);
            let _ = kumo_rt::handle_close(interrupt);
        }
    }
}

fn log(level: u8, args: fmt::Arguments<'_>) {
    struct Line {
        bytes: [u8; 256],
        len: usize,
    }
    impl Write for Line {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            let end = self.len + text.len();
            let dest = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
            dest.copy_from_slice(text.as_bytes());
            self.len = end;
            Ok(())
        }
    }
    let mut line = Line {
        bytes: [0; 256],
        len: 0,
    };
    let _ = line.write_fmt(args);
    let _ = kumo_rt::log_write(level, &line.bytes[..line.len]);
}

#[no_mangle]
extern "C" fn main(resource: u64) -> ! {
    let mut device = Device {
        resource: Handle(resource as u32),
        interrupt: None,
        port: None,
        timer: None,
    };
    log(
        log_level::INFO,
        format_args!(
            "drv-gpu: power-up requested, aperture=0x{:08x}\n",
            APERTURE_BASE
        ),
    );
    let exit_code = match probe(&mut device) {
        Ok(identity) => {
            log(
                log_level::NOTICE,
                format_args!(
                    "drv-gpu: online, Mali-G610 GPU_ID=0x{:08x}, {} cores\n",
                    identity.gpu_id,
                    identity.cores()
                ),
            );
            log(
                log_level::INFO,
                format_args!(
                    "drv-gpu: SHADER_PRESENT=0x{:016x}; identity probe complete\n",
                    identity.shader_present
                ),
            );
            match soft_reset(&mut device) {
                Ok(ResetPath::Interrupt) => {
                    log(
                        log_level::NOTICE,
                        format_args!("drv-gpu: soft reset complete: completion interrupt\n"),
                    );
                    0
                }
                Ok(ResetPath::PolledAfterTimeout) => {
                    log(
                        log_level::NOTICE,
                        format_args!("drv-gpu: soft reset complete: rawstat recheck\n"),
                    );
                    0
                }
                Err(error) => {
                    log(
                        log_level::ERR,
                        format_args!("drv-gpu: soft reset failed: {:?}\n", error),
                    );
                    1
                }
            }
        }
        Err(ProbeError::PowerUp(status)) if status == Errno::NotSupported.status() => {
            log(
                log_level::INFO,
                format_args!("drv-gpu: DevicePowerUp NotSupported; aperture untouched\n"),
            );
            0
        }
        Err(ProbeError::NotReady(power)) => {
            log(
                log_level::ERR,
                format_args!(
                    "drv-gpu: aperture refused: power version={} flags=0x{:02x}\n",
                    power.version, power.flags
                ),
            );
            1
        }
        Err(error) => {
            log(
                log_level::ERR,
                format_args!("drv-gpu: probe failed: {:?}\n", error),
            );
            1
        }
    };
    let _ = kumo_rt::handle_close(device.resource);
    kumo_rt::process_exit(exit_code)
}
