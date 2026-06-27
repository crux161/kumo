#![no_std]
#![no_main]

use kumo_abi::i2c::{I2cOpcode, I2cRequestHeader, I2cSmbusWriteQuickResponse};
use kumo_abi::Handle;
use kumo_rt::{
    channel_create_pair, channel_read_with_handle, channel_write_with_handle, debug_write,
    handle_close, handle_koid, port_bind, port_create, port_wait, process_exit,
};

kumo_rt::entry!(main);

fn log(msg: &[u8]) {
    debug_write(msg.as_ptr(), msg.len());
}

fn probe_address(i2c_client: Handle, addr: u8) -> bool {
    let req = I2cRequestHeader {
        opcode: I2cOpcode::SmbusWriteQuick,
        bus: 0,
        address: addr as u16,
        _pad: 0,
    };

    let req_bytes = unsafe {
        core::slice::from_raw_parts(
            &req as *const _ as *const u8,
            core::mem::size_of::<I2cRequestHeader>(),
        )
    };

    let port_raw = port_create();
    if port_raw == u64::MAX {
        return false;
    }
    let port = Handle(port_raw as u32);

    let (local_resp_raw, remote_resp_raw) = channel_create_pair();
    if local_resp_raw == u64::MAX || remote_resp_raw == u64::MAX {
        let _ = handle_close(port);
        if local_resp_raw != u64::MAX {
            let _ = handle_close(Handle(local_resp_raw as u32));
        }
        if remote_resp_raw != u64::MAX {
            let _ = handle_close(Handle(remote_resp_raw as u32));
        }
        return false;
    }
    let local_resp = Handle(local_resp_raw as u32);
    let remote_resp = Handle(remote_resp_raw as u32);

    if port_bind(port, local_resp) != 0 {
        let _ = handle_close(port);
        let _ = handle_close(local_resp);
        let _ = handle_close(remote_resp);
        return false;
    }
    let local_resp_koid = handle_koid(local_resp);
    if local_resp_koid == u64::MAX {
        let _ = handle_close(port);
        let _ = handle_close(local_resp);
        let _ = handle_close(remote_resp);
        return false;
    }

    if channel_write_with_handle(i2c_client, req_bytes.as_ptr(), req_bytes.len(), remote_resp) != 0
    {
        let _ = handle_close(port);
        let _ = handle_close(local_resp);
        let _ = handle_close(remote_resp);
        return false;
    }

    let source = port_wait(port);
    let _ = handle_close(port);
    if source != local_resp_koid {
        let _ = handle_close(local_resp);
        return false;
    }

    let mut resp_buf = [0u8; core::mem::size_of::<I2cSmbusWriteQuickResponse>()];
    let (n, extra_raw) =
        channel_read_with_handle(local_resp, resp_buf.as_mut_ptr(), resp_buf.len());
    let _ = handle_close(local_resp);
    if extra_raw != 0 {
        let _ = handle_close(Handle(extra_raw as u32));
    }

    if n != core::mem::size_of::<I2cSmbusWriteQuickResponse>() {
        return false;
    }
    let resp = unsafe {
        core::ptr::read_unaligned(resp_buf.as_ptr() as *const I2cSmbusWriteQuickResponse)
    };
    resp.status == 0
}

#[no_mangle]
extern "C" fn main(
    _a0: u64,
    bootstrap_raw: u64,
    _a2: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
    _a7: u64,
) -> ! {
    let bootstrap = Handle(bootstrap_raw as u32);
    log(b"i2cdetect: starting up\n");

    // Wait for the I2C client channel
    let mut tag = [0u8; 1];
    let (received, i2c_client_raw) =
        channel_read_with_handle(bootstrap, tag.as_mut_ptr(), tag.len());
    if received != tag.len() || tag[0] != b'I' || i2c_client_raw == 0 {
        log(b"i2cdetect: bootstrap failed\n");
        process_exit(1);
    }
    let i2c_client = Handle(i2c_client_raw as u32);
    log(b"i2cdetect: channel ok\n");

    // We scan 0x03 to 0x77
    log(b"     0  1  2  3  4  5  6  7  8  9  a  b  c  d  e  f\n");
    for i in (0..128).step_by(16) {
        let mut line = [b' '; 56]; // 16 * 3 + 8 = 56 bytes

        // Convert 'i' to hex for prefix
        line[0] = hex_char((i >> 4) as u8);
        line[1] = hex_char((i & 0xf) as u8);
        line[2] = b':';
        line[3] = b' ';

        for j in 0..16 {
            let addr = i + j;
            let offset = 4 + (j as usize) * 3;

            if addr < 0x03 || addr > 0x77 {
                line[offset] = b' ';
                line[offset + 1] = b' ';
                line[offset + 2] = b' ';
                continue;
            }

            if probe_address(i2c_client, addr as u8) {
                line[offset] = hex_char((addr >> 4) as u8);
                line[offset + 1] = hex_char((addr & 0xf) as u8);
            } else {
                line[offset] = b'-';
                line[offset + 1] = b'-';
            }
            line[offset + 2] = b' ';
        }

        // Add newline and null terminator, then log
        line[52] = b'\n';
        line[53] = 0;

        log(&line[..53]);
    }

    process_exit(0)
}

fn hex_char(val: u8) -> u8 {
    if val < 10 {
        b'0' + val
    } else {
        b'a' + (val - 10)
    }
}
