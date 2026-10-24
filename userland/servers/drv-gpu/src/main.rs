#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]
//j572

use core::fmt::{self, Write};
use drv_gpu::{probe, ProbeError, ProbeIo, APERTURE_BASE, APERTURE_LEN};
use kumo_abi::{sys::log_level, DevicePowerStatus, Errno, Handle, Status, VmarFlags};

kumo_rt::entry!(main);

const MMIO_VA: u64 = 0x1100_0000;

struct Device {
    resource: Handle,
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
        if raw == u64::MAX {
            return Err(ProbeError::Mint);
        }
        let vmo = Handle(raw as u32);
        let status = kumo_rt::vmar_map(Handle(0), vmo, 0, MMIO_VA, APERTURE_LEN, VmarFlags::READ.0)
            as Status;
        let _ = kumo_rt::handle_close(vmo);
        if status == 0 {
            Ok(())
        } else {
            Err(ProbeError::Map(status))
        }
    }

    fn read32(&mut self, offset: u32) -> u32 {
        // SAFETY: probe calls only after a successful Device-memory mapping, with aligned
        // read-only identity offsets inside that mapping. — KESTREL
        unsafe { ((MMIO_VA + u64::from(offset)) as *const u32).read_volatile() }
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
            0
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
