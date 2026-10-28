//j643
//j644
//j645

//! Compile/link acceptance, with the real Session codec and modeled hardware only.
//! The native service and Mesa rendering remain later acceptance gates. — KESTREL
#![no_std]

use core::cell::Cell;
use kumo_gpu::{
    c::{with_api, Api, Info, DEVICE_LOST, PROTOCOL, SERVER_ERROR},
    client::{Client, Transport, TransportError},
    session::{Backend, Session, SubmitError},
    Binding, DeviceInfo, DeviceMmu, DeviceTopology, Reply, Request, Status, Submission, REPLY_LEN,
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
    va_range_start: u64,
    va_range_size: u64,
}
extern "C" {
    fn kumo_mesa_probe(api: *const Api, out: *mut Report) -> u32;
}
const _: () = assert!(core::mem::size_of::<Report>() == 88);
const _: () = assert!(core::mem::size_of::<Api>() == 24);
const INFO: DeviceInfo = DeviceInfo {
    gpu_id: 0xa8670005,
    cs_features: 0x7085f,
};
/// What the Orange Pi attests: MMU_FEATURES 0x2830, whose [7:0] field is the 48-bit
/// virtual-address width upstream then turns into a 1<<48 user range. — SAKER
const MMU_FEATURES: u32 = 0x2830;
const MMU_VA_BITS: u32 = 48;
const MMU_VA_SIZE: u64 = 1 << 48;

struct Hardware {
    busy_once: bool,
    shader_present: u64,
    mmu_features: u32,
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
    fn mmu_features(&mut self) -> Result<DeviceMmu, Status> {
        Ok(DeviceMmu {
            mmu_features: self.mmu_features,
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
            mmu_features: MMU_FEATURES,
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
        // A rejected old ABI must not write the larger version3 report into an
        // old caller's allocation. The extra sentinel words are guard storage, and they
        // span more than the current report so an overrun is observed rather than
        // allowed to smash the stack. — KESTREL, guard widened for v3 — SAKER
        let old_api = Api { version: 2, ..*api };
        let mut guarded = [0xfeedbeefcafebabeu64; 16];
        if unsafe { kumo_mesa_probe(&old_api, guarded.as_mut_ptr().cast()) } != 2
            || guarded != [0xfeedbeefcafebabeu64; 16]
            || sends.get() != 0
        {
            return 11;
        }
        // The C-side version/size/null checks must perform no query I/O. — KESTREL
        for bad in [
            Api { version: 2, ..*api },
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
        for n in [4, 7] {
            if unsafe { kumo_mesa_probe(api, &mut report) } != 1
                || report.info.gpu_id != INFO.gpu_id
                || report.info.cs_features != INFO.cs_features
                || report.info.va_start != kumo_gpu::VA_START
                || report.info.va_end != kumo_gpu::VA_END
                || report.info.max_buffer_bytes != kumo_gpu::MAX_BUFFER_BYTES
                || report.info.shader_present != 0x500000005
                || report.info.mmu_features != MMU_FEATURES
                || report.core_count != 4
                || report.core_id_range != 35
                || report.va_range_start != 0
                || report.va_range_size != MMU_VA_SIZE
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
    if client.is_lost() || client.info() != Ok(INFO) || sends.get() != 8 || closed.get() != 0 {
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
                && report.va_range_size == MMU_VA_SIZE
                && sends.get() == 3
        });
        if !ok {
            return 9;
        }
    }

    // Upstream derives the range; KUMO only attests the word. These cases pin that
    // derivation, prove only [7:0] selects the width, and prove that a range which cannot
    // hold this service's window — or a width that would make Mesa's own shift undefined —
    // is refused with nothing published, rather than clamped. — SAKER
    for (features, va_bits, size, code) in [
        (MMU_FEATURES, MMU_VA_BITS, MMU_VA_SIZE, 1u32),
        (0x0000_0030u32, 48u32, MMU_VA_SIZE, 1u32),
        (0xffff_ff20u32, 32u32, 1u64 << 32, 1u32),
        (0x2818u32, 24u32, 1u64 << 24, 4u32),
        (0x2800u32, 0u32, 1u64, 4u32),
        (0x2840u32, 64u32, 0u64, 4u32),
    ] {
        sends.set(0);
        let mut wire = transport(&sends, &closed, false, false);
        wire.hardware.mmu_features = features;
        let mut client = Client::new(wire);
        let ok = with_api(&mut client, |api| {
            let mut report = Report::default();
            let status = unsafe { kumo_mesa_probe(api, &mut report) };
            if status != code || sends.get() != 3 {
                return false;
            }
            if code == 1 {
                (report.info.mmu_features & 0xff) == va_bits
                    && report.va_range_start == 0
                    && report.va_range_size == size
            } else {
                report.info.gpu_id == 0
                    && report.info.mmu_features == 0
                    && report.core_count == 0
                    && report.va_range_start == 0
                    && report.va_range_size == 0
            }
        });
        if !ok {
            return 12;
        }
    }

    // A zero MMU attestation retires the session exactly as a zero topology does: C sees
    // the failure with cleared output and cannot issue another request. — SAKER
    sends.set(0);
    let mut wire = transport(&sends, &closed, false, false);
    wire.hardware.mmu_features = 0;
    let mut client = Client::new(wire);
    let ok = with_api(&mut client, |api| {
        let mut report = Report::default();
        (unsafe { kumo_mesa_probe(api, &mut report) }) == 3
            && report.query_status == SERVER_ERROR + Status::DeviceLost as u32
            && report.info.gpu_id == 0
            && report.info.mmu_features == 0
            && report.va_range_size == 0
    });
    if !ok || !client.is_lost() || sends.get() != 3 {
        return 13;
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
