#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

use drv_geni_i2c::ProbeConfig;
use kumo_abi::{
    i2c::{
        I2cFuncsResponse, I2cOpcode, I2cRequestHeader, I2cSmbusReadByteResponse,
        I2cSmbusWriteQuickResponse, I2C_FUNC_I2C, I2C_FUNC_SMBUS_QUICK, I2C_FUNC_SMBUS_READ_BYTE, I2C_FUNC_I2C_TRANSFER,
    },
    Handle, VmarFlags,
};
use kumo_geni_i2c::{Controller, RegisterIo};
use kumo_rt::{
    channel_read_with_handle, channel_write, debug_write, handle_close, handle_koid, port_bind,
    port_create, port_wait, process_exit, resource_mint_mmio, vmar_map,
};

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

unsafe fn any_as_u8_slice<T: Sized>(p: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts((p as *const T) as *const u8, core::mem::size_of::<T>()) }
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
    log(b"drv-geni-i2c: starting\n");
    let bootstrap = Handle(bootstrap_channel as u32);
    let mut raw = [0u8; ProbeConfig::BYTES];
    let (received, resource_raw) = channel_read_with_handle(bootstrap, raw.as_mut_ptr(), raw.len());
    if received != raw.len() || resource_raw == 0 {
        log(b"drv-geni-i2c: bootstrap failed\n");
        process_exit(1);
    }
    let config = match ProbeConfig::decode(&raw) {
        Ok(config) => config,
        Err(_) => {
            log(b"drv-geni-i2c: config invalid\n");
            process_exit(1);
        }
    };
    log(b"drv-geni-i2c: config ok\n");

    let resource = Handle(resource_raw as u32);
    let vmo_raw = resource_mint_mmio(resource, config.mmio_base, config.mmio_length);
    if vmo_raw == u64::MAX {
        log(b"drv-geni-i2c: MMIO mint failed\n");
        process_exit(1);
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
        log(b"drv-geni-i2c: MMIO map failed\n");
        process_exit(1);
    }
    log(b"drv-geni-i2c: MMIO mapped\n");

    let registers = MmioRegisters {
        base: MMIO_VA as *mut u8,
    };
    let mut controller = match Controller::new(registers, config.source_clock, POLL_LIMIT) {
        Ok(controller) => controller,
        Err(error) => {
            log_hex(b"drv-geni-i2c: GENI init error=0x", error.code());
            process_exit(1);
        }
    };
    log(b"drv-geni-i2c: GENI FIFO ready\n");

    // Wait for the serve channel client from sora (P7-g style)
    let mut tag = [0u8; 1];
    let (received, serve_raw) = channel_read_with_handle(bootstrap, tag.as_mut_ptr(), tag.len());
    if received != tag.len() || tag[0] != b'S' || serve_raw == 0 {
        log(b"drv-geni-i2c: serve channel missing\n");
        process_exit(1);
    }
    let serve_channel = Handle(serve_raw as u32);
    log(b"drv-geni-i2c: serve channel ok\n");

    let port_raw = port_create();
    if port_raw == u64::MAX {
        log(b"drv-geni-i2c: port failed\n");
        process_exit(1);
    }
    let port = Handle(port_raw as u32);
    if port_bind(port, serve_channel) != 0 {
        log(b"drv-geni-i2c: bind failed\n");
        process_exit(1);
    }
    let serve_koid = handle_koid(serve_channel);
    log(b"drv-geni-i2c: ready\n");

    loop {
        let source = port_wait(port);
        if source != serve_koid {
            continue;
        }

        let mut req_buf = [0u8; core::mem::size_of::<I2cRequestHeader>()];
        let (n, response_handle_raw) =
            channel_read_with_handle(serve_channel, req_buf.as_mut_ptr(), req_buf.len());
        if n == 0 || response_handle_raw == 0 {
            continue;
        }
        let response_handle = Handle(response_handle_raw as u32);

        if n < core::mem::size_of::<I2cRequestHeader>() {
            let _ = handle_close(response_handle);
            continue;
        }

        let req: I2cRequestHeader =
            unsafe { core::ptr::read_unaligned(req_buf.as_ptr() as *const _) };
        let opcode = req.opcode as u32;
        let address = req.address as u8;

        if opcode == I2cOpcode::Funcs as u32 {
            let resp = I2cFuncsResponse {
                status: 0,
                funcs: I2C_FUNC_I2C | I2C_FUNC_SMBUS_QUICK | I2C_FUNC_SMBUS_READ_BYTE | I2C_FUNC_I2C_TRANSFER,
            };
            let out = unsafe { any_as_u8_slice(&resp) };
            let _ = channel_write(response_handle, out.as_ptr(), out.len());
        } else if opcode == I2cOpcode::SmbusWriteQuick as u32 {
            let status = match controller.write(address, &[]) {
                Ok(_) => 0,
                Err(e) => -(e.code() as i32),
            };
            let resp = I2cSmbusWriteQuickResponse { status };
            let out = unsafe { any_as_u8_slice(&resp) };
            let _ = channel_write(response_handle, out.as_ptr(), out.len());
        } else if opcode == I2cOpcode::SmbusReadByte as u32 {
            let mut byte = [0u8; 1];
            let status = match controller.read(address, &mut byte) {
                Ok(_) => 0,
                Err(e) => -(e.code() as i32),
            };
            let resp = I2cSmbusReadByteResponse {
                status,
                value: byte[0],
                _pad: [0; 3],
            };
            let out = unsafe { any_as_u8_slice(&resp) };
            let _ = channel_write(response_handle, out.as_ptr(), out.len());
        } else if opcode == I2cOpcode::Transfer as u32 {
            if n >= core::mem::size_of::<kumo_abi::i2c::I2cTransferRequest>() {
                let treq: kumo_abi::i2c::I2cTransferRequest = unsafe { core::ptr::read_unaligned(req_buf.as_ptr() as *const _) };
                let payload_start = core::mem::size_of::<kumo_abi::i2c::I2cTransferRequest>();
                let payload_len = treq.write_len as usize;
                
                let mut status = 0;
                let mut out_buf = [0u8; 512]; // reasonable max for KUMO I2C
                
                if payload_start + payload_len <= n && treq.read_len as usize <= out_buf.len() {
                    let write_data = &req_buf[payload_start..payload_start + payload_len];
                    let read_data = &mut out_buf[..treq.read_len as usize];
                    
                    let res = if treq.write_len > 0 && treq.read_len > 0 {
                        controller.write_read(address, write_data, read_data)
                    } else if treq.write_len > 0 {
                        controller.write(address, write_data)
                    } else if treq.read_len > 0 {
                        controller.read(address, read_data)
                    } else {
                        Ok(())
                    };
                    
                    status = match res {
                        Ok(_) => 0,
                        Err(e) => -(e.code() as i32),
                    };
                } else {
                    status = -1; // EINVAL
                }
                
                let resp = kumo_abi::i2c::I2cTransferResponse {
                    status,
                    read_len: if status == 0 { treq.read_len } else { 0 },
                    _pad: 0,
                };
                
                let mut resp_msg = [0u8; 512 + core::mem::size_of::<kumo_abi::i2c::I2cTransferResponse>()];
                let resp_hdr = unsafe { any_as_u8_slice(&resp) };
                resp_msg[..resp_hdr.len()].copy_from_slice(resp_hdr);
                
                if status == 0 && treq.read_len > 0 {
                    resp_msg[resp_hdr.len()..resp_hdr.len() + treq.read_len as usize]
                        .copy_from_slice(&out_buf[..treq.read_len as usize]);
                }
                
                let _ = channel_write(response_handle, resp_msg.as_ptr(), resp_hdr.len() + resp.read_len as usize);
            }
        } else {
            // Unknown opcode, ignore.
        }

        let _ = handle_close(response_handle);
    }
}
// — OSPREY 2026-06-26 (d007)
