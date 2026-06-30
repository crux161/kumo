#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

use drv_i2c_hid::{
    bounded_input_frame_len, bounded_report_descriptor_len, classify_input_report_with_mouse,
    decode_mouse_probe, encode_mouse_event, should_log_input_report_stats, BoundedFailureLog,
    DeviceQuirks, InputProbeDecoder, InputProbeError, InputReportClass, InputReportStats,
    ProbeConfig, KEYBOARD_BOOTSTRAP_TAG, MAX_INPUT_FRAME_BYTES, MAX_REPORT_DESCRIPTOR_BYTES,
    MOUSE_BOOTSTRAP_TAG,
};
use kumo_abi::{decode_tlmm_gpio_irq, tlmm_gpio_irq, Handle, VmarFlags};
use kumo_i2c_hid::{
    find_boot_keyboard, find_boot_mouse, Command, Controller, HidDescriptor, PowerState, RegisterIo,
};
use kumo_rt::{
    channel_read_with_handle, channel_write, debug_write, handle_close, handle_koid,
    interrupt_complete, interrupt_create, interrupt_wait, port_bind, port_create, port_unbind,
    port_wait, resource_mint_mmio, timer_create, vmar_map,
};

kumo_rt::entry!(main);

const MMIO_VA: u64 = 0x0000_0000_1100_0000;
const POLL_LIMIT: usize = 1_000_000;
const POWER_ON_SETTLE_NS: u64 = 60_000_000;
const RESET_ACK_TIMEOUT_NS: u64 = 1_000_000_000;
/// DT interrupt flag for IRQ_TYPE_EDGE_FALLING. ELAN i2c-hid needs the attention line treated as
/// falling-edge (Linux FORCE_TRIGGER_FALLING), overriding the level-low (flag 8) the DT declares.
const DT_IRQ_EDGE_FALLING: u32 = 2;

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

fn log_hex(label: &[u8], value: u64) {
    log(label);
    log_hex_inline(value);
    log(b"\n");
}

fn log_hex_inline(mut value: u64) {
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
}

/// Diagnostic: dump raw frame bytes as space-separated hex. Bounded by the caller to the first few
/// IRQ deliveries / non-empty frames so the framebuffer console is not flooded. — KESTREL
fn log_frame(label: &[u8], bytes: &[u8]) {
    log(label);
    for &byte in bytes {
        let pair = [
            b"0123456789abcdef"[(byte >> 4) as usize],
            b"0123456789abcdef"[(byte & 0xf) as usize],
            b' ',
        ];
        debug_write(pair.as_ptr(), pair.len());
    }
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

fn log_input_report_stats(stats: &InputReportStats) {
    log(b"drv-i2c-hid: stats f=0x");
    log_hex_inline(stats.frames as u64);
    log(b" k=0x");
    log_hex_inline(stats.keyboard_reports as u64);
    log(b" m=0x");
    log_hex_inline(stats.mouse_reports as u64);
    log(b" rst=0x");
    log_hex_inline(stats.reset_frames as u64);
    log(b" bog=0x");
    log_hex_inline(stats.bogus_irq_frames as u64);
    log(b" foreign=0x");
    log_hex_inline(stats.foreign_report_ids as u64);
    log(b" proto=0x");
    log_hex_inline(stats.protocol_errors as u64);
    log(b" decode=0x");
    log_hex_inline(stats.decode_errors as u64);
    log(b" ascii=0x");
    log_hex_inline(stats.forwarded_ascii as u64);
    log(b" mouse=0x");
    log_hex_inline(stats.forwarded_mouse as u64);
    log(b" drop=0x");
    log_hex_inline(stats.keyboard_write_drops as u64);
    log(b" mdrop=0x");
    log_hex_inline(stats.mouse_write_drops as u64);
    log(b"\n");
    if stats.last_report_id.is_some()
        || stats.last_protocol_error.is_some()
        || stats.last_decode_error.is_some()
    {
        log(b"drv-i2c-hid: stats last rid=0x");
        log_hex_inline(stats.last_report_id.unwrap_or(0xff) as u64);
        log(b" proto=0x");
        log_hex_inline(stats.last_protocol_error.map_or(0xff, |error| error as u8) as u64);
        log(b" decode=0x");
        log_hex_inline(stats.last_decode_error.map_or(0xff, |error| error as u8) as u64);
        log(b"\n");
    }
}

fn maybe_log_input_report_stats(stats: &InputReportStats, logged: &mut u32) {
    if should_log_input_report_stats(stats.frames, *logged) {
        *logged = logged.saturating_add(1);
        log_input_report_stats(stats);
    }
}

fn sleep_ns(delay_ns: u64) -> bool {
    let port_raw = port_create();
    if port_raw == u64::MAX {
        return false;
    }
    let timer_raw = timer_create(delay_ns);
    if timer_raw == u64::MAX {
        let _ = handle_close(Handle(port_raw as u32));
        return false;
    }
    let port = Handle(port_raw as u32);
    let timer = Handle(timer_raw as u32);
    let ok = port_bind(port, timer) == 0 && port_wait(port) != 0;
    let _ = handle_close(timer);
    let _ = handle_close(port);
    ok
}

fn wait_attention_or_timeout(attention_irq: Handle, timeout_ns: u64) -> bool {
    let port_raw = port_create();
    if port_raw == u64::MAX {
        return false;
    }
    let timer_raw = timer_create(timeout_ns);
    if timer_raw == u64::MAX {
        let _ = handle_close(Handle(port_raw as u32));
        return false;
    }
    let port = Handle(port_raw as u32);
    let timer = Handle(timer_raw as u32);
    if port_bind(port, attention_irq) != 0 || port_bind(port, timer) != 0 {
        let _ = handle_close(timer);
        let _ = handle_close(port);
        return false;
    }
    let timer_koid = handle_koid(timer);
    let source = port_wait(port);
    let _ = port_unbind(port, attention_irq);
    let _ = port_unbind(port, timer);
    let _ = handle_close(timer);
    let _ = handle_close(port);
    if source == 0 || source == timer_koid {
        return false;
    }
    interrupt_wait(attention_irq) != 0
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

    let (received, mouse_raw) = channel_read_with_handle(bootstrap, tag.as_mut_ptr(), tag.len());
    if received != tag.len() || tag[0] != MOUSE_BOOTSTRAP_TAG || mouse_raw == 0 {
        log(b"drv-i2c-hid: mouse bootstrap failed\n");
        kumo_rt::process_exit(1);
    }
    let mouse_channel = Handle(mouse_raw as u32);
    log(b"drv-i2c-hid: mouse channel ok\n");

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

    let quirks = DeviceQuirks::for_vendor_product(descriptor.vendor_id, descriptor.product_id);
    if quirks.no_wakeup_after_reset {
        log(b"drv-i2c-hid: elan no-wakeup-after-reset quirk\n");
    }

    let input_frame_len = match bounded_input_frame_len(descriptor.max_input_length) {
        Ok(length) => length,
        Err(error) => {
            log_input_probe_error(error);
            kumo_rt::process_exit(1);
        }
    };
    log_hex(b"drv-i2c-hid: irq read size=0x", input_frame_len as u64);

    // Keep the command phase fully observable on metal. The last failed flash stopped after the
    // read-size line, so every step from SET_POWER through RESET gets a before/after breadcrumb.
    // Create the GPIO attention object after RESET is on the bus: KUMO's `InterruptCreate` enables
    // the line immediately, unlike Linux's IRQF_NO_AUTOEN request path. The HID reset-complete
    // source is level-low-until-drained, so arming after RESET can still catch and drain it while
    // avoiding a pre-reset asserted line during command writes. — KESTREL
    log(b"drv-i2c-hid: set-power begin\n");
    if let Err(error) = controller.write(
        config.i2c_address,
        &Command::set_power(descriptor.command_register, PowerState::On),
    ) {
        log_hex(b"drv-i2c-hid: set-power error=0x", error as u64);
        kumo_rt::process_exit(1);
    }
    log(b"drv-i2c-hid: set-power done\n");
    log(b"drv-i2c-hid: power-on settle begin\n");
    if !sleep_ns(POWER_ON_SETTLE_NS) {
        log(b"drv-i2c-hid: power-on settle wait failed\n");
        kumo_rt::process_exit(1);
    }
    log(b"drv-i2c-hid: power-on settle done\n");
    log(b"drv-i2c-hid: reset begin\n");
    if let Err(error) = controller.write(
        config.i2c_address,
        &Command::reset(descriptor.command_register),
    ) {
        log_hex(b"drv-i2c-hid: reset error=0x", error as u64);
        kumo_rt::process_exit(1);
    }
    log(b"drv-i2c-hid: reset done\n");
    // ELAN i2c-hid needs the attention line as falling-edge, not the DT's level-low — Linux
    // FORCE_TRIGGER_FALLING. Re-encode the same GPIO pin with the falling-edge flag; the authority
    // key is pin-based, so the granted Resource window still covers it. — CORVUS
    let attention_irq_encoded = if quirks.force_trigger_falling {
        match decode_tlmm_gpio_irq(config.attention_irq) {
            Some(gpio) => {
                log(b"drv-i2c-hid: elan falling-edge attention quirk\n");
                tlmm_gpio_irq(gpio.pin, DT_IRQ_EDGE_FALLING)
            }
            None => config.attention_irq,
        }
    } else {
        config.attention_irq
    };
    let attention_raw = interrupt_create(resource, attention_irq_encoded);
    if attention_raw == u64::MAX {
        log(b"drv-i2c-hid: attention interrupt create failed\n");
        kumo_rt::process_exit(1);
    }
    let attention_irq = Handle(attention_raw as u32);
    log(b"drv-i2c-hid: attention interrupt created\n");
    let mut input_frame = [0u8; MAX_INPUT_FRAME_BYTES];
    log(b"drv-i2c-hid: reset sync wait begin\n");
    if wait_attention_or_timeout(attention_irq, RESET_ACK_TIMEOUT_NS) {
        if let Err(error) = controller.read(config.i2c_address, &mut input_frame[..input_frame_len])
        {
            log_hex(b"drv-i2c-hid: reset sync read error=0x", error as u64);
            kumo_rt::process_exit(1);
        }
        if interrupt_complete(attention_irq) != 0 {
            log(b"drv-i2c-hid: reset sync complete failed\n");
            kumo_rt::process_exit(1);
        }
        let reset_len = u16::from_le_bytes([input_frame[0], input_frame[1]]);
        log_hex(b"drv-i2c-hid: reset sync len=0x", reset_len as u64);
        log_frame(
            b"drv-i2c-hid: reset raw= ",
            &input_frame[..input_frame_len.min(16)],
        );
    } else {
        log(b"drv-i2c-hid: reset sync timeout\n");
    }
    if !quirks.no_wakeup_after_reset {
        if let Err(error) = controller.write(
            config.i2c_address,
            &Command::set_power(descriptor.command_register, PowerState::On),
        ) {
            log_hex(b"drv-i2c-hid: post-reset set-power error=0x", error as u64);
            kumo_rt::process_exit(1);
        }
        if !sleep_ns(POWER_ON_SETTLE_NS) {
            log(b"drv-i2c-hid: post-reset settle wait failed\n");
            kumo_rt::process_exit(1);
        }
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
    let led_output_report_id = kumo_i2c_hid::find_led_output_report_id(&report_descriptor);
    let mouse = find_boot_mouse(&report_descriptor[..report_descriptor_len]).ok();
    if let Some(id) = led_output_report_id {
        log_hex(b"drv-i2c-hid: led-output-report-id=0x", id as u64);
    } else {
        log(b"drv-i2c-hid: led-output-report-id=none\n");
    }
    match keyboard.report_id {
        Some(report_id) => log_hex(b"drv-i2c-hid: keyboard-report-id=0x", report_id as u64),
        None => log(b"drv-i2c-hid: keyboard-report-id=none\n"),
    }
    match mouse.and_then(|report| report.report_id) {
        Some(report_id) => log_hex(b"drv-i2c-hid: mouse-report-id=0x", report_id as u64),
        None if mouse.is_some() => log(b"drv-i2c-hid: mouse-report-id=none\n"),
        None => log(b"drv-i2c-hid: mouse-report=none\n"),
    }

    let mut input_decoder = InputProbeDecoder::new();
    log(b"drv-i2c-hid: attention interrupt ready\n");
    // Keep CORVUS's bounded frame dump on the interrupt path: now each line proves a real GPIO-104
    // delivery survived mask -> drain -> complete, not just a timer poll. — KESTREL
    let mut interrupts: u32 = 0;
    let mut shown_nonempty: u32 = 0;
    let mut keyboard_forward_failures = BoundedFailureLog::new();
    let mut mouse_forward_failures = BoundedFailureLog::new();
    let mut input_decode_failures = BoundedFailureLog::new();
    let mut input_stats = InputReportStats::new();
    let mut input_stats_logs: u32 = 0;
    let mut caps_lock = false;
    loop {
        if interrupt_wait(attention_irq) == 0 {
            log(b"drv-i2c-hid: attention wait failed\n");
            kumo_rt::process_exit(1);
        }

        // Fetch the input report with a PLAIN read (Linux i2c_hid_get_input → i2c_master_recv);
        // addressing the input register first returns the device's "no data" response instead of
        // the pending report — which is why every earlier poll came back empty. — CORVUS
        if let Err(error) = controller.read(config.i2c_address, &mut input_frame[..input_frame_len])
        {
            log_hex(b"drv-i2c-hid: input frame read error=0x", error as u64);
            kumo_rt::process_exit(1);
        }
        if interrupt_complete(attention_irq) != 0 {
            log(b"drv-i2c-hid: attention complete failed\n");
            kumo_rt::process_exit(1);
        }
        // Ground-truth instrumentation. `irq tick` proves the attention line delivered and the
        // length word shows whether the device supplied a reset/empty frame or report bytes.
        // `frame=` dumps the first 16 non-empty reports. — KESTREL
        interrupts = interrupts.wrapping_add(1);
        let frame_len = u16::from_le_bytes([input_frame[0], input_frame[1]]);
        let report_class = classify_input_report_with_mouse(
            &input_frame[..input_frame_len],
            keyboard.report_id,
            mouse,
            quirks,
        );
        input_stats.record_class(report_class);
        // Log EVERY delivery (bounded to 64), not just the first 3, so a keypress that produces a
        // NEW `irq tick` line is visible — this is what distinguishes "no attention IRQ on keypress"
        // (device not reporting) from "IRQ fires but read is empty". Completion suppresses idle
        // redelivery, so at idle this is silent; press a key and watch for a new line. — CORVUS
        if interrupts <= 64 {
            log_hex(b"drv-i2c-hid: irq tick len=0x", frame_len as u64);
        }
        // Dump the raw bytes of the first few reads even when empty, so we can see exactly what a
        // len=0 read contains on the wire. — CORVUS
        if interrupts <= 4 {
            log_frame(
                b"drv-i2c-hid: raw= ",
                &input_frame[..input_frame_len.min(16)],
            );
        }
        if frame_len != 0 && shown_nonempty < 16 {
            shown_nonempty += 1;
            log_frame(
                b"drv-i2c-hid: frame= ",
                &input_frame[..input_frame_len.min(16)],
            );
        }
        if report_class == InputReportClass::MouseReport {
            if let Some(mouse_report) = mouse {
                match decode_mouse_probe(&input_frame[..input_frame_len], mouse_report, quirks) {
                    Ok(Some(report)) => {
                        let event = encode_mouse_event(report);
                        if channel_write(mouse_channel, event.as_ptr(), event.len()) == 0 {
                            input_stats.record_forwarded_mouse();
                        } else if mouse_forward_failures.record() {
                            input_stats.record_mouse_write_drop();
                            log_hex(
                                b"drv-i2c-hid: mouse event dropped count=0x",
                                mouse_forward_failures.count() as u64,
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        if input_decode_failures.record() {
                            log_hex(b"drv-i2c-hid: mouse report decode error=0x", error as u64);
                        }
                    }
                }
            }
            maybe_log_input_report_stats(&input_stats, &mut input_stats_logs);
            continue;
        }
        let input = match input_decoder.decode_with_quirks(
            &input_frame[..input_frame_len],
            keyboard.report_id,
            quirks,
        ) {
            Ok(input) => input,
            // After first light, a single odd input report is soft-state loss, not a driver death:
            // the Elan is a keyboard+pointer combo, so a non-keyboard report ID, a rollover, or a
            // malformed frame can all reach here. Bounded-log a few and keep serving the IRQ loop
            // (DESIGN/002). The true transport failures above — attention wait, I2C read, interrupt
            // complete — stay fatal; only per-report decode loss is recoverable. — CORVUS
            Err(error) => {
                if let InputProbeError::Decode(decode_error) = error {
                    input_stats.record_decode_error(decode_error);
                }
                if input_decode_failures.record() {
                    log_input_probe_error(error);
                    log_hex(
                        b"drv-i2c-hid: input report dropped count=0x",
                        input_decode_failures.count() as u64,
                    );
                }
                maybe_log_input_report_stats(&input_stats, &mut input_stats_logs);
                continue;
            }
        };
        if input.caps_lock_toggle {
            caps_lock = !caps_lock;
            let report_id = led_output_report_id.or(keyboard.report_id);
            let report_id_byte = report_id.unwrap_or(0);

            let mut payload = [0u8; 8];
            let mut len = 0;

            if descriptor.output_register != 0 {
                // Output Register Write:
                // [OutRegLo, OutRegHi, LenLo, LenHi, (ReportID), Payload]
                payload[len] = (descriptor.output_register & 0xff) as u8;
                len += 1;
                payload[len] = (descriptor.output_register >> 8) as u8;
                len += 1;

                let report_len: u16 = if report_id.is_some() { 5 } else { 4 };
                payload[len] = (report_len & 0xff) as u8;
                len += 1;
                payload[len] = (report_len >> 8) as u8;
                len += 1;

                if let Some(id) = report_id {
                    payload[len] = id;
                    len += 1;
                }
                payload[len] = if caps_lock { 0x02 } else { 0x00 };
                len += 1;
                payload[len] = 0x00; // Second byte for extra LEDs like Mic Mute
                len += 1;

                let _ = controller.write(config.i2c_address, &payload[..len]);
            } else {
                // Fallback: Data Register + SET_REPORT
                payload[len] = (descriptor.data_register & 0xff) as u8;
                len += 1;
                payload[len] = (descriptor.data_register >> 8) as u8;
                len += 1;

                let report_len: u16 = if report_id.is_some() { 5 } else { 4 };
                payload[len] = (report_len & 0xff) as u8;
                len += 1;
                payload[len] = (report_len >> 8) as u8;
                len += 1;

                if let Some(id) = report_id {
                    payload[len] = id;
                    len += 1;
                }
                payload[len] = if caps_lock { 0x02 } else { 0x00 };
                len += 1;
                payload[len] = 0x00;
                len += 1;

                let _ = controller.write(config.i2c_address, &payload[..len]);
                let _ = controller.write(
                    config.i2c_address,
                    &kumo_i2c_hid::Command::set_report(descriptor.command_register, report_id_byte),
                );
            }
        }
        // Only emit on real terminal keypress bytes; idle reports and non-byte keys stay silent so
        // the framebuffer console is not flooded. The log label keeps the historic `ascii=...`
        // spelling so metal captures remain comparable across HID slices.
        for &ascii in input.pressed_terminal_bytes() {
            let byte = [ascii];
            if channel_write(keyboard_channel, byte.as_ptr(), byte.len()) == 0 {
                input_stats.record_forwarded_ascii();
                log_hex(b"drv-i2c-hid: key forwarded ascii=0x", ascii as u64);
            } else if keyboard_forward_failures.record() {
                input_stats.record_keyboard_write_drop();
                // A closed/restarting keyboard consumer is soft-state loss, not a hardware-driver
                // death. Keep the IRQ loop alive and drop the byte per DESIGN/002. — KESTREL
                log_hex(
                    b"drv-i2c-hid: keyboard byte dropped count=0x",
                    keyboard_forward_failures.count() as u64,
                );
            }
        }
        maybe_log_input_report_stats(&input_stats, &mut input_stats_logs);
    }
}
