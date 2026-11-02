//j749
//j750
//j751
extern crate std;

use super::*;
use crate::client::TransportError;
use crate::{Command, Reply, Request, Response, Status, REPLY_LEN, REQUEST_LEN, VA_START};
use std::{cell::RefCell, collections::VecDeque, rc::Rc, vec::Vec};

#[derive(Default)]
struct Script {
    calls: Vec<Command>,
    replies: VecDeque<(Result<Response, Status>, Option<u32>)>,
    closed: Vec<u32>,
    fail_send: bool,
}
struct Wire {
    state: Rc<RefCell<Script>>,
    id: u32,
}
impl Transport for Wire {
    type Handle = u32;
    fn send(&mut self, bytes: &[u8; REQUEST_LEN]) -> Result<(), TransportError> {
        let request = Request::decode(bytes).unwrap();
        self.id = request.id;
        let mut state = self.state.borrow_mut();
        state.calls.push(request.command);
        if state.fail_send {
            Err(TransportError::Closed)
        } else {
            Ok(())
        }
    }
    fn receive(
        &mut self,
        bytes: &mut [u8; REPLY_LEN + 1],
    ) -> Result<(usize, Option<u32>), TransportError> {
        let (result, handle) = self.state.borrow_mut().replies.pop_front().unwrap();
        bytes[..REPLY_LEN].copy_from_slice(
            &Reply {
                id: self.id,
                result,
            }
            .encode(),
        );
        Ok((REPLY_LEN, handle))
    }
    fn close_handle(&mut self, handle: u32) {
        self.state.borrow_mut().closed.push(handle);
    }
}
fn client() -> (Client<Wire>, Rc<RefCell<Script>>) {
    let state = Rc::new(RefCell::new(Script::default()));
    let client = Client::new(Wire {
        state: state.clone(),
        id: 0,
    });
    (client, state)
}
fn job(point: u64) -> Submission {
    Submission {
        group: 1,
        stream_va: VA_START,
        stream_bytes: 32,
        signal_point: point,
    }
}
fn point(sync: SyncHandle, value: u64) -> Point {
    Point { sync, value }
}
fn submitted(state: &Rc<RefCell<Script>>, value: u64) {
    state
        .borrow_mut()
        .replies
        .push_back((Ok(Response::Submitted { point: value }), None));
}
fn done(state: &Rc<RefCell<Script>>, point: u64, gpu_status: u32) {
    state
        .borrow_mut()
        .replies
        .push_back((Ok(Response::WaitDone { point, gpu_status }), None));
}

#[test]
fn host_signal_reset_and_timeline_zero_need_no_gpu_io() {
    let (mut client, state) = client();
    let mut syncs = SyncObjects::new(&mut client).unwrap();
    let binary = point(syncs.create_binary(false).unwrap(), 0);
    assert!(!syncs.wait_pending(binary).unwrap());
    syncs.signal(binary).unwrap();
    let captured = syncs.snapshot(binary).unwrap().unwrap();
    syncs.reset(binary.sync).unwrap();
    assert!(!syncs.wait_pending(binary).unwrap());
    assert_eq!(syncs.arm(captured), Ok(FenceStatus::Done { gpu_status: 0 }));
    let timeline = syncs.create_timeline(7).unwrap();
    assert!(syncs.wait_pending(point(timeline, 0)).unwrap());
    assert!(syncs.wait_pending(point(timeline, 7)).unwrap());
    assert!(!syncs.wait_pending(point(timeline, 8)).unwrap());
    syncs.signal(point(timeline, u64::MAX)).unwrap();
    assert_eq!(syncs.reset(timeline), Err(Error::InvalidRequest));
    assert_eq!(syncs.signal(point(timeline, 7)), Err(Error::InvalidRequest));
    assert_eq!(
        syncs.signal(point(binary.sync, 1)),
        Err(Error::InvalidRequest)
    );
    assert!(state.borrow().calls.is_empty());
}

#[test]
fn accepted_signals_are_pending_until_the_real_fence_is_done() {
    let (mut client, state) = client();
    let mut syncs = SyncObjects::new(&mut client).unwrap();
    let binary = point(syncs.create_binary(false).unwrap(), 0);
    let timeline = point(syncs.create_timeline(0).unwrap(), 42);
    submitted(&state, 1);
    let fence = syncs.submit(job(1), &[binary, timeline]).unwrap();
    assert!(syncs.wait_pending(binary).unwrap());
    assert!(syncs.wait_pending(point(timeline.sync, 41)).unwrap());
    assert!(!syncs.wait_pending(point(timeline.sync, 43)).unwrap());
    assert_eq!(syncs.signal(point(timeline.sync, 43)), Err(Error::Busy));
    state
        .borrow_mut()
        .replies
        .push_back((Ok(Response::WaitArmed { point: 1 }), Some(123)));
    assert_eq!(syncs.arm(fence), Ok(FenceStatus::Armed(123)));
    syncs.close_wait_handle(123);
    // Abandoning the wake does not complete the object or make a CPU signal safe.
    assert_eq!(syncs.signal(point(timeline.sync, 43)), Err(Error::Busy));
    done(&state, 1, 0);
    assert_eq!(syncs.arm(fence), Ok(FenceStatus::Done { gpu_status: 0 }));
    assert_eq!(syncs.state.objects.len(), 2);
    let captured = syncs.snapshot(timeline).unwrap().unwrap();
    assert_eq!(syncs.arm(captured), Ok(FenceStatus::Done { gpu_status: 0 }));
    syncs.signal(point(timeline.sync, 43)).unwrap();
    assert_eq!(state.borrow().closed, [123]);
    assert_eq!(state.borrow().calls.len(), 3);
    assert!(!state
        .borrow()
        .calls
        .iter()
        .any(|c| matches!(c, Command::SyncRead { .. })));
}

#[test]
fn snapshots_survive_binary_replacement_reset_and_destroy() {
    let (mut client, state) = client();
    let mut syncs = SyncObjects::new(&mut client).unwrap();
    let binary = point(syncs.create_binary(false).unwrap(), 0);
    submitted(&state, 1);
    syncs.submit(job(1), &[binary]).unwrap();
    let old = syncs.snapshot(binary).unwrap().unwrap();
    // Signaling or replacing the object must never signal the old captured GPU fence.
    syncs.signal(binary).unwrap();
    submitted(&state, 2);
    syncs.submit(job(2), &[binary]).unwrap();
    let new = syncs.snapshot(binary).unwrap().unwrap();
    syncs.reset(binary.sync).unwrap();
    syncs.destroy(binary.sync).unwrap();
    assert_eq!(syncs.snapshot(binary), Err(Error::NotFound));
    done(&state, 1, 0);
    syncs.arm(old).unwrap();
    done(&state, 2, 0);
    syncs.arm(new).unwrap();
    assert_eq!(
        state.borrow().calls,
        [
            Command::Submit(job(1)),
            Command::Submit(job(2)),
            Command::FenceWait { group: 1, point: 1 },
            Command::FenceWait { group: 1, point: 2 }
        ]
    );
}

#[test]
fn invalid_or_foreign_output_never_partially_publishes_or_sends() {
    let (mut other_client, _) = client();
    let mut other = SyncObjects::new(&mut other_client).unwrap();
    let foreign = point(other.create_binary(true).unwrap(), 0);
    let foreign_snapshot = other.snapshot(foreign).unwrap().unwrap();
    let (mut client, state) = client();
    let mut syncs = SyncObjects::new(&mut client).unwrap();
    let binary = point(syncs.create_binary(false).unwrap(), 0);
    let timeline = point(syncs.create_timeline(5).unwrap(), 6);
    for signals in [
        &[binary, foreign][..],
        &[timeline, timeline][..],
        &[binary, point(timeline.sync, 5)][..],
        &[point(binary.sync, 1)][..],
    ] {
        assert!(syncs.submit(job(1), signals).is_err());
        assert!(!syncs.wait_pending(binary).unwrap());
        assert!(!syncs.wait_pending(timeline).unwrap());
    }
    assert_eq!(syncs.arm(foreign_snapshot), Err(Error::NotFound));
    assert_eq!(syncs.destroy(foreign.sync), Err(Error::NotFound));
    assert!(state.borrow().calls.is_empty());
}

#[test]
fn refused_submit_preserves_all_payloads_and_corrected_retry_can_publish() {
    let (mut client, state) = client();
    let mut syncs = SyncObjects::new(&mut client).unwrap();
    let binary = point(syncs.create_binary(true).unwrap(), 0);
    let timeline = point(syncs.create_timeline(0).unwrap(), 9);
    state
        .borrow_mut()
        .replies
        .push_back((Err(Status::NoMemory), None));
    assert_eq!(
        syncs.submit(job(1), &[binary, timeline]),
        Err(Error::Client(ClientError::Server(Status::NoMemory)))
    );
    assert!(!syncs.wait_pending(timeline).unwrap());
    assert_eq!(
        syncs.arm(syncs.snapshot(binary).unwrap().unwrap()),
        Ok(FenceStatus::Done { gpu_status: 0 })
    );
    submitted(&state, 1);
    syncs.submit(job(1), &[binary, timeline]).unwrap();
    assert!(syncs.wait_pending(timeline).unwrap());
}

#[test]
fn sparse_timeline_values_complete_and_prune_only_through_the_observed_fence() {
    let (mut client, state) = client();
    let mut syncs = SyncObjects::new(&mut client).unwrap();
    let timeline = syncs.create_timeline(0).unwrap();
    submitted(&state, 1);
    let first = syncs.submit(job(1), &[point(timeline, 10)]).unwrap();
    submitted(&state, 2);
    let second = syncs.submit(job(2), &[point(timeline, 50)]).unwrap();
    let ten = syncs.snapshot(point(timeline, 9)).unwrap().unwrap();
    assert_eq!(ten, first);
    assert_eq!(syncs.snapshot(point(timeline, 11)).unwrap(), Some(second));
    done(&state, 1, 0);
    syncs.arm(ten).unwrap();
    let Object::Timeline { completed, pending } = &syncs.state.objects[0].object else {
        panic!()
    };
    assert_eq!(*completed, 10);
    assert_eq!(pending.len(), 1);
    done(&state, 2, 0);
    syncs.arm(second).unwrap();
    syncs.signal(point(timeline, 51)).unwrap();
    assert!(!syncs.wait_pending(point(timeline, 52)).unwrap());
    assert_eq!(
        syncs.submit(job(3), &[point(timeline, 50)]),
        Err(Error::InvalidRequest)
    );
}

#[test]
fn quota_returns_no_memory_before_io_and_completion_reclaims_timeline_space() {
    let (mut client, state) = client();
    let mut syncs = SyncObjects::new(&mut client).unwrap();
    let timeline = syncs.create_timeline(0).unwrap();
    for n in 1..=MAX_TIMELINE_POINTS as u64 {
        submitted(&state, n);
        syncs.submit(job(n), &[point(timeline, n * 2)]).unwrap();
    }
    let next = MAX_TIMELINE_POINTS as u64 + 1;
    assert_eq!(
        syncs.submit(job(next), &[point(timeline, next * 2)]),
        Err(Error::NoMemory)
    );
    assert_eq!(state.borrow().calls.len(), MAX_TIMELINE_POINTS);
    let last = syncs
        .snapshot(point(timeline, next * 2 - 2))
        .unwrap()
        .unwrap();
    done(&state, next - 1, 0);
    syncs.arm(last).unwrap();
    submitted(&state, next);
    syncs
        .submit(job(next), &[point(timeline, next * 2)])
        .unwrap();
    for _ in 1..MAX_SYNCS {
        syncs.create_binary(false).unwrap();
    }
    assert_eq!(syncs.create_binary(false), Err(Error::NoMemory));
    syncs.destroy(timeline).unwrap();
    let replacement = syncs.create_binary(false).unwrap();
    assert_ne!(replacement, timeline);
    assert_eq!(syncs.reset(timeline), Err(Error::NotFound));
    syncs.destroy(replacement).unwrap();
    syncs.state.next_id = u32::MAX;
    assert_eq!(syncs.create_binary(false), Err(Error::NoMemory));
}

#[test]
fn failed_completion_latches_loss_even_for_host_signaled_snapshots() {
    let (mut client, state) = client();
    let mut syncs = SyncObjects::new(&mut client).unwrap();
    let host = point(syncs.create_binary(true).unwrap(), 0);
    let host = syncs.snapshot(host).unwrap().unwrap();
    submitted(&state, 1);
    let gpu = syncs.submit(job(1), &[]).unwrap();
    done(&state, 1, 0xdead);
    assert_eq!(syncs.arm(gpu), Err(Error::GpuFault(0xdead)));
    assert_eq!(syncs.arm(host), Err(Error::Client(ClientError::DeviceLost)));
    assert_eq!(
        syncs.submit(job(1), &[]),
        Err(Error::Client(ClientError::DeviceLost))
    );
    assert_eq!(
        syncs.create_binary(true),
        Err(Error::Client(ClientError::DeviceLost))
    );
    assert_eq!(state.borrow().calls.len(), 2);
}

#[test]
fn uncertain_submit_is_never_replayed_or_published() {
    let (mut client, state) = client();
    let mut syncs = SyncObjects::new(&mut client).unwrap();
    let binary = point(syncs.create_binary(false).unwrap(), 0);
    state.borrow_mut().fail_send = true;
    assert_eq!(
        syncs.submit(job(1), &[binary]),
        Err(Error::Client(ClientError::Transport(
            TransportError::Closed
        )))
    );
    assert_eq!(
        syncs.submit(job(1), &[binary]),
        Err(Error::Client(ClientError::DeviceLost))
    );
    assert_eq!(
        syncs.wait_pending(binary),
        Err(Error::Client(ClientError::DeviceLost))
    );
    assert_eq!(state.borrow().calls.len(), 1);
}

#[test]
fn invalid_shape_and_group_switch_are_refused_locally() {
    let (mut client, state) = client();
    let mut syncs = SyncObjects::new(&mut client).unwrap();
    let mut invalid = job(0);
    assert_eq!(syncs.submit(invalid, &[]), Err(Error::InvalidRequest));
    submitted(&state, 1);
    syncs.submit(job(1), &[]).unwrap();
    invalid = job(2);
    invalid.group = 2;
    assert_eq!(syncs.submit(invalid, &[]), Err(Error::InvalidRequest));
    assert_eq!(state.borrow().calls.len(), 1);
}

#[test]
fn a_new_store_cannot_reinterpret_an_old_handle_or_snapshot() {
    let (mut client, state) = client();
    let (old, snapshot) = {
        let mut syncs = SyncObjects::new(&mut client).unwrap();
        let old = point(syncs.create_binary(true).unwrap(), 0);
        (old, syncs.snapshot(old).unwrap().unwrap())
    };
    let mut syncs = SyncObjects::new(&mut client).unwrap();
    let new = point(syncs.create_binary(true).unwrap(), 0);
    assert_eq!(new.sync.id, old.sync.id);
    assert_ne!(new.sync, old.sync);
    assert_eq!(syncs.snapshot(old), Err(Error::NotFound));
    assert_eq!(syncs.arm(snapshot), Err(Error::NotFound));
    assert!(state.borrow().calls.is_empty());
}

#[test]
fn a_malformed_fence_reply_closes_its_grant_and_latches_loss() {
    let (mut client, state) = client();
    let mut syncs = SyncObjects::new(&mut client).unwrap();
    submitted(&state, 1);
    let fence = syncs.submit(job(1), &[]).unwrap();
    // Done may not transfer a handle. The native Client validates the reply before the
    // sync store can turn a GPU payload into a host-signaled one. — MILVUS
    state.borrow_mut().replies.push_back((
        Ok(Response::WaitDone {
            point: 1,
            gpu_status: 0,
        }),
        Some(99),
    ));
    assert_eq!(syncs.arm(fence), Err(Error::Client(ClientError::Protocol)));
    assert_eq!(state.borrow().closed, [99]);
    assert_eq!(
        syncs.arm(fence),
        Err(Error::Client(ClientError::DeviceLost))
    );
    assert_eq!(state.borrow().calls.len(), 2);
}

fn armed(state: &Rc<RefCell<Script>>, point: u64, handle: u32) {
    state
        .borrow_mut()
        .replies
        .push_back((Ok(Response::WaitArmed { point }), Some(handle)));
}

#[test]
fn prepared_inputs_survive_reset_replacement_and_destroy_until_one_submit_is_accepted() {
    let (mut client, state) = client();
    let mut syncs = SyncObjects::new(&mut client).unwrap();
    let input = point(syncs.create_binary(false).unwrap(), 0);
    let timeline = point(syncs.create_timeline(0).unwrap(), 42);
    let output = point(syncs.create_binary(false).unwrap(), 0);
    submitted(&state, 1);
    let first = syncs.submit(job(1), &[input, timeline]).unwrap();
    let mut prepared = syncs
        .prepare(job(2), &[input, timeline, input], &[output])
        .unwrap();
    assert_eq!(prepared.waits, [first]);
    syncs.signal(input).unwrap();
    syncs.reset(input.sync).unwrap();
    syncs.destroy(input.sync).unwrap();
    armed(&state, 1, 77);
    assert_eq!(
        syncs.submit_prepared(&mut prepared),
        Ok(SubmitProgress::Blocked {
            dependency: first,
            wake: 77,
        })
    );
    syncs.close_wait_handle(77);
    assert!(!syncs.wait_pending(output).unwrap());
    assert_eq!(prepared.cursor, 0);
    done(&state, 1, 0);
    submitted(&state, 2);
    let SubmitProgress::Submitted(second) = syncs.submit_prepared(&mut prepared).unwrap() else {
        panic!()
    };
    assert_eq!(syncs.snapshot(output).unwrap(), Some(second));
    let calls = state.borrow().calls.len();
    assert_eq!(
        syncs.submit_prepared(&mut prepared),
        Err(Error::InvalidRequest)
    );
    assert_eq!(state.borrow().calls.len(), calls);
    assert_eq!(state.borrow().closed, [77]);
    assert_eq!(
        state.borrow().calls,
        [
            Command::Submit(job(1)),
            Command::FenceWait { group: 1, point: 1 },
            Command::FenceWait { group: 1, point: 1 },
            Command::Submit(job(2)),
        ]
    );
}

#[test]
fn every_distinct_dependency_must_finish_before_outputs_publish() {
    let (mut client, state) = client();
    let mut syncs = SyncObjects::new(&mut client).unwrap();
    let first = point(syncs.create_binary(false).unwrap(), 0);
    let second = point(syncs.create_binary(false).unwrap(), 0);
    let output = point(syncs.create_timeline(0).unwrap(), 99);
    submitted(&state, 1);
    syncs.submit(job(1), &[first]).unwrap();
    submitted(&state, 2);
    let second_fence = syncs.submit(job(2), &[second]).unwrap();
    let mut prepared = syncs.prepare(job(3), &[first, second], &[output]).unwrap();
    done(&state, 1, 0);
    armed(&state, 2, 88);
    assert_eq!(
        syncs.submit_prepared(&mut prepared),
        Ok(SubmitProgress::Blocked {
            dependency: second_fence,
            wake: 88,
        })
    );
    syncs.close_wait_handle(88);
    assert_eq!(prepared.cursor, 1);
    assert!(!syncs.wait_pending(output).unwrap());
    done(&state, 2, 0);
    submitted(&state, 3);
    assert!(matches!(
        syncs.submit_prepared(&mut prepared),
        Ok(SubmitProgress::Submitted(_))
    ));
    assert!(syncs.wait_pending(output).unwrap());
    assert_eq!(
        state
            .borrow()
            .calls
            .iter()
            .filter(|c| matches!(c, Command::FenceWait { point: 1, .. }))
            .count(),
        1
    );
    assert!(matches!(
        state.borrow().calls.last(),
        Some(Command::Submit(Submission {
            signal_point: 3,
            ..
        }))
    ));
}

#[test]
fn preparation_refuses_unsubmitted_invalid_foreign_and_excessive_inputs_without_io() {
    let (mut other_client, _) = client();
    let mut other = SyncObjects::new(&mut other_client).unwrap();
    let foreign = point(other.create_binary(true).unwrap(), 0);
    let (mut client, state) = client();
    let mut syncs = SyncObjects::new(&mut client).unwrap();
    let host = point(syncs.create_binary(true).unwrap(), 0);
    let future = point(syncs.create_timeline(7).unwrap(), 8);
    let output = point(syncs.create_binary(false).unwrap(), 0);
    assert!(matches!(
        syncs.prepare(job(1), &[host, future], &[output]),
        Err(Error::Unsubmitted { wait_index: 1 })
    ));
    assert!(matches!(
        syncs.prepare(job(1), &[output], &[output]),
        Err(Error::Unsubmitted { wait_index: 0 })
    ));
    assert!(matches!(
        syncs.prepare(job(1), &[foreign], &[output]),
        Err(Error::NotFound)
    ));
    assert!(matches!(
        syncs.prepare(job(1), &[point(host.sync, 1)], &[output]),
        Err(Error::InvalidRequest)
    ));
    assert!(matches!(
        syncs.prepare(job(1), &[host; MAX_SUBMIT_WAITS + 1], &[output]),
        Err(Error::InvalidRequest)
    ));
    assert!(matches!(
        syncs.prepare(job(1), &[future], &[output, output]),
        Err(Error::InvalidRequest)
    ));
    assert!(!syncs.wait_pending(output).unwrap());
    assert!(state.borrow().calls.is_empty());
}

#[test]
fn changed_outputs_are_revalidated_before_any_dependency_io() {
    let (mut client, state) = client();
    let mut syncs = SyncObjects::new(&mut client).unwrap();
    let input = point(syncs.create_binary(false).unwrap(), 0);
    let output = point(syncs.create_timeline(0).unwrap(), 10);
    submitted(&state, 1);
    syncs.submit(job(1), &[input]).unwrap();
    let mut prepared = syncs.prepare(job(2), &[input], &[output]).unwrap();
    syncs.signal(output).unwrap();
    assert_eq!(
        syncs.submit_prepared(&mut prepared),
        Err(Error::InvalidRequest)
    );
    syncs.destroy(output.sync).unwrap();
    assert_eq!(syncs.submit_prepared(&mut prepared), Err(Error::NotFound));
    assert_eq!(state.borrow().calls, [Command::Submit(job(1))]);
}

#[test]
fn refused_prepared_submit_preserves_outputs_and_retry_does_not_rewait_completed_inputs() {
    let (mut client, state) = client();
    let mut syncs = SyncObjects::new(&mut client).unwrap();
    let input = point(syncs.create_binary(false).unwrap(), 0);
    let output = point(syncs.create_timeline(0).unwrap(), 40);
    submitted(&state, 1);
    syncs.submit(job(1), &[input]).unwrap();
    let mut prepared = syncs.prepare(job(2), &[input], &[output]).unwrap();
    done(&state, 1, 0);
    state
        .borrow_mut()
        .replies
        .push_back((Err(Status::NoMemory), None));
    assert_eq!(
        syncs.submit_prepared(&mut prepared),
        Err(Error::Client(ClientError::Server(Status::NoMemory)))
    );
    assert!(!syncs.wait_pending(output).unwrap());
    assert!(!prepared.accepted);
    submitted(&state, 2);
    assert!(matches!(
        syncs.submit_prepared(&mut prepared),
        Ok(SubmitProgress::Submitted(_))
    ));
    assert_eq!(
        state.borrow().calls,
        [
            Command::Submit(job(1)),
            Command::FenceWait { group: 1, point: 1 },
            Command::Submit(job(2)),
            Command::Submit(job(2))
        ]
    );
}

#[test]
fn failed_dependency_prevents_submit_and_all_further_io() {
    let (mut client, state) = client();
    let mut syncs = SyncObjects::new(&mut client).unwrap();
    let input = point(syncs.create_binary(false).unwrap(), 0);
    let output = point(syncs.create_binary(false).unwrap(), 0);
    submitted(&state, 1);
    syncs.submit(job(1), &[input]).unwrap();
    let mut prepared = syncs.prepare(job(2), &[input], &[output]).unwrap();
    done(&state, 1, 0xbad);
    assert_eq!(
        syncs.submit_prepared(&mut prepared),
        Err(Error::GpuFault(0xbad))
    );
    assert_eq!(prepared.cursor, 0);
    assert!(!prepared.accepted);
    assert_eq!(
        syncs.submit_prepared(&mut prepared),
        Err(Error::Client(ClientError::DeviceLost))
    );
    assert_eq!(state.borrow().calls.len(), 2);
}

#[test]
fn uncertain_prepared_submit_cannot_be_replayed() {
    let (mut client, state) = client();
    let mut syncs = SyncObjects::new(&mut client).unwrap();
    let output = point(syncs.create_binary(false).unwrap(), 0);
    let mut prepared = syncs.prepare(job(1), &[], &[output]).unwrap();
    state.borrow_mut().fail_send = true;
    assert_eq!(
        syncs.submit_prepared(&mut prepared),
        Err(Error::Client(ClientError::Transport(
            TransportError::Closed
        )))
    );
    assert_eq!(
        syncs.submit_prepared(&mut prepared),
        Err(Error::Client(ClientError::DeviceLost))
    );
    assert_eq!(state.borrow().calls.len(), 1);
}

#[test]
fn prepared_intent_cannot_cross_store_generations() {
    let (mut client, state) = client();
    let mut prepared = {
        let mut syncs = SyncObjects::new(&mut client).unwrap();
        syncs.prepare(job(1), &[], &[]).unwrap()
    };
    let mut syncs = SyncObjects::new(&mut client).unwrap();
    assert_eq!(syncs.submit_prepared(&mut prepared), Err(Error::NotFound));
    assert!(state.borrow().calls.is_empty());
}
