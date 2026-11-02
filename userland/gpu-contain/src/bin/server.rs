//j710
#![no_std]
#![no_main]
//j709
//j754

//! QEMU-only fake server for G8d-a's contained-close smoke. It speaks enough of the real
//! `kumo-gpu` wire to let a fake client arm two buffers, two bindings and one fence wait,
//! then waits for the client's kill to close the request channel. On close it runs the real
//! `drv_gpu::contain` state machine twice: once with a scripted confirmed reset, once with a
//! scripted refused reset, and reports both exact records. No GPU, MMIO or firmware is used;
//! no real DMA is allocated. The point of the program is the ordering — the client channel is
//! closed before either containment path runs. — TIMBERDOODLE

use core::fmt::{self, Write};

use drv_gpu::contain::{Containment, ContainmentState};
use drv_gpu::{soft_reset, Completion, CompletionIo, MmioIo, ResetError};
use kumo_abi::{Handle, Status as AbiStatus};
use kumo_gpu::session::{Backend, Session, SessionRelease, SubmitError};
use kumo_gpu::{
    BindFlags, Binding, Command, DeviceInfo, Reply, Request, Response, Status, Submission,
    PAGE_BYTES, REQUEST_LEN, VA_START,
};

kumo_rt::entry!(main);

/// `GPU_INT_RAWSTAT`'s reset-completed bit (RESEARCH/009 §4.4). The scripted IO ignores the
/// offset and answers only the one register `soft_reset` rechecks after its wait.
const RESET_COMPLETED: u32 = 1 << 8;
const REPORT_BYTES: usize = 512;

struct ScriptedGpu {
    rawstat: u32,
    completion: Completion,
}

impl MmioIo for ScriptedGpu {
    fn read32(&mut self, _offset: u32) -> u32 {
        self.rawstat
    }
    fn write32(&mut self, _offset: u32, _value: u32) {}
}

impl CompletionIo for ScriptedGpu {
    fn arm_completion(&mut self, _irq: u32, _timeout_ns: u64) -> Result<(), AbiStatus> {
        Ok(())
    }
    fn wait_completion(&mut self) -> Completion {
        self.completion
    }
    fn complete(&mut self) {}
}

/// Counts the driver-side claims a real backend would hold, so the smoke can require that a
/// confirmed containment releases them and a refused one retains them. — TIMBERDOODLE
#[derive(Default)]
struct FakeBackend {
    claims: u32,
    retire_calls: u32,
}

impl Backend for FakeBackend {
    fn info(&mut self) -> Result<DeviceInfo, Status> {
        Ok(DeviceInfo {
            gpu_id: 0xa867_0005,
            cs_features: 0x7085f,
        })
    }
    fn create_vm(&mut self) -> Result<(), Status> {
        Ok(())
    }
    fn create_buffer(&mut self, _id: u32, _bytes: u64) -> Result<(), Status> {
        self.claims += 1;
        Ok(())
    }
    fn bind(&mut self, _binding: Binding) -> Result<(), Status> {
        Ok(())
    }
    fn unbind(&mut self, _bindings: &[Binding]) -> Result<(), Status> {
        Ok(())
    }
    fn destroy_buffer(&mut self, _id: u32, _bindings: &[Binding]) -> Result<(), Status> {
        Ok(())
    }
    fn create_group(&mut self) -> Result<(), Status> {
        Ok(())
    }
    fn submit(&mut self, _submission: Submission) -> Result<(), SubmitError> {
        Ok(())
    }
    fn retire(&mut self) {
        self.retire_calls += 1;
        self.claims = 0;
    }
}

struct Text {
    bytes: [u8; REPORT_BYTES],
    len: usize,
}

impl Default for Text {
    fn default() -> Self {
        Self {
            bytes: [0; REPORT_BYTES],
            len: 0,
        }
    }
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

fn handle(raw: u64) -> Option<Handle> {
    (raw != 0 && (raw as i64) > 0).then_some(Handle(raw as u32))
}

fn send(channel: Handle, bytes: &[u8]) -> bool {
    kumo_rt::channel_write(channel, bytes.as_ptr(), bytes.len()) == 0
}

fn binding(buffer: u32, va: u64) -> Binding {
    Binding {
        vm: 1,
        buffer,
        va,
        offset: 0,
        bytes: PAGE_BYTES,
        flags: BindFlags(BindFlags::READ_ONLY | BindFlags::NO_EXECUTE),
    }
}

/// Populate a session without a wire peer: the same eight requests the fake client sends.
/// The refused-reset half must start from a session that really owns objects, or its zero
/// release counts would prove nothing. — TIMBERDOODLE
fn populate(session: &mut Session, backend: &mut FakeBackend) -> bool {
    let commands = [
        Command::VmCreate,
        Command::BufferCreate { bytes: PAGE_BYTES },
        Command::BufferCreate {
            bytes: 2 * PAGE_BYTES,
        },
        Command::VmBind(binding(1, VA_START)),
        Command::VmBind(binding(2, VA_START + PAGE_BYTES)),
        Command::GroupCreate { vm: 1 },
        Command::Submit(Submission {
            group: 1,
            stream_va: VA_START,
            stream_bytes: 48,
            signal_point: 1,
        }),
        Command::FenceWait { group: 1, point: 1 },
    ];
    for (index, command) in commands.into_iter().enumerate() {
        let request = Request {
            id: index as u32 + 1,
            command,
        };
        if session.dispatch(request, backend).is_err() {
            return false;
        }
    }
    true
}

#[no_mangle]
extern "C" fn main(request_raw: u64, report_raw: u64) -> ! {
    let (Some(request), Some(report)) = (handle(request_raw), handle(report_raw)) else {
        kumo_rt::process_exit(1)
    };
    let mut session = Session::default();
    let mut backend = FakeBackend::default();
    let mut announced = false;
    let mut bytes = [0u8; REQUEST_LEN + 1];
    loop {
        let (len, raw_handle) =
            kumo_rt::channel_read_with_handle(request, bytes.as_mut_ptr(), bytes.len());
        if let Some(handle) = handle(raw_handle) {
            let _ = kumo_rt::handle_close(handle);
        }
        if len == 0 || len > bytes.len() {
            break;
        }
        let Some(frame) = bytes.get(..len).and_then(Request::decode) else {
            break;
        };
        let id = frame.id;
        let arms_wait = matches!(frame.command, Command::FenceWait { .. });
        let result = session.dispatch(frame, &mut backend);
        let armed = matches!(&result, Ok(Response::WaitArmed { .. }));
        let reply = Reply { id, result }.encode();
        if !send(request, &reply) {
            break;
        }
        if arms_wait && armed && !announced {
            // The session owns the wait before the supervisor learns the setup is complete,
            // so the kill that follows can never race the arming. — TIMBERDOODLE
            announced = send(report, b"ready\n");
            if !announced {
                break;
            }
        }
    }

    // The client channel is closed: service stopped accepting. A confirmed reset is the only
    // path that may release; both the session metadata and the driver's claims must go.
    let confirmed = Containment::run(
        soft_reset(&mut ScriptedGpu {
            rawstat: RESET_COMPLETED,
            completion: Completion::Fired,
        }),
        &mut session,
        &mut backend,
    );
    let confirmed_ok = confirmed.state == ContainmentState::Released
        && confirmed.release
            == (SessionRelease {
                buffers: 2,
                bindings: 2,
                waits: 1,
                heaps: 0,
                bytes: 3 * PAGE_BYTES,
            })
        && backend.retire_calls == 1
        && backend.claims == 0;

    // The refused half: a populated session with no proven quiescence must release nothing.
    let mut refused_session = Session::default();
    let mut refused_backend = FakeBackend::default();
    let populated = populate(&mut refused_session, &mut refused_backend);
    let refused = Containment::run(
        Err(ResetError::Timeout { rawstat: 0 }),
        &mut refused_session,
        &mut refused_backend,
    );
    let refused_ok = populated
        && refused.state == ContainmentState::Poisoned
        && refused.release == SessionRelease::NONE
        && refused_backend.retire_calls == 0
        && refused_backend.claims == 2;

    let mut text = Text::default();
    let _ = write!(text, "{}\n{}\n", confirmed, refused);
    let pass = confirmed_ok && refused_ok;
    if pass {
        let _ = write!(text, "gpu-contain: pass\n");
    } else {
        let _ = write!(
            text,
            "gpu-contain: fail confirmed={} refused={}\n",
            confirmed_ok, refused_ok
        );
    }
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &text.bytes[..text.len]);
    let _ = send(report, &text.bytes[..text.len]);
    let _ = kumo_rt::handle_close(report);
    let _ = kumo_rt::handle_close(request);
    kumo_rt::process_exit(u64::from(!pass))
}
