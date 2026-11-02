//j744
//j745
//j751
use super::*;
use crate::{
    c, client::TransportError, Command, Reply, Request, Response, Status, REPLY_LEN, REQUEST_LEN,
};
use alloc::rc::Rc;
use core::cell::RefCell;

#[derive(Default)]
struct State {
    calls: Vec<Command>,
    ids: Vec<u32>,
    closed: Vec<u32>,
    maps: u32,
    unmaps: u32,
    next: u32,
    sessions_closed: u32,
    fail: Option<Status>,
    fail_after: usize,
    map_fail: bool,
    unmap_fail: bool,
    submitted: u64,
    completed: u64,
    gpu_status: u32,
    next_wake: u32,
    waits: Vec<(u32, u64)>,
    wait_outcome: Option<WaitOutcome>,
}
struct Wire {
    state: Rc<RefCell<State>>,
    reply: Option<Reply>,
}
impl Transport for Wire {
    type Handle = u32;
    fn send(&mut self, bytes: &[u8; REQUEST_LEN]) -> Result<(), TransportError> {
        let request = Request::decode(bytes).unwrap();
        let mut state = self.state.borrow_mut();
        state.calls.push(request.command);
        state.ids.push(request.id);
        let failure = if state.fail.is_some() && state.fail_after > 0 {
            state.fail_after -= 1;
            None
        } else {
            state.fail.take()
        };
        let result = if let Some(error) = failure {
            Err(error)
        } else {
            Ok(match request.command {
                Command::BufferCreate { bytes } => {
                    state.next += 1;
                    Response::BufferCreated {
                        buffer: state.next,
                        bytes,
                    }
                }
                Command::BufferDestroy { buffer } => Response::BufferDestroyed { buffer },
                Command::VmCreate => Response::VmCreated { vm: 1 },
                Command::VmBind(_) => Response::VmBound,
                Command::VmUnbind(range) => Response::VmUnbound(range),
                Command::GroupCreate { .. } => Response::GroupCreated { group: 1 },
                Command::Submit(job) => {
                    state.submitted = job.signal_point;
                    Response::Submitted {
                        point: job.signal_point,
                    }
                }
                Command::FenceWait { point, .. } => {
                    if point <= state.completed {
                        Response::WaitDone {
                            point,
                            gpu_status: state.gpu_status,
                        }
                    } else {
                        Response::WaitArmed { point }
                    }
                }
                _ => panic!("unexpected request"),
            })
        };
        self.reply = Some(Reply {
            id: request.id,
            result,
        });
        Ok(())
    }
    fn receive(
        &mut self,
        bytes: &mut [u8; REPLY_LEN + 1],
    ) -> Result<(usize, Option<u32>), TransportError> {
        let reply = self.reply.take().unwrap();
        bytes[..REPLY_LEN].copy_from_slice(&reply.encode());
        let handle = match reply.result {
            Ok(Response::BufferCreated { buffer, .. }) => Some(buffer + 100),
            Ok(Response::WaitArmed { .. }) => {
                let mut state = self.state.borrow_mut();
                state.next_wake += 1;
                Some(500 + state.next_wake)
            }
            _ => None,
        };
        Ok((REPLY_LEN, handle))
    }
    fn close_handle(&mut self, handle: u32) {
        self.state.borrow_mut().closed.push(handle);
    }
}
impl crate::client::SessionTransport for Wire {
    fn close_session(&mut self) {
        self.state.borrow_mut().sessions_closed += 1;
    }
}
struct Memory(Rc<RefCell<State>>);
impl Mapper<u32> for Memory {
    fn wait(&mut self, handle: &u32, timeout_ns: u64) -> WaitOutcome {
        let mut state = self.0.borrow_mut();
        state.waits.push((*handle, timeout_ns));
        state.wait_outcome.take().unwrap_or(WaitOutcome::TimedOut)
    }
    fn map(&mut self, _: &u32, _: u64) -> Result<NonNull<u8>, MapError> {
        let mut state = self.0.borrow_mut();
        state.maps += 1;
        if core::mem::take(&mut state.map_fail) {
            Err(MapError::NoMemory)
        } else {
            Ok(NonNull::new(0x1000 as *mut u8).unwrap())
        }
    }
    fn unmap(&mut self, _: NonNull<u8>, _: u64) -> Result<(), MapError> {
        let mut state = self.0.borrow_mut();
        state.unmaps += 1;
        if core::mem::take(&mut state.unmap_fail) {
            Err(MapError::Failed)
        } else {
            Ok(())
        }
    }
}
fn fixture() -> (Rc<RefCell<State>>, Client<Wire>, Memory) {
    let state = Rc::new(RefCell::new(State::default()));
    (
        state.clone(),
        Client::new(Wire {
            state: state.clone(),
            reply: None,
        }),
        Memory(state),
    )
}

#[test]
fn flags_rounding_and_gpu_wx_are_exact_and_rejections_do_no_io() {
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    for flag in [2, 8, 16, 64, 128, 1 << 31] {
        assert_eq!(buffers.create(1, flag), Err(Error::InvalidRequest));
    }
    for bytes in [0, MAX_BUFFER_BYTES + 1, u64::MAX] {
        assert_eq!(buffers.create(bytes, 0), Err(Error::InvalidRequest));
    }
    assert!(state.borrow().calls.is_empty());
    for flags in [
        0,
        EXECUTABLE,
        NO_MMAP,
        GPU_UNCACHED,
        EXECUTABLE | NO_MMAP | GPU_UNCACHED,
    ] {
        let info = buffers.create(PAGE_BYTES + 1, flags).unwrap();
        assert_eq!(info.bytes, 2 * PAGE_BYTES);
        assert_eq!(info.flags, flags);
        let bound = bind_flags(flags);
        assert!(bound.valid());
        assert_eq!(
            bound.contains(BindFlags::READ_ONLY),
            flags & EXECUTABLE != 0
        );
        assert_eq!(
            bound.contains(BindFlags::NO_EXECUTE),
            flags & EXECUTABLE == 0
        );
        assert_eq!(
            bound.contains(BindFlags::UNCACHED),
            flags & GPU_UNCACHED != 0
        );
        if flags & NO_MMAP != 0 {
            assert_eq!(buffers.map(info.id), Err(Error::InvalidRequest));
        }
        buffers.destroy(info.id).unwrap();
    }
    assert_eq!(state.borrow().maps, 0);
}

#[test]
fn lazy_mapping_is_reused_unmapped_remapped_and_destroyed_once() {
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    let bo = buffers.create(1, EXECUTABLE).unwrap();
    assert_eq!(state.borrow().maps, 0);
    let address = buffers.map(bo.id).unwrap();
    assert_eq!(buffers.map(bo.id), Ok(address));
    assert_eq!(state.borrow().maps, 1);
    buffers.unmap(bo.id).unwrap();
    buffers.unmap(bo.id).unwrap();
    assert_eq!(state.borrow().unmaps, 1);
    buffers.map(bo.id).unwrap();
    buffers.destroy(bo.id).unwrap();
    assert_eq!(state.borrow().closed, [101]);
    assert_eq!(state.borrow().unmaps, 2);
    let count = state.borrow().calls.len();
    assert_eq!(buffers.destroy(bo.id), Err(Error::NotFound));
    assert_eq!(buffers.map(bo.id), Err(Error::NotFound));
    assert_eq!(state.borrow().calls.len(), count);
    assert_eq!(buffers.create(1, 0).unwrap().id, 2);
    buffers.destroy(2).unwrap();
    drop(buffers);
    assert!(!client.is_lost());
}

#[test]
fn recoverable_server_and_map_failures_keep_resources_retryable() {
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    state.borrow_mut().fail = Some(Status::NoMemory);
    assert_eq!(
        buffers.create(1, 0),
        Err(Error::Client(ClientError::Server(Status::NoMemory)))
    );
    assert_eq!(buffers.live_count(), 0);
    let bo = buffers.create(1, 0).unwrap();
    state.borrow_mut().map_fail = true;
    assert_eq!(buffers.map(bo.id), Err(Error::Map(MapError::NoMemory)));
    buffers.map(bo.id).unwrap();
    state.borrow_mut().fail = Some(Status::Busy);
    assert_eq!(
        buffers.destroy(bo.id),
        Err(Error::Client(ClientError::Server(Status::Busy)))
    );
    assert!(state.borrow().closed.is_empty());
    assert_eq!(state.borrow().unmaps, 0);
    state.borrow_mut().unmap_fail = true;
    assert_eq!(buffers.destroy(bo.id), Err(Error::Unmap(MapError::Failed)));
    let count = state.borrow().calls.len();
    assert_eq!(buffers.map(bo.id), Err(Error::NotFound));
    buffers.destroy(bo.id).unwrap();
    assert_eq!(state.borrow().calls.len(), count);
    assert_eq!(state.borrow().closed, [101]);
}

#[test]
fn lost_or_abandoned_scope_releases_only_local_claims_and_never_replays_destroy() {
    let (state, mut client, mut mapper) = fixture();
    {
        let mut buffers = Buffers::new(&mut client, &mut mapper);
        let bo = buffers.create(1, 0).unwrap();
        buffers.map(bo.id).unwrap();
        state.borrow_mut().fail = Some(Status::DeviceLost);
        assert_eq!(
            buffers.destroy(bo.id),
            Err(Error::Client(ClientError::Server(Status::DeviceLost)))
        );
        let count = state.borrow().calls.len();
        assert_eq!(
            buffers.map(bo.id),
            Err(Error::Client(ClientError::DeviceLost))
        );
        assert_eq!(state.borrow().calls.len(), count);
    }
    assert_eq!(state.borrow().calls.len(), 2);
    assert_eq!(state.borrow().unmaps, 1);
    assert_eq!(state.borrow().closed, [101]);
    assert!(client.is_lost());
    let (state, mut client, mut mapper) = fixture();
    {
        let mut buffers = Buffers::new(&mut client, &mut mapper);
        buffers.create(1, NO_MMAP).unwrap();
    }
    assert!(client.is_lost());
    assert_eq!(state.borrow().calls.len(), 1);
    assert_eq!(state.borrow().closed, [101]);
}

#[test]
fn c_callbacks_clear_outputs_refuse_null_and_keep_ids_private() {
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    c::with_buffer_api(&mut buffers, |api| unsafe {
        let create = api.buffer_create.unwrap();
        let map = api.buffer_map.unwrap();
        let mut info = Info {
            id: 99,
            flags: 99,
            bytes: 99,
        };
        assert_eq!(
            create(core::ptr::null_mut(), 1, 0, &mut info),
            c::BAD_ARGUMENT
        );
        assert_eq!(
            create(api.context, 1, 0, core::ptr::null_mut()),
            c::BAD_ARGUMENT
        );
        assert!(state.borrow().calls.is_empty());
        assert_eq!(create(api.context, 1, 64, &mut info), c::INVALID_REQUEST);
        assert_eq!(info, Info::default());
        assert_eq!(create(api.context, 1, NO_MMAP, &mut info), c::OK);
        let mut pointer = 0xdead as *mut core::ffi::c_void;
        assert_eq!(map(api.context, info.id, &mut pointer), c::INVALID_REQUEST);
        assert!(pointer.is_null());
        assert_eq!(api.buffer_destroy.unwrap()(api.context, info.id), c::OK);
        assert_eq!(
            api.buffer_destroy.unwrap()(api.context, info.id),
            c::UNKNOWN_BUFFER
        );
        state.borrow_mut().fail = Some(Status::Busy);
        let mut query = c::Info {
            gpu_id: 99,
            ..c::Info::default()
        };
        assert_eq!(
            api.query.unwrap()(api.context, &mut query),
            c::SERVER_ERROR + Status::Busy as u32
        );
        assert_eq!(query.gpu_id, 0);
    });
}

#[test]
fn native_vm_lifetimes_use_one_service_vm_and_fresh_logical_ids() {
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    buffers.open_device().unwrap();
    assert_eq!(buffers.open_device(), Err(Error::InvalidRequest));
    let bo = buffers
        .create(2 * PAGE_BYTES, EXECUTABLE | GPU_UNCACHED)
        .unwrap();
    let vm = buffers.create_native_vm(VA_START, 8 * PAGE_BYTES).unwrap();
    let request = VmBinding {
        vm,
        buffer: bo.id,
        va: VA_START,
        offset: PAGE_BYTES,
        bytes: PAGE_BYTES,
    };
    buffers.bind_native(request).unwrap();
    let Command::VmBind(binding) = *state.borrow().calls.last().unwrap() else {
        panic!()
    };
    assert_eq!(binding.vm, 1);
    assert_eq!(
        binding.flags,
        BindFlags(BindFlags::READ_ONLY | BindFlags::UNCACHED)
    );
    let calls = state.borrow().calls.len();
    assert_eq!(buffers.bind_native(request), Err(Error::InvalidRequest));
    assert_eq!(
        buffers.bind_native(VmBinding {
            offset: 2 * PAGE_BYTES,
            ..request
        }),
        Err(Error::InvalidRequest)
    );
    assert_eq!(
        buffers.bind_native(VmBinding {
            vm: vm + 1,
            ..request
        }),
        Err(Error::NotFound)
    );
    assert_eq!(state.borrow().calls.len(), calls);
    buffers.destroy_native_vm(vm).unwrap();
    let next = buffers.create_native_vm(VA_START, 8 * PAGE_BYTES).unwrap();
    assert!(next > vm);
    assert_eq!(buffers.bind_native(request), Err(Error::NotFound));
    buffers
        .bind_native(VmBinding {
            vm: next,
            ..request
        })
        .unwrap();
    buffers.destroy(bo.id).unwrap(); // implicit unbind removes local native binding metadata
    let calls = state.borrow().calls.len();
    buffers.destroy_native_vm(next).unwrap();
    assert_eq!(state.borrow().calls.len(), calls);
    assert_eq!(
        state
            .borrow()
            .calls
            .iter()
            .filter(|c| **c == Command::VmCreate)
            .count(),
        1
    );
    buffers.close_device();
    buffers.close_device();
    assert_eq!(state.borrow().sessions_closed, 1);
    assert!(!buffers.device_ready());
}

#[test]
fn native_unbind_requires_exact_coverage_and_preserves_metadata_on_refusal() {
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    buffers.open_device().unwrap();
    let bo = buffers.create(4 * PAGE_BYTES, 0).unwrap();
    let vm = buffers
        .create_native_vm(VA_START + PAGE_BYTES, 8 * PAGE_BYTES)
        .unwrap();
    for i in [1, 2] {
        buffers
            .bind_native(VmBinding {
                vm,
                buffer: bo.id,
                va: VA_START + i * PAGE_BYTES,
                offset: 0,
                bytes: PAGE_BYTES,
            })
            .unwrap();
    }
    let calls = state.borrow().calls.len();
    for (va, bytes) in [
        (VA_START, PAGE_BYTES),
        (VA_START + 2 * PAGE_BYTES, 2 * PAGE_BYTES),
        (VA_START + PAGE_BYTES + 1, PAGE_BYTES),
        (u64::MAX, PAGE_BYTES),
    ] {
        assert!(buffers.unbind_native(vm, va, bytes).is_err());
    }
    assert_eq!(state.borrow().calls.len(), calls);
    state.borrow_mut().fail = Some(crate::Status::NoMemory);
    assert!(buffers
        .unbind_native(vm, VA_START + PAGE_BYTES, 2 * PAGE_BYTES)
        .is_err());
    buffers
        .unbind_native(vm, VA_START + PAGE_BYTES, 2 * PAGE_BYTES)
        .unwrap();
    assert!(buffers.bindings.is_empty());
    buffers.destroy(bo.id).unwrap();
    buffers.destroy_native_vm(vm).unwrap();
    buffers.close_device();
}

#[test]
fn native_scope_closes_on_abandonment_and_failure_without_replaying_work() {
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    c::with_device_api(&mut buffers, |api| unsafe {
        assert_eq!(api.device_open.unwrap()(api.context), c::OK);
        let mut vm = 999;
        assert_eq!(
            api.vm_create.unwrap()(api.context, VA_START, PAGE_BYTES, &mut vm),
            c::OK
        );
        assert_ne!(vm, 0);
        let mut info = Info::default();
        assert_eq!(
            api.buffer_create.unwrap()(api.context, PAGE_BYTES, 0, &mut info),
            c::OK
        );
        let mut pointer = core::ptr::null_mut();
        assert_eq!(
            api.buffer_map.unwrap()(api.context, info.id, &mut pointer),
            c::OK
        );
        // Leave children live: scope exit owns local teardown and endpoint close.
    });
    assert_eq!(state.borrow().sessions_closed, 1);
    assert_eq!(state.borrow().closed, [101]);
    assert_eq!(state.borrow().unmaps, 1);
    assert!(!state
        .borrow()
        .calls
        .iter()
        .any(|c| matches!(c, Command::BufferDestroy { .. })));
    assert_eq!(
        buffers.open_device(),
        Err(Error::Client(ClientError::DeviceLost))
    );
}

#[test]
fn native_vm_destroy_failure_is_retryable_without_reissuing_successful_unbinds() {
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    buffers.open_device().unwrap();
    let bo = buffers.create(PAGE_BYTES, 0).unwrap();
    let vm = buffers.create_native_vm(VA_START, 2 * PAGE_BYTES).unwrap();
    for va in [VA_START, VA_START + PAGE_BYTES] {
        buffers
            .bind_native(VmBinding {
                vm,
                buffer: bo.id,
                va,
                offset: 0,
                bytes: PAGE_BYTES,
            })
            .unwrap();
    }
    state.borrow_mut().fail = Some(crate::Status::NoMemory);
    state.borrow_mut().fail_after = 1;
    assert!(buffers.destroy_native_vm(vm).is_err());
    assert_eq!(buffers.bindings.len(), 1);
    buffers.destroy_native_vm(vm).unwrap();
    assert!(buffers.bindings.is_empty());
    let unbound: Vec<u64> = state
        .borrow()
        .calls
        .iter()
        .filter_map(|command| match command {
            Command::VmUnbind(range) => Some(range.va),
            _ => None,
        })
        .collect();
    assert_eq!(unbound, [VA_START + PAGE_BYTES, VA_START, VA_START]);
    buffers.destroy(bo.id).unwrap();
    buffers.close_device();
}

fn native(api: &c::Api, request: crate::c_sync::Request) -> (u32, crate::c_sync::Reply) {
    let mut reply = crate::c_sync::Reply {
        id: u64::MAX,
        state: u32::MAX,
        gpu_status: u32::MAX,
        value: u64::MAX,
    };
    let status = unsafe { api.native.unwrap()(api.context, &request, &mut reply) };
    (status, reply)
}
fn native_request(operation: u32, id: u64, value: u64) -> crate::c_sync::Request {
    crate::c_sync::Request {
        operation,
        id,
        value,
        ..Default::default()
    }
}
fn native_open(api: &c::Api) -> u32 {
    assert_eq!(unsafe { api.device_open.unwrap()(api.context) }, c::OK);
    let mut vm = 0;
    assert_eq!(
        unsafe { api.vm_create.unwrap()(api.context, VA_START, VA_END - VA_START, &mut vm) },
        c::OK
    );
    vm
}
fn native_group(api: &c::Api, vm: u32) -> u64 {
    let (status, group) = native(
        api,
        native_request(crate::c_sync::GROUP_CREATE, 0, u64::from(vm)),
    );
    assert_eq!(status, c::OK);
    group.id
}
fn native_stream(api: &c::Api, vm: u32) -> u32 {
    let mut info = Info::default();
    assert_eq!(
        unsafe { api.buffer_create.unwrap()(api.context, PAGE_BYTES, EXECUTABLE, &mut info) },
        c::OK
    );
    let binding = VmBinding {
        vm,
        buffer: info.id,
        va: VA_START,
        offset: 0,
        bytes: PAGE_BYTES,
    };
    assert_eq!(
        unsafe { api.vm_bind.unwrap()(api.context, &binding) },
        c::OK
    );
    info.id
}
fn native_job(
    api: &c::Api,
    vm: u32,
    group: u64,
    point: u64,
    waits: &[crate::c_sync::Point],
    signals: &[crate::c_sync::Point],
) -> (u32, crate::c_sync::Reply) {
    use crate::c_sync as n;
    native(
        api,
        n::Request {
            operation: n::PREPARE,
            id: group,
            job: n::Job {
                vm,
                stream_bytes: 32,
                stream_va: VA_START,
                signal_point: point,
            },
            waits: waits.as_ptr(),
            signals: signals.as_ptr(),
            wait_count: waits.len() as u32,
            signal_count: signals.len() as u32,
            ..Default::default()
        },
    )
}

#[test]
fn native_callback_is_device_only_and_malformed_requests_clear_outputs_without_io() {
    use crate::c_sync as n;
    let (state, mut client, mut mapper) = fixture();
    c::with_api(&mut client, |api| assert!(api.native.is_none()));
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    c::with_buffer_api(&mut buffers, |api| assert!(api.native.is_none()));
    c::with_device_api(&mut buffers, |api| {
        assert_eq!(api.version, 13);
        assert_eq!(api.bytes, 120);
        let _vm = native_open(api);
        let before = state.borrow().calls.len();
        for request in [
            n::Request {
                flags: 1,
                ..native_request(n::CREATE_BINARY, 0, 0)
            },
            native_request(n::CREATE_BINARY, 0, 2),
            native_request(100, 0, 0),
            n::Request {
                signal_count: 65,
                ..native_request(n::PREPARE, 1, 0)
            },
            n::Request {
                wait_count: 1,
                ..native_request(n::PREPARE, 1, 0)
            },
            native_request(n::WAIT, 1, 0),
            native_request(n::WAIT, 1, n::MAX_TIMEOUT_NS + 1),
        ] {
            assert_eq!(native(api, request), (c::BAD_ARGUMENT, n::Reply::default()));
        }
        let mut out = n::Reply {
            id: 99,
            ..Default::default()
        };
        assert_eq!(
            unsafe { api.native.unwrap()(api.context, core::ptr::null(), &mut out) },
            c::BAD_ARGUMENT
        );
        assert_eq!(out, n::Reply::default());
        assert_eq!(state.borrow().calls.len(), before);
    });
    assert_eq!(state.borrow().sessions_closed, 1);
}

#[test]
fn native_metadata_persists_between_callbacks_and_foreign_or_wrong_kind_ids_refuse() {
    use crate::c_sync as n;
    let (_other_state, mut other_client, mut other_mapper) = fixture();
    let mut other = Buffers::new(&mut other_client, &mut other_mapper);
    let foreign = c::with_device_api(&mut other, |api| {
        native_open(api);
        native(api, native_request(n::CREATE_BINARY, 0, 1)).1.id
    });
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    c::with_device_api(&mut buffers, |api| {
        native_open(api);
        let (status, binary) = native(api, native_request(n::CREATE_BINARY, 0, 1));
        assert_eq!(status, c::OK);
        assert_ne!(binary.id, foreign);
        assert_eq!(
            native(api, native_request(n::PENDING, foreign, 0)),
            (n::UNKNOWN_OBJECT, n::Reply::default())
        );
        let snapshot = native(api, native_request(n::SNAPSHOT, binary.id, 0)).1;
        assert_eq!(
            native(api, native_request(n::PENDING, snapshot.id, 0)).0,
            n::UNKNOWN_OBJECT
        );
        assert_eq!(native(api, native_request(n::RESET, binary.id, 0)).0, c::OK);
        assert_eq!(
            native(api, native_request(n::PENDING, binary.id, 0))
                .1
                .state,
            n::UNAVAILABLE
        );
        assert_eq!(
            native(api, native_request(n::ARM, snapshot.id, 0)).1.state,
            n::COMPLETE
        );
        assert_eq!(state.borrow().calls, [Command::VmCreate]);
    });
}

#[test]
fn native_submits_share_the_bo_endpoint_capture_inputs_and_publish_once() {
    use crate::c_sync as n;
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    c::with_device_api(&mut buffers, |api| {
        let vm = native_open(api);
        native_stream(api, vm);
        let group = native_group(api, vm);
        let input = native(api, native_request(n::CREATE_BINARY, 0, 0)).1.id;
        let output = native(api, native_request(n::CREATE_TIMELINE, 0, 0)).1.id;
        let a = n::Point {
            id: input,
            value: 0,
        };
        let b = n::Point {
            id: output,
            value: 50,
        };
        let first = native_job(api, vm, group, 1, &[], &[a]);
        assert_eq!(first.0, c::OK);
        assert_eq!(
            native(api, native_request(n::STEP, first.1.id, 0)).1.state,
            n::SUBMITTED
        );
        let second = native_job(api, vm, group, 2, &[a], &[b]);
        assert_eq!(second.0, c::OK);
        native(api, native_request(n::RESET, input, 0));
        native(api, native_request(n::DROP, input, 0));
        let blocked = native(api, native_request(n::STEP, second.1.id, 0));
        assert_eq!(blocked.1.state, n::AVAILABLE);
        assert_eq!(
            native(api, native_request(n::WAIT, blocked.1.id, 1000))
                .1
                .state,
            n::TIMED_OUT
        );
        assert_eq!(
            native(api, native_request(n::PENDING, output, 50)).1.state,
            n::UNAVAILABLE
        );
        state.borrow_mut().completed = 1;
        state.borrow_mut().fail = Some(Status::NoMemory);
        // First re-arm is the recoverable refusal; the intent and outputs remain intact.
        assert_ne!(
            native(api, native_request(n::STEP, second.1.id, 0)).0,
            c::OK
        );
        assert_eq!(
            native(api, native_request(n::PENDING, output, 50)).1.state,
            n::UNAVAILABLE
        );
        let accepted = native(api, native_request(n::STEP, second.1.id, 0));
        assert_eq!(accepted.0, c::OK);
        assert_eq!(accepted.1.state, n::SUBMITTED);
        let before = state.borrow().calls.len();
        assert_eq!(
            native(api, native_request(n::STEP, second.1.id, 0)).0,
            c::INVALID_REQUEST
        );
        assert_eq!(state.borrow().calls.len(), before);
    });
    let state = state.borrow();
    assert_eq!(state.ids, (1..=state.ids.len() as u32).collect::<Vec<_>>());
    assert_eq!(
        state
            .calls
            .iter()
            .filter(|c| matches!(c, Command::VmCreate))
            .count(),
        1
    );
    assert!(!state
        .calls
        .iter()
        .any(|c| matches!(c, Command::SyncRead { .. })));
}

#[test]
fn native_wait_consumes_and_closes_handles_for_every_outcome_and_scope_cleanup() {
    use crate::c_sync as n;
    for outcome in [
        WaitOutcome::Woken,
        WaitOutcome::TimedOut,
        WaitOutcome::Closed,
        WaitOutcome::Failed,
        WaitOutcome::Unsupported,
    ] {
        let (state, mut client, mut mapper) = fixture();
        let mut buffers = Buffers::new(&mut client, &mut mapper);
        c::with_device_api(&mut buffers, |api| {
            let vm = native_open(api);
            native_stream(api, vm);
            let group = native_group(api, vm);
            let job = native_job(api, vm, group, 1, &[], &[]).1;
            let snapshot = native(api, native_request(n::STEP, job.id, 0)).1;
            let wake = native(api, native_request(n::ARM, snapshot.id, 0)).1;
            state.borrow_mut().wait_outcome = Some(outcome);
            let result = native(api, native_request(n::WAIT, wake.id, 2000));
            match outcome {
                WaitOutcome::Woken => assert_eq!(result.1.state, n::WOKEN),
                WaitOutcome::TimedOut => assert_eq!(result.1.state, n::TIMED_OUT),
                WaitOutcome::Closed => assert_eq!(result.1.state, n::CLOSED),
                _ => assert_ne!(result.0, c::OK),
            }
            assert_eq!(
                native(api, native_request(n::WAIT, wake.id, 2000)).0,
                n::UNKNOWN_OBJECT
            );
            assert_eq!(state.borrow().closed, [501]);
            // Woken is not completion. A second arm is still pending and scope close owns it.
            assert_eq!(
                native(api, native_request(n::ARM, snapshot.id, 0)).1.state,
                n::AVAILABLE
            );
        });
        assert!(state.borrow().closed.contains(&502));
        assert_eq!(
            state.borrow().closed.iter().filter(|h| **h == 501).count(),
            1
        );
        assert_eq!(
            state.borrow().closed.iter().filter(|h| **h == 502).count(),
            1
        );
        assert_eq!(state.borrow().sessions_closed, 1);
    }
}

#[test]
fn native_job_refuses_a_destroyed_stream_before_waiting_or_publishing() {
    use crate::c_sync as n;
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    c::with_device_api(&mut buffers, |api| {
        let vm = native_open(api);
        let stream = native_stream(api, vm);
        let group = native_group(api, vm);
        let job = native_job(api, vm, group, 1, &[], &[]).1;
        assert_eq!(
            unsafe { api.buffer_destroy.unwrap()(api.context, stream) },
            c::OK
        );
        let before = state.borrow().calls.len();
        assert_eq!(
            native(api, native_request(n::STEP, job.id, 0)).0,
            c::INVALID_REQUEST
        );
        assert_eq!(state.borrow().calls.len(), before);
    });
}

#[test]
fn native_table_quota_and_stale_ids_refuse_without_gpu_io() {
    use crate::c_sync as n;
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    c::with_device_api(&mut buffers, |api| {
        native_open(api);
        let binary = native(api, native_request(n::CREATE_BINARY, 0, 1)).1.id;
        let old = native(api, native_request(n::SNAPSHOT, binary, 0)).1.id;
        for _ in 2..n::MAX_OBJECTS {
            assert_eq!(native(api, native_request(n::SNAPSHOT, binary, 0)).0, c::OK);
        }
        assert_eq!(
            native(api, native_request(n::SNAPSHOT, binary, 0)),
            (c::NO_MEMORY, n::Reply::default())
        );
        assert_eq!(native(api, native_request(n::DROP, old, 0)).0, c::OK);
        assert_ne!(
            native(api, native_request(n::SNAPSHOT, binary, 0)).1.id,
            old
        );
        assert_eq!(
            native(api, native_request(n::ARM, old, 0)).0,
            n::UNKNOWN_OBJECT
        );
        assert_eq!(state.borrow().calls, [Command::VmCreate]);
    });
}
