#![no_std]
#![no_main]
//j928
//j929
//! `media redoxprobe write|verify|crash|verify-crash`: the SH8-b and SH8-c gates over the volume
//! session Sora grants. Prints one `REDOXPROBE <PHASE> PASS` or `REDOXPROBE <PHASE> FAIL at
//! <step>` line. `crash` runs through a reconnecting session over the mount's control endpoint
//! (`fs_mount`), which Sora grants with a read-write `media` launch of a supervised mount.

use kumo_abi::{unpack_argv, Handle, Rights};
use kumo_rt::{
    channel_create_pair, channel_read, channel_write, channel_write_with_handle, debug_write,
    handle_close, handle_duplicate, process_exit, startup, vmo_create, vmo_read, vmo_write,
};
use kumo_vfs::reconnect::{LinkError, Reconnecting, SessionLink};
use redoxprobe::{crash_phase, verify_crash_phase, verify_phase, write_phase, CrashHost, Vfs};

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

/// The crash phase's session: the granted one first, then any it mints over `fs_mount`.
struct Link {
    session: Handle,
    control: Handle,
}

impl SessionLink for Link {
    fn exchange(&mut self, request: &[u8], vmo: u32, reply: &mut [u8]) -> Result<usize, LinkError> {
        let sent = if vmo == 0 {
            channel_write(self.session, request.as_ptr(), request.len())
        } else {
            // A copy travels, so a retry can carry the VMO again.
            let copy = handle_duplicate(
                Handle(vmo),
                Rights(Rights::READ.0 | Rights::WRITE.0 | Rights::TRANSFER.0),
            );
            if copy == 0 || copy == u64::MAX {
                return Err(LinkError::Failed);
            }
            let status = channel_write_with_handle(
                self.session,
                request.as_ptr(),
                request.len(),
                Handle(copy as u32),
            );
            if status != 0 {
                let _ = handle_close(Handle(copy as u32));
            }
            status
        };
        if sent != 0 {
            return Err(LinkError::Closed);
        }
        let n = channel_read(self.session, reply.as_mut_ptr(), reply.len());
        if n == 0 || n == u64::MAX {
            return Err(LinkError::Closed);
        }
        if n > reply.len() as u64 {
            return Err(LinkError::Failed);
        }
        Ok(n as usize)
    }

    fn reconnect(&mut self) -> bool {
        if self.control.0 == 0 {
            return false;
        }
        let (client, server) = channel_create_pair();
        if client == u64::MAX || server == u64::MAX {
            return false;
        }
        let grant = [kumo_vfs::SESSION_GRANT, kumo_vfs::SESSION_ACCESS_READ_WRITE];
        if channel_write_with_handle(
            self.control,
            grant.as_ptr(),
            grant.len(),
            Handle(server as u32),
        ) != 0
        {
            let _ = handle_close(Handle(server as u32));
            let _ = handle_close(Handle(client as u32));
            return false;
        }
        let _ = handle_close(self.session);
        self.session = Handle(client as u32);
        true
    }
}

struct Host;

impl CrashHost for Host {
    fn pause(&mut self) {
        let _ = kumo_rt::sleep_ns(20_000_000);
    }

    fn vmo_filled(&mut self, len: usize, fill: &dyn Fn(&mut [u8], u64)) -> u32 {
        let raw = vmo_create(len as u64);
        if raw == 0 || raw == u64::MAX {
            return 0;
        }
        let mut piece = [0u8; 256];
        let mut at = 0;
        while at < len {
            let n = piece.len().min(len - at);
            fill(&mut piece[..n], at as u64);
            if vmo_write(Handle(raw as u32), at as u64, piece.as_ptr(), n) != 0 {
                let _ = handle_close(Handle(raw as u32));
                return 0;
            }
            at += n;
        }
        raw as u32
    }

    fn release(&mut self, vmo: u32) {
        let _ = handle_close(Handle(vmo));
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
        b"verify-crash" => (b"VERIFY-CRASH", verify_crash_phase(&mut Channel(fs))),
        b"crash" => {
            let Some(control) = startup.fs_mount else {
                emit(
                    stdout,
                    &[b"REDOXPROBE CRASH FAIL at no control endpoint granted\n"],
                );
                process_exit(1);
            };
            let mut rc = Reconnecting::<Link, 8>::new(Link {
                session: fs,
                control,
            });
            let result = crash_phase(&mut rc, &mut Host);
            if let Ok(sessions) = result {
                let mut digit = [b'0' + (sessions.min(9)) as u8, b'\n'];
                if sessions > 9 {
                    digit[0] = b'+';
                }
                emit(
                    stdout,
                    &[b"redoxprobe: reconnected; new sessions minted: ", &digit],
                );
            }
            (b"CRASH", result.map(|_| ()))
        }
        _ => {
            emit(
                stdout,
                &[b"usage: redoxprobe write|verify|crash|verify-crash\n"],
            );
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
