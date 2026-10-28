//j643
//j644

//! Compile/link acceptance, with the real Session codec and modeled hardware only.
//! The native service and Mesa rendering remain later acceptance gates. — KESTREL
#![no_std]

use core::cell::Cell;
use kumo_gpu::{
    c::{with_api, Api, Info, DEVICE_LOST, PROTOCOL, SERVER_ERROR},
    client::{Client, Transport, TransportError},
    session::{Backend, Session, SubmitError},
    Binding, DeviceInfo, DeviceTopology, Reply, Request, Status, Submission, REPLY_LEN,
    REQUEST_LEN,
};

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[repr(C)]
#[derive(Default)]
struct Report {
    info: Info,
    missing_properties: u64,
    query_status: u32,
    reserved: u32,
    core_count: u32,
    core_id_range: u32,
}
extern "C" {
    fn kumo_mesa_probe(api: *const Api, out: *mut Report) -> u32;
}
const _: () = assert!(core::mem::size_of::<Report>() == 64);
const _: () = assert!(core::mem::size_of::<Api>() == 24);
const INFO: DeviceInfo = DeviceInfo {
    gpu_id: 0xa8670005,
    cs_features: 0x7085f,
};

struct Hardware {
    busy_once: bool,
    shader_present: u64,
}
impl Backend for Hardware {
    fn info(&mut self) -> Result<DeviceInfo, Status> {
        if core::mem::replace(&mut self.busy_once, false) {
            Err(Status::Busy)
        } else {
            Ok(INFO)
        }
    }
    fn topology(&mut self) -> Result<DeviceTopology, Status> {
        Ok(DeviceTopology {
            shader_present: self.shader_present,
        })
    }
    fn create_vm(&mut self) -> Result<(), Status> {
        Err(Status::NotSupported)
    }
    fn create_buffer(&mut self, _: u32, _: u64) -> Result<(), Status> {
        Err(Status::NotSupported)
    }
    fn bind(&mut self, _: Binding) -> Result<(), Status> {
        Err(Status::NotSupported)
    }
    fn create_group(&mut self) -> Result<(), Status> {
        Err(Status::NotSupported)
    }
    fn submit(&mut self, _: Submission) -> Result<(), SubmitError> {
        Err(SubmitError::Unpublished(Status::NotSupported))
    }
}
struct Loopback<'a> {
    session: Session,
    hardware: Hardware,
    reply: [u8; REPLY_LEN],
    sends: &'a Cell<u32>,
    closed: &'a Cell<u32>,
    rejected_grant: bool,
}
impl Transport for Loopback<'_> {
    type Handle = u32;
    fn send(&mut self, bytes: &[u8; REQUEST_LEN]) -> Result<(), TransportError> {
        let request = Request::decode(bytes).ok_or(TransportError::Failed)?;
        let expected = self.sends.get() + 1;
        if request.id != expected {
            return Err(TransportError::Failed);
        }
        self.sends.set(expected);
        self.reply = Reply {
            id: request.id,
            result: self.session.dispatch(request, &mut self.hardware),
        }
        .encode();
        Ok(())
    }
    fn receive(
        &mut self,
        bytes: &mut [u8; REPLY_LEN + 1],
    ) -> Result<(usize, Option<u32>), TransportError> {
        bytes[..REPLY_LEN].copy_from_slice(&self.reply);
        Ok((REPLY_LEN, self.rejected_grant.then_some(77)))
    }
    fn close_handle(&mut self, handle: u32) {
        if handle == 77 {
            self.closed.set(self.closed.get() + 1);
        }
    }
}
fn transport<'a>(
    sends: &'a Cell<u32>,
    closed: &'a Cell<u32>,
    busy_once: bool,
    rejected_grant: bool,
) -> Loopback<'a> {
    Loopback {
        session: Session::default(),
        hardware: Hardware {
            busy_once,
            shader_present: 0x500000005,
        },
        reply: [0; REPLY_LEN],
        sends,
        closed,
        rejected_grant,
    }
}

#[no_mangle]
pub extern "C" fn kumo_mesa_bridge_proof() -> u32 {
    let sends = Cell::new(0);
    let closed = Cell::new(0);
    let mut client = Client::new(transport(&sends, &closed, true, false));
    let result = with_api(&mut client, |api| {
        let mut report = Report::default();
        // A rejected old ABI must not write the larger version2 report into an
        // old caller's allocation. The extra sentinel words are guard storage. — KESTREL
        let old_api = Api { version: 1, ..*api };
        let mut guarded = [0xfeedbeefcafebabeu64; 8];
        if unsafe { kumo_mesa_probe(&old_api, guarded.as_mut_ptr().cast()) } != 2
            || guarded != [0xfeedbeefcafebabeu64; 8]
            || sends.get() != 0
        {
            return 11;
        }
        // The C-side version/size/null checks must perform no query I/O. — KESTREL
        for bad in [
            Api { version: 1, ..*api },
            Api {
                bytes: api.bytes - 1,
                ..*api
            },
            Api {
                query: None,
                ..*api
            },
            Api {
                context: core::ptr::null_mut(),
                ..*api
            },
        ] {
            if unsafe { kumo_mesa_probe(&bad, &mut report) } != 2 || sends.get() != 0 {
                return 1;
            }
        }
        if unsafe { kumo_mesa_probe(core::ptr::null(), &mut report) } != 2
            || unsafe { kumo_mesa_probe(api, core::ptr::null_mut()) } != 2
            || sends.get() != 0
        {
            return 2;
        }
        if unsafe { kumo_mesa_probe(api, &mut report) } != 3
            || report.query_status != SERVER_ERROR + Status::Busy as u32
            || report.info.gpu_id != 0
            || sends.get() != 1
        {
            return 3;
        }
        for n in [3, 5] {
            if unsafe { kumo_mesa_probe(api, &mut report) } != 1
                || report.info.gpu_id != INFO.gpu_id
                || report.info.cs_features != INFO.cs_features
                || report.info.va_start != kumo_gpu::VA_START
                || report.info.va_end != kumo_gpu::VA_END
                || report.info.max_buffer_bytes != kumo_gpu::MAX_BUFFER_BYTES
                || report.info.shader_present != 0x500000005
                || report.core_count != 4
                || report.core_id_range != 35
                || report.missing_properties != 1023
                || report.query_status != 0
                || report.reserved != 0
                || sends.get() != n
            {
                return 4;
            }
        }
        0
    });
    if result != 0 {
        return result;
    }
    if client.is_lost() || client.info() != Ok(INFO) || sends.get() != 6 || closed.get() != 0 {
        return 5;
    }

    for (mask, count, range) in [
        (1, 1, 1),
        (0xf, 4, 4),
        (0x8000000000000001, 2, 64),
        (u64::MAX, 64, 64),
    ] {
        sends.set(0);
        let mut wire = transport(&sends, &closed, false, false);
        wire.hardware.shader_present = mask;
        let mut client = Client::new(wire);
        let ok = with_api(&mut client, |api| {
            let mut report = Report::default();
            (unsafe { kumo_mesa_probe(api, &mut report) }) == 1
                && report.info.shader_present == mask
                && report.core_count == count
                && report.core_id_range == range
                && sends.get() == 2
        });
        if !ok {
            return 9;
        }
    }

    // Zero is an invalid attestation, not a one-core fallback. The server retires;
    // C sees failure with cleared output and cannot issue another request. — KESTREL
    sends.set(0);
    let mut wire = transport(&sends, &closed, false, false);
    wire.hardware.shader_present = 0;
    let mut client = Client::new(wire);
    let ok = with_api(&mut client, |api| {
        let mut report = Report::default();
        (unsafe { kumo_mesa_probe(api, &mut report) }) == 3
            && report.query_status == SERVER_ERROR + Status::DeviceLost as u32
            && report.info.gpu_id == 0
            && report.info.shader_present == 0
            && report.core_count == 0
            && report.core_id_range == 0
    });
    if !ok || !client.is_lost() || sends.get() != 2 {
        return 10;
    }

    sends.set(0);
    let mut client = Client::new(transport(&sends, &closed, false, true));
    let result = with_api(&mut client, |api| {
        let mut report = Report::default();
        if unsafe { kumo_mesa_probe(api, &mut report) } != 3
            || report.query_status != PROTOCOL
            || report.info.gpu_id != 0
            || sends.get() != 1
            || closed.get() != 1
        {
            return 6;
        }
        if unsafe { kumo_mesa_probe(api, &mut report) } != 3
            || report.query_status != DEVICE_LOST
            || sends.get() != 1
            || closed.get() != 1
        {
            return 7;
        }
        0
    });
    if result != 0 {
        return result;
    }
    if !client.is_lost() {
        return 8;
    }
    0
}
