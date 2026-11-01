//j744
use super::*;
use crate::{
    c, client::TransportError, Command, Reply, Request, Response, Status, REPLY_LEN, REQUEST_LEN,
};
use alloc::rc::Rc;
use core::cell::RefCell;

#[derive(Default)]
struct State {
    calls: Vec<Command>,
    closed: Vec<u32>,
    maps: u32,
    unmaps: u32,
    next: u32,
    fail: Option<Status>,
    map_fail: bool,
    unmap_fail: bool,
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
        let result = if let Some(error) = state.fail.take() {
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
            _ => None,
        };
        Ok((REPLY_LEN, handle))
    }
    fn close_handle(&mut self, handle: u32) {
        self.state.borrow_mut().closed.push(handle);
    }
}
struct Memory(Rc<RefCell<State>>);
impl Mapper<u32> for Memory {
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
