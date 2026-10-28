//j641
//j644

extern crate std;
use super::*;
use crate::{
    session::{Backend, Session, SubmitError},
    BindFlags, DeviceTopology, PAGE_BYTES, VA_START,
};
use std::{collections::VecDeque, vec::Vec};

const INFO: DeviceInfo = DeviceInfo {
    gpu_id: 0xa8670005,
    cs_features: 0x7085f,
};

#[derive(Default)]
struct Script {
    sent: Vec<Request>,
    frames: VecDeque<(Vec<u8>, usize, Option<u32>)>,
    closed: Vec<u32>,
    reads: usize,
    send_error: Option<TransportError>,
    receive_error: Option<TransportError>,
}
impl Script {
    fn reply(&mut self, id: u32, result: Result<Response, Status>, handle: Option<u32>) {
        self.frames
            .push_back((Reply { id, result }.encode().to_vec(), REPLY_LEN, handle));
    }
}
impl Transport for Script {
    type Handle = u32;
    fn send(&mut self, bytes: &[u8; REQUEST_LEN]) -> Result<(), TransportError> {
        self.sent
            .push(Request::decode(bytes).expect("canonical outgoing request"));
        self.send_error.map_or(Ok(()), Err)
    }
    fn receive(
        &mut self,
        bytes: &mut [u8; REPLY_LEN + 1],
    ) -> Result<(usize, Option<u32>), TransportError> {
        self.reads += 1;
        if let Some(error) = self.receive_error {
            return Err(error);
        }
        let (frame, len, handle) = self
            .frames
            .pop_front()
            .expect("no implicit receive or retry");
        let n = frame.len().min(bytes.len());
        bytes[..n].copy_from_slice(&frame[..n]);
        Ok((len, handle))
    }
    fn close_handle(&mut self, handle: u32) {
        self.closed.push(handle);
    }
}
fn job(point: u64) -> Submission {
    Submission {
        group: 1,
        stream_va: VA_START,
        stream_bytes: 32,
        signal_point: point,
    }
}

#[test]
fn malformed_stale_oversized_and_wrong_kind_replies_close_grants_and_stop_io() {
    let good = Reply {
        id: 1,
        result: Ok(Response::Info(INFO)),
    }
    .encode()
    .to_vec();
    let mut frames = Vec::new();
    for n in 0..REPLY_LEN {
        frames.push((good.clone(), n, Some(88)));
    }
    let mut oversized = good.clone();
    oversized.push(0);
    frames.push((oversized, REPLY_LEN + 1, Some(88)));
    frames.push((good.clone(), usize::MAX, Some(88)));
    // A valid information reply still cannot carry a handle.
    frames.push((good.clone(), REPLY_LEN, Some(88)));
    for at in [0, 4, 12, 40] {
        let mut bad = good.clone();
        bad[at] ^= 0x80;
        frames.push((bad, REPLY_LEN, Some(88)));
    }
    for reply in [
        Reply {
            id: 2,
            result: Ok(Response::Info(INFO)),
        },
        Reply {
            id: 0,
            result: Err(Status::BadRequest),
        },
        Reply {
            id: 1,
            result: Ok(Response::VmCreated { vm: 1 }),
        },
        Reply {
            id: 1,
            result: Ok(Response::BufferCreated {
                buffer: 1,
                bytes: PAGE_BYTES,
            }),
        },
        Reply {
            id: 1,
            result: Err(Status::Busy),
        },
    ] {
        frames.push((reply.encode().to_vec(), REPLY_LEN, Some(88)));
    }
    for frame in frames {
        let mut transport = Script::default();
        transport.frames.push_back(frame);
        let mut client = Client::new(transport);
        assert_eq!(client.info(), Err(Error::Protocol));
        assert!(client.is_lost());
        assert_eq!(client.info(), Err(Error::DeviceLost));
        assert_eq!(client.transport.closed, [88]);
        assert_eq!(client.transport.sent.len(), 1);
        assert_eq!(client.transport.reads, 1);
    }
}

#[test]
fn allocation_size_handle_count_and_timeline_points_must_match_the_request() {
    for (command, response, handle) in [
        (
            Command::BufferCreate { bytes: PAGE_BYTES },
            Response::BufferCreated {
                buffer: 1,
                bytes: PAGE_BYTES * 2,
            },
            Some(9),
        ),
        (
            Command::BufferCreate { bytes: PAGE_BYTES },
            Response::BufferCreated {
                buffer: 1,
                bytes: PAGE_BYTES,
            },
            None,
        ),
        (
            Command::Submit(job(1)),
            Response::Submitted { point: 2 },
            None,
        ),
        (
            Command::SyncRead { group: 1, point: 2 },
            Response::Sync {
                point: 1,
                state: SyncState::Pending,
            },
            None,
        ),
        (
            Command::SyncRead { group: 1, point: 2 },
            Response::Sync {
                point: 1,
                state: SyncState::Complete { gpu_status: 0 },
            },
            None,
        ),
        (
            Command::GroupCreate { vm: 1 },
            Response::VmCreated { vm: 1 },
            None,
        ),
        (Command::VmCreate, Response::GroupCreated { group: 1 }, None),
    ] {
        let mut transport = Script::default();
        transport.reply(1, Ok(response), handle);
        let mut client = Client::new(transport);
        assert_eq!(client.call(command), Err(Error::Protocol));
        assert_eq!(client.info(), Err(Error::DeviceLost));
        assert_eq!(
            client.transport.closed,
            handle.into_iter().collect::<Vec<_>>()
        );
        assert_eq!(client.transport.sent.len(), 1);
    }
}

#[test]
fn recoverable_server_errors_consume_ids_and_leave_the_client_live() {
    for status in [
        Status::BadRequest,
        Status::BadState,
        Status::NotFound,
        Status::Busy,
        Status::NoMemory,
        Status::NotSupported,
    ] {
        let mut transport = Script::default();
        transport.reply(1, Err(status), None);
        transport.reply(2, Ok(Response::Info(INFO)), None);
        let mut client = Client::new(transport);
        assert_eq!(client.info(), Err(Error::Server(status)));
        assert!(!client.is_lost());
        assert_eq!(client.info(), Ok(INFO));
        assert_eq!(
            client
                .transport
                .sent
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert!(client.transport.closed.is_empty());
    }
}

#[test]
fn explicit_device_loss_and_failed_gpu_completion_are_observed_once_then_latched() {
    let mut transport = Script::default();
    transport.reply(1, Err(Status::DeviceLost), None);
    let mut client = Client::new(transport);
    assert_eq!(client.info(), Err(Error::Server(Status::DeviceLost)));
    assert!(client.is_lost());
    assert_eq!(client.info(), Err(Error::DeviceLost));
    assert_eq!(client.transport.sent.len(), 1);
    for gpu_status in [1, u32::MAX] {
        let mut transport = Script::default();
        transport.reply(
            1,
            Ok(Response::Sync {
                point: 7,
                state: SyncState::Complete { gpu_status },
            }),
            None,
        );
        let mut client = Client::new(transport);
        assert_eq!(
            client.sync_read(1, 7),
            Ok(SyncState::Complete { gpu_status })
        );
        assert!(client.is_lost());
        assert_eq!(client.submit(job(8)), Err(Error::DeviceLost));
        assert_eq!(client.transport.sent.len(), 1);
    }
}

#[test]
fn transport_failures_never_retry_or_replay_an_uncertain_submission() {
    for error in [TransportError::Closed, TransportError::Failed] {
        for receive in [false, true] {
            let mut transport = Script::default();
            if receive {
                transport.receive_error = Some(error);
            } else {
                transport.send_error = Some(error);
            }
            let mut client = Client::new(transport);
            assert_eq!(client.submit(job(1)), Err(Error::Transport(error)));
            assert_eq!(client.submit(job(1)), Err(Error::DeviceLost));
            assert_eq!(client.transport.sent.len(), 1);
            assert_eq!(client.transport.reads, usize::from(receive));
        }
    }
}

#[test]
fn invalid_local_requests_use_no_id_and_exhaustion_never_wraps_to_zero() {
    let mut transport = Script::default();
    transport.reply(u32::MAX, Ok(Response::Info(INFO)), None);
    let mut client = Client::new(transport);
    client.id = u32::MAX - 1;
    assert_eq!(client.create_buffer(0), Err(Error::InvalidRequest));
    assert!(!client.is_lost());
    assert!(client.transport.sent.is_empty());
    assert_eq!(client.info(), Ok(INFO));
    assert_eq!(client.info(), Err(Error::RequestIdsExhausted));
    assert_eq!(client.info(), Err(Error::DeviceLost));
    assert_eq!(client.transport.sent.len(), 1);
    assert_eq!(client.transport.sent[0].id, u32::MAX);
}

#[derive(Default)]
struct Model;
impl Backend for Model {
    fn topology(&mut self) -> Result<DeviceTopology, Status> {
        Ok(DeviceTopology {
            shader_present: 0x500000005,
        })
    }
    fn info(&mut self) -> Result<DeviceInfo, Status> {
        Ok(INFO)
    }
    fn create_vm(&mut self) -> Result<(), Status> {
        Ok(())
    }
    fn create_buffer(&mut self, _: u32, _: u64) -> Result<(), Status> {
        Ok(())
    }
    fn bind(&mut self, _: Binding) -> Result<(), Status> {
        Ok(())
    }
    fn create_group(&mut self) -> Result<(), Status> {
        Ok(())
    }
    fn submit(&mut self, _: Submission) -> Result<(), SubmitError> {
        Ok(())
    }
}
#[derive(Default)]
struct Loopback {
    session: Session,
    model: Model,
    transport: Script,
}
impl Transport for Loopback {
    type Handle = u32;
    fn send(&mut self, bytes: &[u8; REQUEST_LEN]) -> Result<(), TransportError> {
        self.transport.send(bytes)?;
        let request = Request::decode(bytes).unwrap();
        let result = self.session.dispatch(request, &mut self.model);
        let handle = matches!(result, Ok(Response::BufferCreated { .. })).then_some(42);
        self.transport.reply(request.id, result, handle);
        Ok(())
    }
    fn receive(
        &mut self,
        bytes: &mut [u8; REPLY_LEN + 1],
    ) -> Result<(usize, Option<u32>), TransportError> {
        self.transport.receive(bytes)
    }
    fn close_handle(&mut self, handle: u32) {
        self.transport.close_handle(handle);
    }
}

#[test]
fn all_typed_operations_use_the_real_server_contract_and_preserve_pending_completion() {
    let mut client = Client::new(Loopback::default());
    assert_eq!(client.info(), Ok(INFO));
    assert_eq!(
        client.topology(),
        Ok(DeviceTopology {
            shader_present: 0x500000005
        })
    );
    let vm = client.create_vm().unwrap();
    assert_eq!(client.create_vm(), Err(Error::Server(Status::BadState)));
    let buffer = client.create_buffer(PAGE_BYTES).unwrap();
    assert_eq!(
        buffer,
        Buffer {
            id: 1,
            bytes: PAGE_BYTES,
            handle: 42
        }
    );
    client
        .bind(Binding {
            vm,
            buffer: buffer.id,
            va: VA_START,
            offset: 0,
            bytes: buffer.bytes,
            flags: BindFlags(BindFlags::READ_ONLY | BindFlags::NO_EXECUTE),
        })
        .unwrap();
    let group = client.create_group(vm).unwrap();
    assert_eq!(group, 1);
    client.submit(job(1)).unwrap();
    assert_eq!(client.sync_read(group, 1), Ok(SyncState::Pending));
    client.transport.session.complete(1, 0).unwrap();
    assert_eq!(
        client.sync_read(group, 1),
        Ok(SyncState::Complete { gpu_status: 0 })
    );
    assert!(!client.is_lost());
    assert!(client.transport.transport.closed.is_empty());
    let ids: Vec<_> = client
        .transport
        .transport
        .sent
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, (1..=ids.len() as u32).collect::<Vec<_>>());
}

#[test]
fn topology_rejects_wrong_kind_id_zero_and_grants_without_replay() {
    let good = Response::Topology(DeviceTopology {
        shader_present: 0x8000000000000001,
    });
    for (id, response, grant) in [
        (1, Response::Info(INFO), None),
        (2, good, None),
        (
            1,
            Response::Topology(DeviceTopology { shader_present: 0 }),
            None,
        ),
        (1, good, Some(88)),
    ] {
        let mut wire = Script::default();
        wire.reply(id, Ok(response), grant);
        let mut client = Client::new(wire);
        assert_eq!(client.topology(), Err(Error::Protocol));
        assert_eq!(client.topology(), Err(Error::DeviceLost));
        assert_eq!(client.transport.sent.len(), 1);
        assert_eq!(
            client.transport.closed,
            grant.into_iter().collect::<Vec<_>>()
        );
    }
    let mut wire = Script::default();
    wire.reply(1, Err(Status::NotSupported), None);
    wire.reply(2, Ok(Response::Info(INFO)), None);
    let mut client = Client::new(wire);
    assert_eq!(client.topology(), Err(Error::Server(Status::NotSupported)));
    assert_eq!(client.info(), Ok(INFO));
    assert!(!client.is_lost());
}
