//j751
//j752
//j757
//j779
//j789
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
    report: Option<crate::GroupReport>,
    read_fail: bool,
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
                Command::GroupCreate { .. } | Command::GroupCreateQueues { .. } => {
                    Response::GroupCreated { group: 1 }
                }
                Command::GroupWatch { group } => Response::GroupWatch {
                    group,
                    report: crate::GroupReport::USABLE,
                },
                Command::Submit(job) => {
                    state.submitted = job.signal_point;
                    Response::Submitted {
                        point: job.signal_point,
                    }
                }
                Command::SubmitStreams(job) => {
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
                //j757
                Command::HeapCreate(spec) => {
                    assert_eq!(spec.vm, 1, "heaps always name the server VM");
                    state.next += 1;
                    let context_va = crate::HEAP_VA_START + u64::from(state.next) * 0x40_0000;
                    Response::HeapCreated {
                        heap: state.next,
                        placement: crate::HeapPlacement {
                            context_va,
                            first_chunk_va: context_va + PAGE_BYTES,
                        },
                    }
                }
                Command::HeapDestroy { heap } => Response::HeapDestroyed {
                    heap,
                    bytes: 3 * PAGE_BYTES,
                },
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
            Ok(Response::GroupWatch { .. }) => Some(700),
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
    fn read(&mut self, handle: &u32, bytes: &mut [u8]) -> Result<(), MapError> {
        assert_eq!(*handle, 700);
        let state = self.0.borrow();
        if state.read_fail {
            return Err(MapError::Failed);
        }
        let report = state.report.unwrap_or(if state.gpu_status != 0 {
            crate::GroupReport {
                state: crate::GroupState::Fatal,
                gpu_status: state.gpu_status,
            }
        } else {
            crate::GroupReport::USABLE
        });
        bytes.copy_from_slice(&report.encode(1));
        Ok(())
    }
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
        assert_eq!(api.version, 16);
        assert_eq!(api.bytes, 136);
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

#[test]
fn group_state_keeps_positive_driver_facts_after_shared_owner_close() {
    use crate::{c_sync as n, GroupReport, GroupState};
    for report in [
        GroupReport::TIMED_OUT,
        GroupReport {
            state: GroupState::Fatal,
            gpu_status: 0xbeef,
        },
    ] {
        let (state, mut client, mut mapper) = fixture();
        let mut buffers = Buffers::new(&mut client, &mut mapper);
        c::with_device_api(&mut buffers, |api| {
            let vm = native_open(api);
            let group = native_group(api, vm);
            assert_eq!(
                native(api, native_request(n::GROUP_STATE, group, 0))
                    .1
                    .value,
                GroupState::Usable as u64
            );
            state.borrow_mut().report = Some(report);
            let before = state.borrow().calls.len();
            let observed = native(api, native_request(n::GROUP_STATE, group, 0));
            assert_eq!(observed.0, c::OK);
            assert_eq!(observed.1.value, report.state as u64);
            assert_eq!(observed.1.gpu_status, report.gpu_status);
            assert_eq!(
                unsafe { api.device_status.unwrap()(api.context) },
                c::DEVICE_LOST
            );
            assert_eq!(unsafe { api.device_close.unwrap()(api.context) }, c::OK);
            state.borrow_mut().read_fail = true;
            let cached = native(api, native_request(n::GROUP_STATE, group, 0));
            assert_eq!(cached, observed);
            assert_eq!(
                native(api, native_request(n::GROUP_STATE, group + 100000, 0)).0,
                n::UNKNOWN_OBJECT
            );
            assert_eq!(state.borrow().calls.len(), before);
        });
        assert_eq!(
            state.borrow().closed.iter().filter(|h| **h == 700).count(),
            1
        );
        assert_eq!(state.borrow().sessions_closed, 1);
    }
}

#[test]
fn group_state_never_calls_a_closed_or_unreadable_group_usable() {
    use crate::{c_sync as n, GroupState};
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    c::with_device_api(&mut buffers, |api| {
        let vm = native_open(api);
        let group = native_group(api, vm);
        state.borrow_mut().read_fail = true;
        assert_eq!(
            native(api, native_request(n::GROUP_STATE, group, 0))
                .1
                .value,
            GroupState::Unknown as u64
        );
        assert_eq!(unsafe { api.device_close.unwrap()(api.context) }, c::OK);
        assert_eq!(
            native(api, native_request(n::GROUP_STATE, group, 0))
                .1
                .value,
            GroupState::Unknown as u64
        );
    });
    let (_, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    c::with_device_api(&mut buffers, |api| {
        let vm = native_open(api);
        let group = native_group(api, vm);
        assert_eq!(unsafe { api.device_close.unwrap()(api.context) }, c::OK);
        assert_eq!(
            native(api, native_request(n::GROUP_STATE, group, 0))
                .1
                .value,
            GroupState::Unknown as u64
        );
    });
}

#[test]
fn unreadable_watch_at_group_creation_closes_the_grant_and_latches_loss() {
    use crate::c_sync as n;
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    c::with_device_api(&mut buffers, |api| {
        let vm = native_open(api);
        state.borrow_mut().read_fail = true;
        assert_eq!(
            native(api, native_request(n::GROUP_CREATE, 0, vm as u64)),
            (c::PROTOCOL, n::Reply::default())
        );
        assert_eq!(state.borrow().closed, [700]);
        assert_eq!(
            unsafe { api.device_status.unwrap()(api.context) },
            c::DEVICE_LOST
        );
    });
    assert_eq!(state.borrow().closed, [700]);
}

//j757
fn heap_request(vm: u32, chunk_bytes: u32, initial_chunks: u32, max_chunks: u32) -> c::HeapRequest {
    c::HeapRequest {
        vm,
        chunk_bytes,
        initial_chunks,
        max_chunks,
        target_in_flight: 65535,
        flags: 0,
    }
}
fn heap_create(api: &c::Api, request: c::HeapRequest) -> (u32, c::HeapInfo) {
    let mut info = c::HeapInfo {
        heap: u32::MAX,
        reserved: u32::MAX,
        context_va: u64::MAX,
        first_chunk_va: u64::MAX,
    };
    let status = unsafe { api.heap_create.unwrap()(api.context, &request, &mut info) };
    (status, info)
}

#[test]
fn heap_callbacks_are_device_only_and_refuse_bad_shapes_without_io() {
    let (state, mut client, mut mapper) = fixture();
    c::with_api(&mut client, |api| {
        assert!(api.heap_create.is_none() && api.heap_destroy.is_none())
    });
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    c::with_buffer_api(&mut buffers, |api| assert!(api.heap_create.is_none()));
    c::with_device_api(&mut buffers, |api| {
        let vm = native_open(api);
        let before = state.borrow().calls.len();
        let k128 = 128 * 1024;
        for bad in [
            c::HeapRequest {
                flags: 1,
                ..heap_request(vm, k128, 1, 1)
            },
            heap_request(vm, 64 * 1024, 1, 1),
            heap_request(vm, k128 + 512, 1, 1),
            heap_request(vm, k128, 0, 1),
            heap_request(vm, k128, 2, 1),
        ] {
            let (status, info) = heap_create(api, bad);
            assert_eq!(status, c::INVALID_REQUEST, "{bad:?}");
            assert_eq!(info, c::HeapInfo::default());
        }
        // An unknown logical VM is refused locally too.
        assert_eq!(
            heap_create(api, heap_request(vm + 1, k128, 1, 1)).0,
            c::UNKNOWN_BUFFER
        );
        let mut bytes = u64::MAX;
        assert_eq!(
            unsafe { api.heap_destroy.unwrap()(api.context, 7, &mut bytes) },
            crate::c_sync::UNKNOWN_OBJECT
        );
        assert_eq!(bytes, 0);
        assert_eq!(state.borrow().calls.len(), before);
    });
}

#[test]
fn heaps_cross_on_the_server_vm_cap_at_eight_and_return_their_bytes() {
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    c::with_device_api(&mut buffers, |api| {
        let vm = native_open(api);
        let mut heaps = Vec::new();
        for _ in 0..crate::MAX_HEAPS {
            let (status, info) = heap_create(api, heap_request(vm, 2 * 1024 * 1024, 5, 64));
            assert_eq!(status, c::OK);
            assert!(crate::HeapPlacement {
                context_va: info.context_va,
                first_chunk_va: info.first_chunk_va
            }
            .valid());
            assert_eq!(info.reserved, 0);
            heaps.push(info.heap);
        }
        let before = state.borrow().calls.len();
        assert_eq!(
            heap_create(api, heap_request(vm, 128 * 1024, 1, 1)).0,
            c::NO_MEMORY
        );
        assert_eq!(state.borrow().calls.len(), before);
        let mut bytes = 0;
        assert_eq!(
            unsafe { api.heap_destroy.unwrap()(api.context, heaps[2], &mut bytes) },
            c::OK
        );
        assert_eq!(bytes, 3 * PAGE_BYTES);
        assert_eq!(
            unsafe { api.heap_destroy.unwrap()(api.context, heaps[2], &mut bytes) },
            crate::c_sync::UNKNOWN_OBJECT
        );
        assert_eq!(
            heap_create(api, heap_request(vm, 128 * 1024, 1, 1)).0,
            c::OK
        );
    });
}

#[test]
fn destroying_the_logical_vm_destroys_its_heaps_first() {
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    c::with_device_api(&mut buffers, |api| {
        let vm = native_open(api);
        let first = heap_create(api, heap_request(vm, 128 * 1024, 1, 2)).1.heap;
        let second = heap_create(api, heap_request(vm, 128 * 1024, 1, 2)).1.heap;
        let before = state.borrow().calls.len();
        assert_eq!(unsafe { api.vm_destroy.unwrap()(api.context, vm) }, c::OK);
        let calls = state.borrow().calls[before..].to_vec();
        assert_eq!(
            calls,
            [
                Command::HeapDestroy { heap: second },
                Command::HeapDestroy { heap: first }
            ]
        );
        // The new logical VM starts with no heaps.
        let mut fresh = 0;
        assert_eq!(
            unsafe { api.vm_create.unwrap()(api.context, VA_START, VA_END - VA_START, &mut fresh) },
            c::OK
        );
        let mut bytes = 0;
        assert_eq!(
            unsafe { api.heap_destroy.unwrap()(api.context, first, &mut bytes) },
            crate::c_sync::UNKNOWN_OBJECT
        );
    });
}

#[test]
fn a_lost_device_creates_no_heap() {
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    c::with_device_api(&mut buffers, |api| {
        let vm = native_open(api);
        state.borrow_mut().fail = Some(Status::DeviceLost);
        assert_ne!(
            heap_create(api, heap_request(vm, 128 * 1024, 1, 1)).0,
            c::OK
        );
        let before = state.borrow().calls.len();
        let (status, info) = heap_create(api, heap_request(vm, 128 * 1024, 1, 1));
        assert_eq!(status, c::DEVICE_LOST);
        assert_eq!(info, c::HeapInfo::default());
        assert_eq!(state.borrow().calls.len(), before);
    });
}

/// VK2-a (ABI 16, PLAN/015 §8.2): a three-queue native group and one PREPARE of subqueue
/// streams become one GroupCreateQueues and one SubmitStreams, with no other I/O.
#[test]
fn native_three_queue_group_carries_subqueue_streams_under_one_point() {
    use crate::c_sync as n;
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    c::with_device_api(&mut buffers, |api| {
        assert_eq!(api.version, 16);
        let vm = native_open(api);
        native_stream(api, vm);
        let (status, group) = native(
            api,
            n::Request {
                flags: n::GROUP_QUEUES,
                ..native_request(n::GROUP_CREATE, 0, u64::from(vm))
            },
        );
        assert_eq!(status, c::OK);
        assert!(matches!(
            state.borrow().calls.iter().rev().nth(1),
            Some(Command::GroupCreateQueues { vm: 1, queues: 3 })
        ));
        let streams = [
            n::Stream {
                queue: 0,
                stream_bytes: 32,
                stream_va: VA_START,
            },
            n::Stream {
                queue: 1,
                stream_bytes: 32,
                stream_va: VA_START + 64,
            },
            n::Stream {
                queue: 2,
                stream_bytes: 32,
                stream_va: VA_START + 128,
            },
        ];
        let prepare = |streams: &[n::Stream], point| n::Request {
            operation: n::PREPARE,
            id: group.id,
            job: n::Job {
                vm,
                stream_bytes: 0,
                stream_va: 0,
                signal_point: point,
            },
            streams: streams.as_ptr(),
            stream_count: streams.len() as u32,
            ..Default::default()
        };
        let (status, job) = native(api, prepare(&streams, 1));
        assert_eq!(status, c::OK);
        let before = state.borrow().calls.len();
        assert_eq!(
            native(api, native_request(n::STEP, job.id, 0)).1.state,
            n::SUBMITTED
        );
        let calls = state.borrow().calls[before..].to_vec();
        assert!(matches!(
            calls.as_slice(),
            [Command::SubmitStreams(m)] if m.count == 3 && m.signal_point == 1
                && m.streams.iter().enumerate().all(|(q, s)| usize::from(s.queue) == q)
        ));
    });
}

#[test]
fn native_stream_requests_refuse_bad_shapes_without_io() {
    use crate::c_sync as n;
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    c::with_device_api(&mut buffers, |api| {
        let vm = native_open(api);
        native_stream(api, vm);
        // A one-queue group: only queue 0 exists.
        let group = native_group(api, vm);
        let one = [n::Stream {
            queue: 1,
            stream_bytes: 32,
            stream_va: VA_START,
        }];
        let two = [
            n::Stream {
                queue: 1,
                stream_bytes: 32,
                stream_va: VA_START,
            },
            n::Stream {
                queue: 0,
                stream_bytes: 32,
                stream_va: VA_START + 64,
            },
        ];
        let four = [n::Stream {
            queue: 0,
            stream_bytes: 32,
            stream_va: VA_START,
        }; 4];
        let base = |streams: &[n::Stream]| n::Request {
            operation: n::PREPARE,
            id: group,
            job: n::Job {
                vm,
                stream_bytes: 0,
                stream_va: 0,
                signal_point: 1,
            },
            streams: streams.as_ptr(),
            stream_count: streams.len() as u32,
            ..Default::default()
        };
        let before = state.borrow().calls.len();
        let cases: [(&str, n::Request, u32); 7] = [
            ("queue beyond the group", base(&one), c::INVALID_REQUEST),
            ("descending queues", base(&two), c::INVALID_REQUEST),
            ("four streams", base(&four), c::BAD_ARGUMENT),
            (
                "job stream beside streams",
                n::Request {
                    job: n::Job {
                        vm,
                        stream_bytes: 32,
                        stream_va: VA_START,
                        signal_point: 1,
                    },
                    ..base(&one)
                },
                c::BAD_ARGUMENT,
            ),
            (
                "null streams",
                n::Request {
                    streams: core::ptr::null(),
                    ..base(&one)
                },
                c::BAD_ARGUMENT,
            ),
            (
                "reserved word",
                n::Request {
                    reserved: 1,
                    ..base(&one)
                },
                c::BAD_ARGUMENT,
            ),
            (
                "streams on another operation",
                n::Request {
                    streams: one.as_ptr(),
                    stream_count: 1,
                    ..native_request(n::GROUP_STATE, group, 0)
                },
                c::BAD_ARGUMENT,
            ),
        ];
        for (name, request, want) in cases {
            assert_eq!(native(api, request).0, want, "{name}");
        }
        // The three-queue flag belongs to GROUP_CREATE alone.
        assert_eq!(
            native(
                api,
                n::Request {
                    flags: n::GROUP_QUEUES,
                    ..native_request(n::CREATE_BINARY, 0, 0)
                }
            )
            .0,
            c::BAD_ARGUMENT
        );
        assert_eq!(state.borrow().calls.len(), before);
    });
}

#[test]
fn native_multi_stream_step_rechecks_a_later_queue_binding_without_io() {
    use crate::c_sync as n;
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    c::with_device_api(&mut buffers, |api| {
        let vm = native_open(api);
        native_stream(api, vm);
        let mut later = Info::default();
        assert_eq!(
            unsafe { api.buffer_create.unwrap()(api.context, PAGE_BYTES, EXECUTABLE, &mut later) },
            c::OK
        );
        let binding = VmBinding {
            vm,
            buffer: later.id,
            va: VA_START + PAGE_BYTES,
            offset: 0,
            bytes: PAGE_BYTES,
        };
        assert_eq!(
            unsafe { api.vm_bind.unwrap()(api.context, &binding) },
            c::OK
        );
        let (status, group) = native(
            api,
            n::Request {
                flags: n::GROUP_QUEUES,
                ..native_request(n::GROUP_CREATE, 0, u64::from(vm))
            },
        );
        assert_eq!(status, c::OK);
        let streams = [
            n::Stream {
                queue: 0,
                stream_bytes: 32,
                stream_va: VA_START,
            },
            n::Stream {
                queue: 2,
                stream_bytes: 32,
                stream_va: binding.va,
            },
        ];
        let (status, job) = native(
            api,
            n::Request {
                operation: n::PREPARE,
                id: group.id,
                job: n::Job {
                    vm,
                    signal_point: 1,
                    ..Default::default()
                },
                streams: streams.as_ptr(),
                stream_count: streams.len() as u32,
                ..Default::default()
            },
        );
        assert_eq!(status, c::OK);
        assert_eq!(
            unsafe { api.buffer_destroy.unwrap()(api.context, later.id) },
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

/// VK6-a (PLAN/015 §8.49 D1a): a device close inside the scope, which is how the C bridge
/// reports loss, keeps every live CPU alias with its handle; unmap and destroy then release
/// locally without messaging the closed session. — TIMBERDOODLE
#[test]
fn lost_session_keeps_live_aliases_until_unmap_or_destroy() {
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    buffers.open_device().unwrap();
    let mapped = buffers.create(PAGE_BYTES, 0).unwrap();
    let unmapped = buffers.create(PAGE_BYTES, 0).unwrap();
    let second = buffers.create(PAGE_BYTES, 0).unwrap();
    let alias = buffers.map(mapped.id).unwrap();
    buffers.map(second.id).unwrap();
    buffers.close_device();
    assert_eq!(state.borrow().sessions_closed, 1);
    assert_eq!(state.borrow().unmaps, 0);
    assert_eq!(state.borrow().closed, [102]);
    assert_eq!(buffers.live_count(), 2);
    assert!(!buffers.device_ready());
    // No new aliases on a lost session; the kept one is untouched.
    let calls = state.borrow().calls.len();
    for id in [unmapped.id, mapped.id] {
        assert_eq!(buffers.map(id), Err(Error::Client(ClientError::DeviceLost)));
    }
    assert_eq!(state.borrow().maps, 2);
    let _ = alias;
    // Mesa's free path: munmap, then destroy. Neither sends a request.
    buffers.unmap(mapped.id).unwrap();
    assert_eq!(state.borrow().unmaps, 1);
    buffers.destroy(mapped.id).unwrap();
    assert_eq!(state.borrow().closed, [102, 101]);
    // A destroy of a still-mapped orphan unmaps it first.
    buffers.destroy(second.id).unwrap();
    assert_eq!(state.borrow().unmaps, 2);
    assert_eq!(state.borrow().closed, [102, 101, 103]);
    assert_eq!(buffers.destroy(unmapped.id), Err(Error::NotFound));
    assert_eq!(state.borrow().calls.len(), calls);
    assert!(!state
        .borrow()
        .calls
        .iter()
        .any(|c| matches!(c, Command::BufferDestroy { .. })));
    assert_eq!(buffers.live_count(), 0);
    buffers.close_device();
    assert_eq!(state.borrow().sessions_closed, 1);
}

/// VK6-a: the scope's end releases aliases a device close kept, so nothing outlives the API.
#[test]
fn scope_end_releases_aliases_kept_after_loss() {
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    c::with_device_api(&mut buffers, |api| unsafe {
        assert_eq!(api.device_open.unwrap()(api.context), c::OK);
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
        // The bridge's loss: poison closes the device; the alias survives it.
        assert_eq!(api.device_close.unwrap()(api.context), c::OK);
        assert_eq!(state.borrow().unmaps, 0);
        assert!(state.borrow().closed.is_empty());
        assert_ne!(api.device_status.unwrap()(api.context), c::OK);
        // The app never frees it; the scope's end must.
    });
    assert_eq!(state.borrow().sessions_closed, 1);
    assert_eq!(state.borrow().unmaps, 1);
    assert_eq!(state.borrow().closed, [101]);
    assert_eq!(buffers.live_count(), 0);
}

/// VK6-a: after loss the C bridge's free path (unmap, then destroy) succeeds, so Mesa's BO
/// teardown no longer re-poisons a lost device with NotFound.
#[test]
fn c_free_path_after_loss_succeeds_locally() {
    let (state, mut client, mut mapper) = fixture();
    let mut buffers = Buffers::new(&mut client, &mut mapper);
    c::with_device_api(&mut buffers, |api| unsafe {
        assert_eq!(api.device_open.unwrap()(api.context), c::OK);
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
        assert_eq!(api.device_close.unwrap()(api.context), c::OK);
        let calls = state.borrow().calls.len();
        assert_eq!(api.buffer_unmap.unwrap()(api.context, info.id), c::OK);
        assert_eq!(api.buffer_destroy.unwrap()(api.context, info.id), c::OK);
        assert_eq!(state.borrow().calls.len(), calls);
        assert_eq!(state.borrow().unmaps, 1);
        assert_eq!(state.borrow().closed, [101]);
    });
    assert_eq!(state.borrow().unmaps, 1);
    assert_eq!(buffers.live_count(), 0);
}
