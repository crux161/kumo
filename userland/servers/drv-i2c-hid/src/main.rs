#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

use drv_i2c_hid::{
    bounded_input_frame_len, bounded_report_descriptor_len, InputProbeDecoder, InputProbeError,
    ProbeConfig, KEYBOARD_BOOTSTRAP_TAG, MAX_INPUT_FRAME_BYTES, MAX_REPORT_DESCRIPTOR_BYTES,
};
use kumo_abi::{Handle, VmarFlags};
use kumo_i2c_hid::{
    find_boot_keyboard, Command, Controller, HidDescriptor, PowerState, RegisterIo,
};
use kumo_rt::{
    channel_read_with_handle, channel_write, debug_write, handle_close, port_bind, port_create,
    port_wait, resource_mint_mmio, timer_create, vmar_map,
};

kumo_rt::entry!(main);

const MMIO_VA: u64 = 0x0000_0000_1100_0000;
const POLL_LIMIT: usize = 1_000_000;
/// Poll cadence for the input register while interrupt-completion (DESIGN/016) is unbuilt: 10 ms
/// (~100 Hz), well under a keystroke and cheap on a cooperative scheduler that yields each tick.
const POLL_INTERVAL_NS: u64 = 10_000_000;

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

fn log_input_probe_error(error: InputProbeError) {
    match error {
        InputProbeError::InvalidLength => log(b"drv-i2c-hid: input frame length invalid\n"),
        InputProbeError::Protocol(error) => {
            log_hex(b"drv-i2c-hid: input frame protocol error=0x", error as u64);
        }
        InputProbeError::Decode(error) => {
            log_hex(b"drv-i2c-hid: input frame decode error=0x", error as u64);
        }
    }
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

    let mut tag = [0u8; 1];
    let (received, keyboard_raw) = channel_read_with_handle(bootstrap, tag.as_mut_ptr(), tag.len());
    if received != tag.len() || tag[0] != KEYBOARD_BOOTSTRAP_TAG || keyboard_raw == 0 {
        log(b"drv-i2c-hid: keyboard bootstrap failed\n");
        kumo_rt::process_exit(1);
    }
    let keyboard_channel = Handle(keyboard_raw as u32);
    log(b"drv-i2c-hid: keyboard channel ok\n");

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

    // HID-over-I2C bring-up (spec 1.0 §7.2; order per Linux i2c-hid `i2c_hid_start_hwreset`):
    // SET_POWER(On) then RESET, written to the command register. The device answers RESET with a
    // length-0 input report (the reset-complete sync), which the poll loop decodes as a benign
    // no-event frame. Without this the device stays unstarted and only ever returns that empty
    // frame — the exact koid-0x2b exit=1 we kept hitting (it never powered on). — CORVUS
    if let Err(error) = controller.write(
        config.i2c_address,
        &Command::set_power(descriptor.command_register, PowerState::On),
    ) {
        log_hex(b"drv-i2c-hid: set-power error=0x", error as u64);
        kumo_rt::process_exit(1);
    }
    if let Err(error) = controller.write(
        config.i2c_address,
        &Command::reset(descriptor.command_register),
    ) {
        log_hex(b"drv-i2c-hid: reset error=0x", error as u64);
        kumo_rt::process_exit(1);
    }
    log(b"drv-i2c-hid: power-on + reset issued\n");

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

    let input_frame_len = match bounded_input_frame_len(descriptor.max_input_length) {
        Ok(length) => length,
        Err(error) => {
            log_input_probe_error(error);
            kumo_rt::process_exit(1);
        }
    };
    // Poll mode: deliberately do NOT arm the GPIO-104 attention IRQ. That line is level-low, and
    // the kernel's clear-before-drain ack with no completion primitive storms a cooperative
    // single-core kernel (DESIGN/016). Until that interrupt-completion lifecycle lands, yield on a
    // one-shot timer bound to a port and poll the input register. Every failure here is <= the
    // prior contained exit: a bad transfer exits (contained TOWER, boot proceeds); an idle device
    // just yields and re-polls — never a storm. Swap to interrupt_wait once DESIGN/016 ships.
    // — CORVUS
    let port_raw = port_create();
    if port_raw == u64::MAX {
        log(b"drv-i2c-hid: poll port create failed\n");
        kumo_rt::process_exit(1);
    }
    let port = Handle(port_raw as u32);

    let mut input_decoder = InputProbeDecoder::new();
    let mut input_frame = [0u8; MAX_INPUT_FRAME_BYTES];
    log(b"drv-i2c-hid: poll loop ready\n");
    loop {
        // Cooperative yield: a one-shot timer bound to our port, awaited via PortWait.
        let timer_raw = timer_create(POLL_INTERVAL_NS);
        if timer_raw == u64::MAX {
            log(b"drv-i2c-hid: poll timer create failed\n");
            kumo_rt::process_exit(1);
        }
        let timer = Handle(timer_raw as u32);
        if port_bind(port, timer) != 0 {
            log(b"drv-i2c-hid: poll timer bind failed\n");
            kumo_rt::process_exit(1);
        }
        port_wait(port);
        let _ = handle_close(timer);

        if let Err(error) = controller.write_read(
            config.i2c_address,
            &descriptor.input_register.to_le_bytes(),
            &mut input_frame[..input_frame_len],
        ) {
            log_hex(b"drv-i2c-hid: input frame transfer error=0x", error as u64);
            kumo_rt::process_exit(1);
        }
        let input = match input_decoder.decode(&input_frame[..input_frame_len], keyboard.report_id)
        {
            Ok(input) => input,
            Err(error) => {
                log_input_probe_error(error);
                kumo_rt::process_exit(1);
            }
        };
        // Only emit on a real keypress; idle polls (the common case at 100 Hz) stay silent so the
        // framebuffer console is not flooded.
        if let Some(ascii) = input.first_pressed_ascii {
            let byte = [ascii];
            if channel_write(keyboard_channel, byte.as_ptr(), byte.len()) == 0 {
                log_hex(b"drv-i2c-hid: key forwarded ascii=0x", ascii as u64);
            } else {
                log(b"drv-i2c-hid: keyboard byte forward failed\n");
                kumo_rt::process_exit(1);
            }
        }
    }
}
