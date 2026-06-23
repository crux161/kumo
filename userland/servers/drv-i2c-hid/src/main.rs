#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

use drv_i2c_hid::{bounded_report_descriptor_len, ProbeConfig, MAX_REPORT_DESCRIPTOR_BYTES};
use kumo_abi::{Handle, VmarFlags};
use kumo_i2c_hid::{find_boot_keyboard, Controller, HidDescriptor, RegisterIo};
use kumo_rt::{channel_read_with_handle, debug_write, resource_mint_mmio, vmar_map};

kumo_rt::entry!(main);

const MMIO_VA: u64 = 0x0000_0000_1100_0000;
const POLL_LIMIT: usize = 1_000_000;

struct MmioRegisters {
    base: *mut u8,
}

impl RegisterIo for MmioRegisters {
    fn read(&mut self, offset: u32) -> u32 {
        unsafe { self.base.add(offset as usize).cast::<u32>().read_volatile() }
    }

    fn write(&mut self, offset: u32, value: u32) {
        unsafe {
            self.base
                .add(offset as usize)
                .cast::<u32>()
                .write_volatile(value)
        }
    }
}

fn log(message: &[u8]) {
    debug_write(message.as_ptr(), message.len());
}

fn log_hex(label: &[u8], mut value: u64) {
    log(label);
    let mut digits = [0u8; 16];
    let mut start = digits.len();
    loop {
        start -= 1;
        let digit = (value & 0xf) as u8;
        digits[start] = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        };
        value >>= 4;
        if value == 0 {
            break;
        }
    }
    log(&digits[start..]);
    log(b"\n");
}

#[no_mangle]
extern "C" fn main(
    _arg0: u64,
    bootstrap_channel: u64,
    _arg2: u64,
    _arg3: u64,
    _arg4: u64,
    _arg5: u64,
    _arg6: u64,
    _arg7: u64,
) -> ! {
    log(b"drv-i2c-hid: starting\n");
    let bootstrap = Handle(bootstrap_channel as u32);
    let mut raw = [0u8; ProbeConfig::BYTES];
    let (received, resource_raw) = channel_read_with_handle(bootstrap, raw.as_mut_ptr(), raw.len());
    if received != raw.len() || resource_raw == 0 {
        log(b"drv-i2c-hid: bootstrap failed\n");
        kumo_rt::process_exit(1);
    }
    let config = match ProbeConfig::decode(&raw) {
        Ok(config) => config,
        Err(_) => {
            log(b"drv-i2c-hid: config invalid\n");
            kumo_rt::process_exit(1);
        }
    };
    log(b"drv-i2c-hid: config ok\n");

    let resource = Handle(resource_raw as u32);
    let vmo_raw = resource_mint_mmio(resource, config.mmio_base, config.mmio_length);
    if vmo_raw == u64::MAX {
        log(b"drv-i2c-hid: MMIO mint failed\n");
        kumo_rt::process_exit(1);
    }
    if vmar_map(
        Handle(0),
        Handle(vmo_raw as u32),
        0,
        MMIO_VA,
        config.mmio_length,
        (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::DEVICE).0,
    ) != 0
    {
        log(b"drv-i2c-hid: MMIO map failed\n");
        kumo_rt::process_exit(1);
    }
    log(b"drv-i2c-hid: MMIO mapped\n");

    let registers = MmioRegisters {
        base: MMIO_VA as *mut u8,
    };
    let mut controller = match Controller::new(registers, config.source_clock, POLL_LIMIT) {
        Ok(controller) => controller,
        Err(error) => {
            log_hex(b"drv-i2c-hid: GENI init error=0x", error as u64);
            kumo_rt::process_exit(1);
        }
    };
    log(b"drv-i2c-hid: GENI FIFO ready\n");

    let mut raw_descriptor = [0u8; HidDescriptor::BYTES];
    if let Err(error) = controller.write_read(
        config.i2c_address,
        &config.hid_descriptor_register.to_le_bytes(),
        &mut raw_descriptor,
    ) {
        log_hex(b"drv-i2c-hid: transfer error=0x", error as u64);
        kumo_rt::process_exit(1);
    }
    let descriptor = match HidDescriptor::parse(&raw_descriptor) {
        Ok(descriptor) => descriptor,
        Err(error) => {
            log_hex(b"drv-i2c-hid: descriptor error=0x", error as u64);
            kumo_rt::process_exit(1);
        }
    };

    log(b"drv-i2c-hid: descriptor ok\n");
    log_hex(b"drv-i2c-hid: vendor=0x", descriptor.vendor_id as u64);
    log_hex(b"drv-i2c-hid: product=0x", descriptor.product_id as u64);
    log_hex(
        b"drv-i2c-hid: report-len=0x",
        descriptor.report_descriptor_length as u64,
    );
    log_hex(
        b"drv-i2c-hid: input-reg=0x",
        descriptor.input_register as u64,
    );
    log_hex(
        b"drv-i2c-hid: max-input=0x",
        descriptor.max_input_length as u64,
    );

    let report_descriptor_len =
        match bounded_report_descriptor_len(descriptor.report_descriptor_length) {
            Ok(length) => length,
            Err(error) => {
                log_hex(
                    b"drv-i2c-hid: report descriptor length error=0x",
                    error as u64,
                );
                kumo_rt::process_exit(1);
            }
        };
    let mut report_descriptor = [0u8; MAX_REPORT_DESCRIPTOR_BYTES];
    if let Err(error) = controller.write_read(
        config.i2c_address,
        &descriptor.report_descriptor_register.to_le_bytes(),
        &mut report_descriptor[..report_descriptor_len],
    ) {
        log_hex(
            b"drv-i2c-hid: report descriptor transfer error=0x",
            error as u64,
        );
        kumo_rt::process_exit(1);
    }
    let keyboard = match find_boot_keyboard(&report_descriptor[..report_descriptor_len]) {
        Ok(keyboard) => keyboard,
        Err(error) => {
            log_hex(
                b"drv-i2c-hid: report descriptor parse error=0x",
                error as u64,
            );
            kumo_rt::process_exit(1);
        }
    };

    log(b"drv-i2c-hid: report descriptor ok\n");
    match keyboard.report_id {
        Some(report_id) => log_hex(b"drv-i2c-hid: keyboard-report-id=0x", report_id as u64),
        None => log(b"drv-i2c-hid: keyboard-report-id=none\n"),
    }
    kumo_rt::process_exit(0);
}
