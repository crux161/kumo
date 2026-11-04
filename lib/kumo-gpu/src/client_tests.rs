//j681
//j707
//j711
//j752
//j778
extern crate std;
use super::*;
use crate::{
    session::{Backend, Session, SubmitError},
    BindFlags, DeviceTopology, Unbind, PAGE_BYTES, VA_START,
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
fn fence_wait_reports_done_or_armed_and_owns_the_wake_handle() {
    let mut transport = Script::default();
    transport.reply(
        1,
        Ok(Response::WaitDone {
            point: 1,
            gpu_status: 0,
        }),
        None,
    );
    transport.reply(2, Ok(Response::WaitArmed { point: 1 }), Some(9));
    let mut client = Client::new(transport);
    assert_eq!(
        client.fence_wait(Fence { group: 1, point: 1 }),
        Ok(FenceStatus::Done { gpu_status: 0 })
    );
    assert_eq!(
        client.fence_wait(Fence { group: 1, point: 1 }),
        Ok(FenceStatus::Armed(9))
    );
    // The armed handle belongs to the caller; nothing was closed behind its back.
    assert!(client.transport.closed.is_empty());
    assert_eq!(client.transport.sent.len(), 2);
    assert_eq!(
        client.transport.sent[0].command,
        Command::FenceWait { group: 1, point: 1 }
    );
    assert_eq!(
        client.transport.sent[1].command,
        Command::FenceWait { group: 1, point: 1 }
    );
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
    fn unbind(&mut self, _: &[Binding]) -> Result<(), Status> {
        Ok(())
    }
    fn destroy_buffer(&mut self, _: u32, _: &[Binding]) -> Result<(), Status> {
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

#[test]
fn unbind_tiles_live_bindings_after_the_group_and_refuses_unknown_ranges() {
    let mut client = Client::new(Loopback::default());
    let vm = client.create_vm().unwrap();
    let first = client.create_buffer(PAGE_BYTES).unwrap();
    let second = client.create_buffer(PAGE_BYTES).unwrap();
    let mapping = |buffer: u32, va: u64| Binding {
        vm,
        buffer,
        va,
        offset: 0,
        bytes: PAGE_BYTES,
        flags: BindFlags(BindFlags::READ_ONLY | BindFlags::NO_EXECUTE),
    };
    client.bind(mapping(first.id, VA_START)).unwrap();
    client
        .bind(mapping(second.id, VA_START + PAGE_BYTES))
        .unwrap();
    client.create_group(vm).unwrap();
    // One request covers two adjacent live bindings after GroupCreate.
    let range = Unbind {
        vm,
        va: VA_START,
        bytes: 2 * PAGE_BYTES,
    };
    client.unbind(range).unwrap();
    // The same range is now unknown; the refusal is recoverable and proves the session
    // dropped both bindings without needing to read its private table. — TIMBERDOODLE
    assert_eq!(client.unbind(range), Err(Error::Server(Status::NotFound)));
    assert!(!client.is_lost());
    // A live rebind at the freed VA proves the table was really cleared.
    let rebound = mapping(first.id, VA_START);
    client.bind(rebound).unwrap();
    assert_eq!(
        client.transport.transport.sent.last().unwrap().command,
        Command::VmBind(rebound)
    );
    assert!(client.transport.transport.closed.is_empty());
}

#[test]
fn unbind_rejects_a_wrong_kind_or_mismatched_range_reply() {
    let range = Unbind {
        vm: 1,
        va: VA_START,
        bytes: PAGE_BYTES,
    };
    let replies: [Result<Response, Status>; 2] = [
        Ok(Response::VmBound),
        Ok(Response::VmUnbound(Unbind {
            va: VA_START + PAGE_BYTES,
            ..range
        })),
    ];
    for reply in replies {
        let mut transport = Script::default();
        transport.reply(1, reply, None);
        let mut client = Client::new(transport);
        assert_eq!(client.unbind(range), Err(Error::Protocol));
        assert!(client.is_lost());
        assert_eq!(client.unbind(range), Err(Error::DeviceLost));
        assert_eq!(client.transport.sent.len(), 1);
    }
}

#[test]
fn destroy_buffer_releases_the_id_before_and_after_the_group_exists() {
    let mut client = Client::new(Loopback::default());
    let vm = client.create_vm().unwrap();
    let first = client.create_buffer(PAGE_BYTES).unwrap();
    let second = client.create_buffer(PAGE_BYTES).unwrap();
    assert_eq!((first.id, second.id), (1, 2));
    let mapping = |buffer: u32| Binding {
        vm,
        buffer,
        va: VA_START,
        offset: 0,
        bytes: PAGE_BYTES,
        flags: BindFlags(BindFlags::READ_ONLY | BindFlags::NO_EXECUTE),
    };
    client.bind(mapping(first.id)).unwrap();
    client.destroy_buffer(&first).unwrap();
    // The session unbound the VA range with the buffer, so the next allocation may map it;
    // the consumed ID answers NotFound and leaves the client live. — TIMBERDOODLE
    client.bind(mapping(second.id)).unwrap();
    assert_eq!(
        client.destroy_buffer(&first),
        Err(Error::Server(Status::NotFound))
    );
    assert!(!client.is_lost());
    // Destroy is live once the group exists (j711): its implicit unbind batch is legal and a
    // replayed ID still answers NotFound. — TIMBERDOODLE
    client.create_group(vm).unwrap();
    client.destroy_buffer(&second).unwrap();
    assert_eq!(
        client.destroy_buffer(&second),
        Err(Error::Server(Status::NotFound))
    );
    assert!(!client.is_lost());
    assert!(client.transport.transport.closed.is_empty());
}

#[test]
fn group_watch_correlates_identity_and_owns_exactly_one_read_grant() {
    let mut wire = Script::default();
    wire.reply(
        1,
        Ok(Response::GroupWatch {
            group: 1,
            report: crate::GroupReport::USABLE,
        }),
        Some(77),
    );
    let mut client = Client::new(wire);
    let watch = client.watch_group(1).unwrap();
    assert_eq!(watch.group, 1);
    assert_eq!(watch.report, crate::GroupReport::USABLE);
    client.close_buffer_handle(watch.handle);
    assert_eq!(client.transport.closed, [77]);
    assert!(!client.is_lost());
    for (group, handle) in [(2, Some(88)), (1, None)] {
        let mut wire = Script::default();
        wire.reply(
            1,
            Ok(Response::GroupWatch {
                group,
                report: crate::GroupReport::USABLE,
            }),
            handle,
        );
        let mut client = Client::new(wire);
        assert!(matches!(client.watch_group(1), Err(Error::Protocol)));
        assert!(client.is_lost());
        if handle.is_some() {
            assert_eq!(client.transport.closed, [88]);
        }
    }
    let mut wire = Script::default();
    wire.reply(
        1,
        Ok(Response::GroupWatch {
            group: 1,
            report: crate::GroupReport::TIMED_OUT,
        }),
        Some(99),
    );
    let mut client = Client::new(wire);
    let watch = client.watch_group(1).unwrap();
    assert!(client.is_lost());
    assert_eq!(watch.report, crate::GroupReport::TIMED_OUT);
    client.close_buffer_handle(watch.handle);
    assert_eq!(client.watch_group(1).err(), Some(Error::DeviceLost));
    assert_eq!(client.transport.sent.len(), 1);
}

/// VK1 (found by VK2-a's native tests): the client accepts exactly the replies the new
/// commands may have, so a three-queue group and a multi-stream submit work on a real wire.
#[test]
fn three_queue_group_and_multi_stream_submit_replies_are_accepted_exactly() {
    let streams = crate::MultiSubmission {
        group: 1,
        signal_point: 4,
        count: 2,
        streams: [
            crate::StreamRef {
                queue: 0,
                stream_va: VA_START,
                stream_bytes: 32,
            },
            crate::StreamRef {
                queue: 2,
                stream_va: VA_START + 64,
                stream_bytes: 32,
            },
            crate::StreamRef::default(),
        ],
    };
    let mut transport = Script::default();
    transport.reply(1, Ok(Response::GroupCreated { group: 1 }), None);
    transport.reply(2, Ok(Response::Submitted { point: 4 }), None);
    let mut client = Client::new(transport);
    assert_eq!(client.create_group_queues(1, 3), Ok(1));
    assert_eq!(
        client
            .submit_streams_fence(streams)
            .map(|fence| fence.point),
        Ok(4)
    );
    // A reply naming another point is a protocol fault, as for a single Submit.
    let mut transport = Script::default();
    transport.reply(1, Ok(Response::Submitted { point: 5 }), None);
    let mut client = Client::new(transport);
    assert_eq!(
        client.call(Command::SubmitStreams(streams)),
        Err(Error::Protocol)
    );
}
