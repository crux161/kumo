#![no_std]
#![no_main]
//j928
//! `media redoxprobe write|verify`: the SH8-b gate over the volume session Sora grants.
//! Prints one `REDOXPROBE <PHASE> PASS` or `REDOXPROBE <PHASE> FAIL at <step>` line.

use kumo_abi::{unpack_argv, Handle};
use kumo_rt::{channel_read, channel_write, debug_write, process_exit, startup, vmo_read};
use redoxprobe::{verify_phase, write_phase, Vfs};

kumo_rt::entry!(main);

struct Channel(Handle);

impl Vfs for Channel {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Option<usize> {
        if channel_write(self.0, request.as_ptr(), request.len()) != 0 {
            return None;
        }
        let n = channel_read(self.0, reply.as_mut_ptr(), reply.len());
        (n != 0 && n != u64::MAX && n <= reply.len() as u64).then_some(n as usize)
    }
}

fn emit(stdout: Handle, parts: &[&[u8]]) {
    let mut line = [0u8; 128];
    let mut len = 0;
    for part in parts {
        let n = part.len().min(line.len() - len);
        line[len..len + n].copy_from_slice(&part[..n]);
        len += n;
    }
    if stdout.0 == 0 {
        debug_write(line.as_ptr(), len);
    } else {
        let _ = channel_write(stdout, line.as_ptr(), len);
    }
}

#[no_mangle]
extern "C" fn main(
    bootstrap: u64,
    _a2: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
    _a7: u64,
    _a8: u64,
) -> ! {
    let startup = startup(Handle(bootstrap as u32));
    let stdout = startup.stdout.unwrap_or(Handle(0));
    let mut argv = [0u8; 256];
    if let Some(handle) = startup.argv {
        if vmo_read(handle, 0, argv.as_mut_ptr(), argv.len()) != 0 {
            emit(stdout, &[b"redoxprobe: argv read failed\n"]);
            process_exit(2);
        }
    }
    let phase = unpack_argv(&argv).nth(1).unwrap_or(b"");
    let Some(fs) = startup.fs else {
        emit(
            stdout,
            &[b"REDOXPROBE FAIL: no filesystem granted (run it with `media`)\n"],
        );
        process_exit(2);
    };
    let (name, result): (&[u8], _) = match phase {
        b"write" => (b"WRITE", write_phase(&mut Channel(fs))),
        b"verify" => (b"VERIFY", verify_phase(&mut Channel(fs))),
        _ => {
            emit(stdout, &[b"usage: redoxprobe write|verify\n"]);
            process_exit(2);
        }
    };
    match result {
        Ok(()) => {
            emit(stdout, &[b"REDOXPROBE ", name, b" PASS\n"]);
            process_exit(0)
        }
        Err(step) => {
            emit(
                stdout,
                &[b"REDOXPROBE ", name, b" FAIL at ", step.as_bytes(), b"\n"],
            );
            process_exit(1)
        }
    }
}
