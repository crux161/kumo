#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

use drv_i2c_hid::{
    bounded_input_frame_len, bounded_report_descriptor_len, input_read_len, BoundedFailureLog,
    DecodedReport, DeviceQuirks, InputProbeDecoder, InputProbeError, ProbeConfig,
    KEYBOARD_BOOTSTRAP_TAG, MAX_INPUT_FRAME_BYTES, MAX_REPORT_DESCRIPTOR_BYTES,
};
use kumo_abi::i2c::{I2cOpcode, I2cRequestHeader, I2cTransferRequest, I2cTransferResponse};
use kumo_abi::Handle;
use kumo_i2c_hid::{inspect_report_descriptor, Command, HidDescriptor, PowerState};
use kumo_rt::{
    channel_read_with_handle, channel_write, debug_write, handle_close, handle_koid,
    interrupt_complete, interrupt_create, interrupt_wait, port_bind, port_create, port_unbind,
    port_wait, resource_mint_mmio, timer_create, vmar_map,
};

kumo_rt::entry!(main);

fn any_as_u8_slice<T: Sized>(p: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts((p as *const T) as *const u8, core::mem::size_of::<T>()) }
}

struct IpcError {
    code: u64,
}

impl IpcError {
    fn code(&self) -> u64 {
        self.code
    }
}

struct IpcController {
    channel: Handle,
}

impl IpcController {
    fn write(&mut self, address: u8, data: &[u8]) -> Result<(), IpcError> {
        self.write_read(address, data, &mut [])
    }
    
    fn read(&mut self, address: u8, buffer: &mut [u8]) -> Result<(), IpcError> {
        self.write_read(address, &[], buffer)
    }
    
    fn write_read(&mut self, address: u8, write_data: &[u8], read_data: &mut [u8]) -> Result<(), IpcError> {
        let req = kumo_abi::i2c::I2cTransferRequest {
            header: kumo_abi::i2c::I2cRequestHeader {
                opcode: kumo_abi::i2c::I2cOpcode::Transfer,
                bus: 0,
                address: address as u16,
                _pad: 0,
            },
            write_len: write_data.len() as u16,
            read_len: read_data.len() as u16,
        };
        
        let mut req_msg = [0u8; 512];
        let req_hdr = any_as_u8_slice(&req);
        req_msg[..req_hdr.len()].copy_from_slice(req_hdr);
        if write_data.len() > 0 {
            req_msg[req_hdr.len()..req_hdr.len() + write_data.len()].copy_from_slice(write_data);
        }
        
        let (local_resp, remote_resp) = kumo_rt::channel_create_pair();
        if local_resp == u64::MAX || remote_resp == u64::MAX {
            return Err(IpcError { code: 1000 });
        }
        
        let status = kumo_rt::channel_write_with_handle(
            self.channel,
            req_msg.as_ptr(),
            req_hdr.len() + write_data.len(),
            Handle(remote_resp as u32),
        );
        if status != 0 {
            let _ = kumo_rt::handle_close(Handle(local_resp as u32));
            return Err(IpcError { code: 1001 });
        }
        
        let port_raw = kumo_rt::port_create();
        let port = Handle(port_raw as u32);
        kumo_rt::port_bind(port, Handle(local_resp as u32));
        let _ = kumo_rt::port_wait(port);
        let _ = kumo_rt::handle_close(port);
        
        let mut resp_msg = [0u8; 512];
        let (received, tag_raw) = kumo_rt::channel_read_with_handle(
            Handle(local_resp as u32),
            resp_msg.as_mut_ptr(),
            resp_msg.len(),
        );
        
        let _ = kumo_rt::handle_close(Handle(local_resp as u32));
        if tag_raw != 0 {
            let _ = kumo_rt::handle_close(Handle(tag_raw as u32));
        }
        
        if received < core::mem::size_of::<kumo_abi::i2c::I2cTransferResponse>() {
            return Err(IpcError { code: 1003 });
        }
        
        let resp: kumo_abi::i2c::I2cTransferResponse = unsafe { core::ptr::read_unaligned(resp_msg.as_ptr() as *const _) };
        if resp.status != 0 {
            return Err(IpcError { code: (-resp.status) as u64 });
        }
        
        if resp.read_len as usize > 0 && resp.read_len as usize <= read_data.len() {
            let hdr_len = core::mem::size_of::<kumo_abi::i2c::I2cTransferResponse>();
            if received >= hdr_len + resp.read_len as usize {
                read_data[..resp.read_len as usize].copy_from_slice(&resp_msg[hdr_len..hdr_len + resp.read_len as usize]);
            } else {
                return Err(IpcError { code: 1004 });
            }
        }
        Ok(())
    }
}

const POLL_LIMIT: usize = 1_000_000;
const WAKE_RETRY_NS: u64 = 500_000;
const POWER_ON_SETTLE_NS: u64 = 60_000_000;
const RESET_RETRY_NS: u64 = 1_000_000_000;
const RESET_ACK_TIMEOUT_NS: u64 = 1_000_000_000;
const NO_IRQ_AFTER_RESET_DELAY_NS: u64 = 100_000_000;
const STEADY_POLL_FALLBACK_NS: u64 = 20_000_000;
const RESET_ATTEMPTS: u32 = 3;
const ATTENTION_WAIT_TIMEOUT: u64 = 0;
const ATTENTION_WAIT_FAILED: u64 = u64::MAX;

fn log(message: &[u8]) {
    debug_write(message.as_ptr(), message.len());
}

fn log_hex(label: &[u8], mut value: u64) {
    let mut line = [0u8; 128];
    let mut len = label.len().min(line.len());
    line[..len].copy_from_slice(&label[..len]);
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
    for &digit in &digits[start..] {
        if len == line.len() {
            break;
        }
        line[len] = digit;
        len += 1;
    }
    if len < line.len() {
        line[len] = b'\n';
        len += 1;
    }
    log(&line[..len]);
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

fn wait_attention_or_timeout(attention_irq: Handle, timeout_ns: u64) -> u64 {
    let port_raw = port_create();
    if port_raw == u64::MAX {
        return ATTENTION_WAIT_FAILED;
    }
    let timer_raw = timer_create(timeout_ns);
    if timer_raw == u64::MAX {
        let _ = handle_close(Handle(port_raw as u32));
        return ATTENTION_WAIT_FAILED;
    }
    let port = Handle(port_raw as u32);
    let timer = Handle(timer_raw as u32);
    if port_bind(port, attention_irq) != 0 || port_bind(port, timer) != 0 {
        let _ = handle_close(timer);
        let _ = handle_close(port);
        return ATTENTION_WAIT_FAILED;
    }
    let timer_koid = handle_koid(timer);
    let source = port_wait(port);
    let _ = port_unbind(port, attention_irq);
    let _ = port_unbind(port, timer);
    let _ = handle_close(timer);
    let _ = handle_close(port);
    if source == 0 || source == timer_koid {
        return ATTENTION_WAIT_TIMEOUT;
    }
    let count = interrupt_wait(attention_irq);
    if count == 0 {
        ATTENTION_WAIT_FAILED
    } else {
        count
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

    let (received, i2c_client_raw) = channel_read_with_handle(bootstrap, tag.as_mut_ptr(), tag.len());
    if received != tag.len() || tag[0] != b'I' || i2c_client_raw == 0 {
        log(b"drv-i2c-hid: i2c client bootstrap failed\n");
        kumo_rt::process_exit(1);
    }
    let mut controller = IpcController { channel: Handle(i2c_client_raw as u32) };
    log(b"drv-i2c-hid: I2C IPC ready\n");

    let resource = Handle(resource_raw as u32);

    let mut raw_descriptor = [0u8; HidDescriptor::BYTES];
    if let Err(error) = controller.write_read(
        config.i2c_address,
        &config.hid_descriptor_register.to_le_bytes(),
        &mut raw_descriptor,
    ) {
        log_hex(
            b"drv-i2c-hid: descriptor transfer retry error=0x",
            error.code(),
        );
        if !sleep_ns(WAKE_RETRY_NS) {
            log(b"drv-i2c-hid: descriptor retry wait failed\n");
            kumo_rt::process_exit(1);
        }
        if let Err(error) = controller.write_read(
            config.i2c_address,
            &config.hid_descriptor_register.to_le_bytes(),
            &mut raw_descriptor,
        ) {
            log_hex(b"drv-i2c-hid: transfer error=0x", error.code());
            kumo_rt::process_exit(1);
        }
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
    if quirks.no_irq_after_reset {
        log(b"drv-i2c-hid: no-irq-after-reset quirk\n");
    }
    if quirks.no_wakeup_after_reset {
        log(b"drv-i2c-hid: elan no-wakeup-after-reset quirk\n");
    }
    if quirks.bad_input_size {
        log(b"drv-i2c-hid: bad-input-size quirk\n");
    }
    if quirks.re_power_on {
        log(b"drv-i2c-hid: re-power-on quirk\n");
    }

    let reset_input_frame_len = match bounded_input_frame_len(descriptor.max_input_length) {
        Ok(length) => length,
        Err(error) => {
            log_input_probe_error(error);
            kumo_rt::process_exit(1);
        }
    };
    log_hex(
        b"drv-i2c-hid: reset read size=0x",
        reset_input_frame_len as u64,
    );

    // Keep the command phase fully observable on metal. The last failed flash stopped after the
    // read-size line, so every step from SET_POWER through RESET gets a before/after breadcrumb.
    // Create the GPIO attention object after RESET is on the bus: KUMO's `InterruptCreate` enables
    // the line immediately, unlike Linux's IRQF_NO_AUTOEN request path. The HID reset-complete
    // source is level-low-until-drained, so arming after RESET can still catch and drain it while
    // avoiding a pre-reset asserted line during command writes. — KESTREL
    let mut reset_started = false;
    let mut attempt = 1u32;
    while attempt <= RESET_ATTEMPTS {
        log_hex(b"drv-i2c-hid: reset attempt=0x", attempt as u64);
        log(b"drv-i2c-hid: set-power begin\n");
        let set_power = controller.write(
            config.i2c_address,
            &Command::set_power(descriptor.command_register, PowerState::On),
        );
        let set_power = if set_power.is_err() {
            if !sleep_ns(WAKE_RETRY_NS) {
                log(b"drv-i2c-hid: set-power retry wait failed\n");
                kumo_rt::process_exit(1);
            }
            controller.write(
                config.i2c_address,
                &Command::set_power(descriptor.command_register, PowerState::On),
            )
        } else {
            set_power
        };
        if let Err(error) = set_power {
            log_hex(b"drv-i2c-hid: set-power error=0x", error.code());
        } else {
            log(b"drv-i2c-hid: set-power done\n");
            log(b"drv-i2c-hid: power-on settle begin\n");
            if !sleep_ns(POWER_ON_SETTLE_NS) {
                log(b"drv-i2c-hid: power-on settle wait failed\n");
                kumo_rt::process_exit(1);
            }
            log(b"drv-i2c-hid: power-on settle done\n");
            log(b"drv-i2c-hid: reset begin\n");
            match controller.write(
                config.i2c_address,
                &Command::reset(descriptor.command_register),
            ) {
                Ok(()) => {
                    reset_started = true;
                    log(b"drv-i2c-hid: reset done\n");
                    break;
                }
                Err(error) => log_hex(b"drv-i2c-hid: reset error=0x", error.code()),
            }
        }
        if attempt == RESET_ATTEMPTS {
            break;
        }
        if !sleep_ns(RESET_RETRY_NS) {
            log(b"drv-i2c-hid: reset retry wait failed\n");
            kumo_rt::process_exit(1);
        }
        attempt += 1;
    }
    if !reset_started {
        log(b"drv-i2c-hid: reset attempts exhausted\n");
        kumo_rt::process_exit(1);
    }
    // Arm the attention line with the trigger the device tree declares (level-low for
    // `keyboard@68`, `tlmm_gpio_irq(104, 8)`), exactly as Linux requests it
    // (`IRQF_TRIGGER_LOW | IRQF_ONESHOT`). The kernel masks the line on delivery and re-enables it
    // on `interrupt_complete` after the I2C read has drained (de-asserted) the source — the
    // DESIGN/016 lifecycle, which is KUMO's ONESHOT equivalent. KUMO previously re-encoded this to
    // falling-edge (J289/J290) on an invented `FORCE_TRIGGER_FALLING` quirk that Linux does not
    // have; edge dropped any report still pending at service time (no fresh edge), so it is gone and
    // the DT encoding is used unchanged. — CORVUS
    let attention_raw = interrupt_create(resource, config.attention_irq);
    if attention_raw == u64::MAX {
        log(b"drv-i2c-hid: attention interrupt create failed\n");
        kumo_rt::process_exit(1);
    }
    let attention_irq = Handle(attention_raw as u32);
    log(b"drv-i2c-hid: attention interrupt created\n");
    let mut input_frame = [0u8; MAX_INPUT_FRAME_BYTES];
    if quirks.no_irq_after_reset {
        log(b"drv-i2c-hid: reset sync no-irq delay begin\n");
        if !sleep_ns(NO_IRQ_AFTER_RESET_DELAY_NS) {
            log(b"drv-i2c-hid: reset sync no-irq delay failed\n");
            kumo_rt::process_exit(1);
        }
        log(b"drv-i2c-hid: reset sync no-irq delay done\n");
    } else {
        log(b"drv-i2c-hid: reset sync wait begin\n");
        let reset_fires = wait_attention_or_timeout(attention_irq, RESET_ACK_TIMEOUT_NS);
        if reset_fires == ATTENTION_WAIT_FAILED {
            log(b"drv-i2c-hid: reset sync wait failed\n");
            kumo_rt::process_exit(1);
        } else if reset_fires != ATTENTION_WAIT_TIMEOUT {
            if let Err(error) = controller.read(
                config.i2c_address,
                &mut input_frame[..reset_input_frame_len],
            ) {
                log_hex(b"drv-i2c-hid: reset sync read error=0x", error.code());
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
                &input_frame[..reset_input_frame_len.min(16)],
            );
        } else {
            log(b"drv-i2c-hid: reset sync timeout\n");
        }
    }
    if !quirks.no_wakeup_after_reset {
        if let Err(error) = controller.write(
            config.i2c_address,
            &Command::set_power(descriptor.command_register, PowerState::On),
        ) {
            log_hex(b"drv-i2c-hid: post-reset set-power error=0x", error.code());
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
            error.code(),
        );
        kumo_rt::process_exit(1);
    }
    let report_info = match inspect_report_descriptor(&report_descriptor[..report_descriptor_len]) {
        Ok(info) => info,
        Err(error) => {
            log_hex(
                b"drv-i2c-hid: report descriptor parse error=0x",
                error as u64,
            );
            kumo_rt::process_exit(1);
        }
    };
    let keyboard = report_info.keyboard;

    log(b"drv-i2c-hid: report descriptor ok\n");
    log_hex(
        b"drv-i2c-hid: descriptor-max-input=0x",
        descriptor.max_input_length as u64,
    );
    match keyboard.report_id {
        Some(report_id) => log_hex(b"drv-i2c-hid: keyboard-report-id=0x", report_id as u64),
        None => log(b"drv-i2c-hid: keyboard-report-id=none\n"),
    }
    log_hex(
        b"drv-i2c-hid: report-max-input=0x",
        report_info.max_input_frame_bytes as u64,
    );

    let input_frame_len = match input_read_len(
        descriptor.max_input_length,
        report_info.max_input_frame_bytes,
    ) {
        Ok(length) => length,
        Err(error) => {
            log_input_probe_error(error);
            kumo_rt::process_exit(1);
        }
    };
    log_hex(b"drv-i2c-hid: irq read size=0x", input_frame_len as u64);

    if quirks.re_power_on {
        if let Err(error) = controller.write(
            config.i2c_address,
            &Command::set_power(descriptor.command_register, PowerState::On),
        ) {
            log_hex(b"drv-i2c-hid: re-power-on error=0x", error.code());
            kumo_rt::process_exit(1);
        }
        if !sleep_ns(POWER_ON_SETTLE_NS) {
            log(b"drv-i2c-hid: re-power-on settle wait failed\n");
            kumo_rt::process_exit(1);
        }
    }

    let mut input_decoder = InputProbeDecoder::new();
    log(b"drv-i2c-hid: attention interrupt ready\n");
    // Steady-state runs SILENT, like a Linux input driver: no per-key / per-frame logging, so the
    // console shows the shell's own echo of what you type rather than driver spam. Only exceptional
    // paths log (bounded), plus a one-shot `keyboard input live` marker on the first forwarded key so
    // a fresh flash can still tell "forwarding works" from "echo path broken". — CORVUS
    // Keep GPIO attention first, but do not let a missing TLMM/PDC delivery park the only keyboard
    // producer forever. The 20 ms timeout restores J267's timer-paced plain-read bridge while still
    // completing real DESIGN/016 deliveries after a successful drain. — KESTREL
    let mut input_decode_failures = BoundedFailureLog::new();
    let mut keyboard_forward_failures = BoundedFailureLog::new();
    let mut non_keyboard_reports = BoundedFailureLog::new();
    let mut poll_read_failures = BoundedFailureLog::new();
    let mut logged_first_key = false;
    let mut logged_first_attention = false;
    let mut logged_first_read = false;
    let mut logged_poll_fallback = false;
    loop {
        let fires = wait_attention_or_timeout(attention_irq, STEADY_POLL_FALLBACK_NS);
        if fires == ATTENTION_WAIT_FAILED {
            log(b"drv-i2c-hid: attention wait failed\n");
            kumo_rt::process_exit(1);
        }
        let attention_fired = fires != ATTENTION_WAIT_TIMEOUT;
        if attention_fired && !logged_first_attention {
            log_hex(b"drv-i2c-hid: attention fired count=0x", fires);
        } else if !attention_fired && !logged_poll_fallback {
            logged_poll_fallback = true;
            log(b"drv-i2c-hid: attention poll fallback active\n");
        }
        if !logged_first_read {
            log(b"drv-i2c-hid: input read begin\n");
        }

        // Fetch the input report with a PLAIN read (Linux i2c_hid_get_input → i2c_master_recv);
        // addressing the input register first returns the device's "no data" response instead of
        // the pending report — which is why every earlier poll came back empty. — CORVUS
        if let Err(error) = controller.read(config.i2c_address, &mut input_frame[..input_frame_len])
        {
            if attention_fired {
                log_hex(b"drv-i2c-hid: input frame read error=0x", error.code());
                kumo_rt::process_exit(1);
            }
            if poll_read_failures.record() {
                log_hex(b"drv-i2c-hid: poll read error=0x", error.code());
            }
            continue;
        }
        if interrupt_complete(attention_irq) != 0 {
            log(b"drv-i2c-hid: attention complete failed\n");
            kumo_rt::process_exit(1);
        }
        if attention_fired {
            logged_first_attention = true;
        }
        if !logged_first_read {
            logged_first_read = true;
            log(b"drv-i2c-hid: input read ok\n");
        }
        let input = match input_decoder.decode_report_with_quirks(
            &input_frame[..input_frame_len],
            keyboard.report_id,
            quirks,
        ) {
            Ok(DecodedReport::Keyboard(input)) => input,
            // Reset/empty/bogus-IRQ frame: nothing to forward, not an error.
            Ok(DecodedReport::Empty) => continue,
            // A valid input report for another collection (the Elan keyboard@68 also speaks
            // consumer / system-control reports). Linux's `hid_input_report` routes these to their
            // own report; KUMO owns only the keyboard, so this is a benign skip. Log a bounded
            // sample of the foreign report IDs so a metal flash shows what the device interleaves,
            // then continue. — CORVUS
            Ok(DecodedReport::NonKeyboard { report_id }) => {
                if non_keyboard_reports.record() {
                    log_hex(b"drv-i2c-hid: non-keyboard report id=0x", report_id as u64);
                }
                continue;
            }
            Err(error) => {
                // A malformed *keyboard* report (rollover, truncated) once the device is live is a
                // dropped report, not a reason to kill the hardware driver. — KESTREL
                if input_decode_failures.record() {
                    log_input_probe_error(error);
                }
                continue;
            }
        };
        if let Some(ascii) = input.first_pressed_ascii {
            let byte = [ascii];
            if channel_write(keyboard_channel, byte.as_ptr(), byte.len()) != 0 {
                // A closed/restarting keyboard consumer is soft-state loss, not a hardware-driver
                // death. Keep the IRQ loop alive and drop the byte per DESIGN/002. — KESTREL
                if keyboard_forward_failures.record() {
                    log_hex(
                        b"drv-i2c-hid: keyboard byte dropped count=0x",
                        keyboard_forward_failures.count() as u64,
                    );
                }
            } else if !logged_first_key {
                // One-shot proof that the first decoded keystroke reached the keyboard channel; the
                // shell's echo carries every key after this. — CORVUS
                logged_first_key = true;
                log(b"drv-i2c-hid: keyboard input live\n");
            }
        }
    }
}
