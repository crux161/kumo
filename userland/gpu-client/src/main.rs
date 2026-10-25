#![no_std]
#![no_main]
//j595

//! One cooperative session granted by Sora in x0, with no Resource, MMIO or firmware handle.
//! Recovery class: soft-state. On any error exit and let drv-gpu contain the session.

use core::fmt::{self, Write};
use gpu_client::{proof_stream, VALUES};
use kumo_abi::{Handle, VmarFlags};
use kumo_gpu::{
    BindFlags, Binding, Command, Reply, Request, Response, Submission, SyncState, PAGE_BYTES,
    REPLY_LEN, VA_START,
};

kumo_rt::entry!(main);

struct Text {
    bytes: [u8; 256],
    len: usize,
}
impl Write for Text {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self
            .len
            .checked_add(text.len())
            .filter(|end| *end <= self.bytes.len())
            .ok_or(fmt::Error)?;
        self.bytes[self.len..end].copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}
fn log(args: fmt::Arguments<'_>) {
    let mut text = Text {
        bytes: [0; 256],
        len: 0,
    };
    let _ = text.write_fmt(args);
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &text.bytes[..text.len]);
}
fn handle(raw: u64) -> Option<Handle> {
    (raw != 0 && (raw as i64) > 0).then_some(Handle(raw as u32))
}
fn barrier() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}
struct Client {
    channel: Handle,
    id: u32,
}
impl Client {
    fn call(&mut self, command: Command) -> Result<(Response, Option<Handle>), ()> {
        self.id += 1;
        let bytes = Request {
            id: self.id,
            command,
        }
        .encode();
        if kumo_rt::channel_write(self.channel, bytes.as_ptr(), bytes.len()) != 0 {
            return Err(());
        }
        let mut bytes = [0u8; REPLY_LEN + 1];
        let (len, raw) =
            kumo_rt::channel_read_with_handle(self.channel, bytes.as_mut_ptr(), bytes.len());
        let grant = handle(raw);
        let decoded = bytes.get(..len).and_then(Reply::decode);
        match decoded {
            Some(Reply {
                id,
                result: Ok(response),
            }) if id == self.id && response.handle_count() == usize::from(grant.is_some()) => {
                Ok((response, grant))
            }
            _ => {
                log(format_args!(
                    "gpu-client: request {} reply {:?} ({} bytes, handle={})\n",
                    self.id,
                    decoded,
                    len,
                    grant.is_some()
                ));
                if let Some(h) = grant {
                    let _ = kumo_rt::handle_close(h);
                }
                Err(())
            }
        }
    }
    fn simple(&mut self, command: Command) -> Result<Response, ()> {
        let (response, grant) = self.call(command)?;
        if let Some(h) = grant {
            let _ = kumo_rt::handle_close(h);
            return Err(());
        }
        Ok(response)
    }
}

fn run(channel: Handle) -> Result<(), ()> {
    let mut client = Client { channel, id: 0 };
    let Response::VmCreated { vm } = client.simple(Command::VmCreate)? else {
        return Err(());
    };
    let (response, grant) = client.call(Command::BufferCreate {
        bytes: 2 * PAGE_BYTES,
    })?;
    let Response::BufferCreated { buffer, bytes } = response else {
        return Err(());
    };
    if bytes != 2 * PAGE_BYTES {
        return Err(());
    }
    let vmo = grant.ok_or(())?;
    let (status, cpu) = kumo_rt::vmar_map_anywhere(
        vmo,
        bytes,
        (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
    );
    if status != 0 || cpu == 0 {
        return Err(());
    }
    for (offset, flags) in [
        (
            0,
            BindFlags::READ_ONLY | BindFlags::NO_EXECUTE | BindFlags::UNCACHED,
        ),
        (PAGE_BYTES, BindFlags::NO_EXECUTE | BindFlags::UNCACHED),
    ] {
        let b = Binding {
            vm,
            buffer,
            va: VA_START + offset,
            offset,
            bytes: PAGE_BYTES,
            flags: BindFlags(flags),
        };
        if client.simple(Command::VmBind(b))? != Response::VmBound {
            return Err(());
        }
    }
    let Response::GroupCreated { group } = client.simple(Command::GroupCreate { vm })? else {
        return Err(());
    };
    for (index, value) in VALUES.into_iter().enumerate() {
        let point = index as u64 + 1;
        let stream = proof_stream(VA_START + PAGE_BYTES, value);
        for (i, word) in stream.iter().enumerate() {
            // SAFETY: aligned words inside the shared 8 KiB Normal-NC mapping. This client
            // writes only before submission or after the previous point completed.
            unsafe {
                ((cpu + i as u64 * 8) as *mut u64).write_volatile(*word);
            }
        }
        unsafe {
            ((cpu + PAGE_BYTES) as *mut u32).write_volatile(0xfeed_face);
        }
        barrier();
        if client.simple(Command::Submit(Submission {
            group,
            stream_va: VA_START,
            stream_bytes: (stream.len() * 8) as u32,
            signal_point: point,
        }))? != (Response::Submitted { point })
        {
            return Err(());
        }
        let mut complete = false;
        for _ in 0..1000 {
            match client.simple(Command::SyncRead { group, point })? {
                Response::Sync {
                    point: p,
                    state: SyncState::Complete { gpu_status: 0 },
                } if p == point => {
                    complete = true;
                    break;
                }
                Response::Sync {
                    point: p,
                    state: SyncState::Pending,
                } if p == point => {
                    let _ = kumo_rt::sleep_ns(1_000_000);
                }
                _ => return Err(()),
            }
        }
        if !complete {
            return Err(());
        }
        barrier();
        // SAFETY: the target word of the retained mapping, read only after observed completion.
        let got = unsafe { ((cpu + PAGE_BYTES) as *const u32).read_volatile() };
        if got != value {
            log(format_args!(
                "gpu-client: point {} wrong value 0x{:08x}, expected 0x{:08x}\n",
                point, got, value
            ));
            return Err(());
        }
        log(format_args!(
            "gpu-client: point {} complete, GPU wrote 0x{:08x} into shared VMO\n",
            point, got
        ));
    }
    log(format_args!(
        "gpu-client: two submissions complete; shared values verified\n"
    ));
    let _ = kumo_rt::handle_close(vmo);
    Ok(())
}

#[no_mangle]
extern "C" fn main(channel: u64, _unused: u64) -> ! {
    let Some(channel) = handle(channel) else {
        kumo_rt::process_exit(1)
    };
    log(format_args!(
        "gpu-client: separate process starting GPU session\n"
    ));
    let result = run(channel);
    if result.is_err() {
        log(format_args!("gpu-client: session failed\n"));
    }
    let _ = kumo_rt::handle_close(channel);
    kumo_rt::process_exit(u64::from(result.is_err()))
}
