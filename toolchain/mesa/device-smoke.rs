//j745
//j746
//j751
//! Real C runtime/Mesa object lifecycle over the Session codec and real kernel VMO grants.
#![no_std]
extern crate alloc;
use alloc::{rc::Rc, vec, vec::Vec};
use core::{
    alloc::{GlobalAlloc, Layout},
    cell::RefCell,
    ffi::c_void,
};
use kumo_abi::{sys::Syscall, Handle, Rights};
use kumo_gpu::{
    bo::Buffers,
    c::{with_device_api, Api},
    client::{Client, SessionTransport, Transport, TransportError},
    session::{Backend, Session, SessionRelease, SubmitError},
    BindFlags, Binding, Command, DeviceCore, DeviceInfo, DeviceMemory, DeviceMmu, DeviceTexture,
    DeviceThreads, DeviceTiler, DeviceTopology, Reply, Request, Response, Status, Submission,
    PAGE_BYTES, REPLY_LEN, REQUEST_LEN,
};
mod native;
struct LibcAllocator;
extern "C" {
    fn posix_memalign(out: *mut *mut c_void, alignment: usize, size: usize) -> i32;
    fn free(p: *mut c_void);
    fn kumo_mesa_device_cases(api: *const Api, mode: u32) -> u32;
}
unsafe impl GlobalAlloc for LibcAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut p = core::ptr::null_mut();
        if posix_memalign(
            &mut p,
            layout.align().max(core::mem::size_of::<usize>()),
            layout.size(),
        ) != 0
        {
            return core::ptr::null_mut();
        }
        p.cast()
    }
    unsafe fn dealloc(&self, p: *mut u8, _: Layout) {
        free(p.cast());
    }
}
#[global_allocator]
static ALLOCATOR: LibcAllocator = LibcAllocator;
static CURRENT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static CONTROL: core::sync::atomic::AtomicPtr<RefCell<Server>> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
extern "C" {
    fn printf(format: *const u8, ...) -> i32;
    fn abort() -> !;
}
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    unsafe {
        printf(
            b"G7o-d native device FAILED Rust mode=%u line=%u\n\0".as_ptr(),
            CURRENT.load(core::sync::atomic::Ordering::Relaxed),
            info.location().map_or(0, |p| p.line()),
        );
        abort()
    }
}
fn handle(raw: u64) -> Handle {
    assert!(raw != 0 && raw <= u32::MAX as u64);
    Handle(raw as u32)
}
fn close(vmo: Handle) {
    assert_eq!(
        native::call(Syscall::HandleClose, [vmo.0 as u64, 0, 0, 0, 0, 0]).0,
        0
    );
}

struct Hardware {
    mode: u32,
    buffers: Vec<(u32, Handle)>,
    binds: Vec<Binding>,
    bind_calls: u32,
    unbind_calls: u32,
    reject_submit: bool,
}
impl Backend for Hardware {
    fn info(&mut self) -> Result<DeviceInfo, Status> {
        Ok(DeviceInfo {
            gpu_id: 0xa8670005,
            cs_features: 0x7085f,
        })
    }
    fn topology(&mut self) -> Result<DeviceTopology, Status> {
        Ok(DeviceTopology {
            shader_present: 0xf,
        })
    }
    fn mmu_features(&mut self) -> Result<DeviceMmu, Status> {
        Ok(DeviceMmu {
            mmu_features: 0x2830,
        })
    }
    fn tiler_features(&mut self) -> Result<DeviceTiler, Status> {
        if self.mode == 1 {
            Err(Status::NotSupported)
        } else {
            Ok(DeviceTiler {
                tiler_features: 0x809,
            })
        }
    }
    fn texture_features(&mut self) -> Result<DeviceTexture, Status> {
        Ok(DeviceTexture {
            texture_features: [0xc1ffff9e, 0, 0, 0],
        })
    }
    fn core_features(&mut self) -> Result<DeviceCore, Status> {
        Ok(DeviceCore { core_features: 0 })
    }
    fn thread_properties(&mut self) -> Result<DeviceThreads, Status> {
        Ok(DeviceThreads {
            max_threads: 1024,
            max_workgroup_size: 1024,
            thread_features: 0x03100005,
        })
    }
    fn memory_properties(&mut self) -> Result<DeviceMemory, Status> {
        if self.mode == 5 {
            Err(Status::NotSupported)
        } else {
            Ok(DeviceMemory {
                l2_features: 0x07120306,
                mem_features: 0x301,
                coherency_features: 0,
                coherency_protocol: 31,
            })
        }
    }
    fn create_vm(&mut self) -> Result<(), Status> {
        Ok(())
    }
    fn create_buffer(&mut self, id: u32, bytes: u64) -> Result<(), Status> {
        let mut pages = vec![0; (bytes / PAGE_BYTES) as usize];
        let vmo = handle(
            native::call(
                Syscall::VmoCreatePaged,
                [
                    bytes,
                    pages.as_mut_ptr() as u64,
                    pages.len() as u64,
                    0,
                    0,
                    0,
                ],
            )
            .0,
        );
        self.buffers.push((id, vmo));
        Ok(())
    }
    fn destroy_buffer(&mut self, id: u32, bindings: &[Binding]) -> Result<(), Status> {
        if self.mode == 3 {
            return Err(Status::NoMemory);
        }
        assert_eq!(
            bindings.len(),
            self.binds.iter().filter(|b| b.buffer == id).count()
        );
        self.binds.retain(|b| b.buffer != id);
        let index = self
            .buffers
            .iter()
            .position(|record| record.0 == id)
            .ok_or(Status::NotFound)?;
        close(self.buffers.swap_remove(index).1);
        Ok(())
    }
    fn bind(&mut self, binding: Binding) -> Result<(), Status> {
        self.bind_calls += 1;
        if self.mode == 2 && self.bind_calls == 2 {
            return Err(Status::NoMemory);
        }
        // Mode 8 binds the drivers' executable pools (buffers 2 and 3, cached) beside a
        // data BO; it is the G7o-e W^X shape. — TIMBERDOODLE
        let flags = match (self.mode, binding.buffer) {
            (9.., _) => BindFlags::READ_ONLY,
            (8, 2 | 3) => BindFlags::READ_ONLY,
            (8, _) => BindFlags::NO_EXECUTE,
            (_, 2) => BindFlags::READ_ONLY | BindFlags::UNCACHED,
            _ => BindFlags::NO_EXECUTE,
        };
        assert_eq!(binding.flags, BindFlags(flags));
        self.binds.push(binding);
        Ok(())
    }
    fn unbind(&mut self, bindings: &[Binding]) -> Result<(), Status> {
        self.unbind_calls += 1;
        if self.mode == 4 {
            return Err(Status::NoMemory);
        }
        for binding in bindings {
            assert!(self.binds.contains(binding));
        }
        self.binds.retain(|b| !bindings.contains(b));
        Ok(())
    }
    fn create_group(&mut self) -> Result<(), Status> {
        if self.mode >= 9 {
            Ok(())
        } else {
            Err(Status::NotSupported)
        }
    }
    fn submit(&mut self, _: Submission) -> Result<(), SubmitError> {
        if self.mode < 9 {
            return Err(SubmitError::Unpublished(Status::NotSupported));
        }
        if core::mem::take(&mut self.reject_submit) {
            Err(SubmitError::Unpublished(Status::NoMemory))
        } else {
            Ok(())
        }
    }
    fn retire(&mut self) {
        for (_, vmo) in self.buffers.drain(..) {
            close(vmo);
        }
        self.binds.clear();
    }
}
struct Server {
    session: Session,
    hardware: Hardware,
    calls: Vec<Command>,
    closed: u32,
    grants_closed: u32,
    release: SessionRelease,
    wakes: Vec<Handle>,
    wake_reads: Vec<Handle>,
    wakes_closed: u32,
    last_request: u32,
    last_point: u64,
    uncertain_submit: bool,
}
struct Wire {
    server: Rc<RefCell<Server>>,
    reply: Option<Reply>,
    grant: Option<Handle>,
}
impl Transport for Wire {
    type Handle = Handle;
    fn send(&mut self, bytes: &[u8; REQUEST_LEN]) -> Result<(), TransportError> {
        let request = Request::decode(bytes).unwrap();
        let mut server = self.server.borrow_mut();
        assert_eq!(server.closed, 0);
        assert_eq!(request.id, server.last_request + 1);
        server.last_request = request.id;
        server.calls.push(request.command);
        let Server {
            session, hardware, ..
        } = &mut *server;
        let result = session.dispatch(request, hardware);
        if let Ok(Response::BufferCreated { buffer, .. }) = result {
            let vmo = hardware.buffers.iter().find(|r| r.0 == buffer).unwrap().1;
            self.grant = Some(handle(
                native::call(
                    Syscall::HandleDuplicate,
                    [
                        vmo.0 as u64,
                        (Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER).0
                            as u64,
                        0,
                        0,
                        0,
                        0,
                    ],
                )
                .0,
            ));
        }
        if let Ok(Response::Submitted { point }) = result {
            server.last_point = point;
            if core::mem::take(&mut server.uncertain_submit) {
                return Err(TransportError::Failed);
            }
        }
        if let Ok(Response::WaitArmed { .. }) = result {
            let (read, write) = native::call(Syscall::ChannelCreate, [0; 6]);
            self.grant = Some(handle(read));
            server.wakes.push(handle(write));
            server.wake_reads.push(handle(read));
        }
        self.reply = Some(Reply {
            id: request.id,
            result,
        });
        Ok(())
    }
    fn receive(
        &mut self,
        bytes: &mut [u8; REPLY_LEN + 1],
    ) -> Result<(usize, Option<Handle>), TransportError> {
        bytes[..REPLY_LEN].copy_from_slice(&self.reply.take().unwrap().encode());
        Ok((REPLY_LEN, self.grant.take()))
    }
    fn close_handle(&mut self, vmo: Handle) {
        close(vmo);
        let mut server = self.server.borrow_mut();
        if server.wake_reads.contains(&vmo) {
            server.wakes_closed += 1;
        } else {
            server.grants_closed += 1;
        }
    }
}
impl SessionTransport for Wire {
    fn close_session(&mut self) {
        let mut server = self.server.borrow_mut();
        assert_eq!(server.closed, 0);
        server.closed += 1;
        // No GPU exists in this modeled backend. This models the reset-proved branch,
        // never a claim that a client-side close proves real hardware quiescence. — KESTREL
        server.release = server.session.close();
        server.hardware.retire();
        for write in server.wakes.drain(..) {
            close(write);
        }
    }
}

/// Only the C proof's modeled producer calls this between native API invocations.
/// The Rc remains live for the mode; no callback reentry or GPU authority is added.
/// — MILVUS
#[no_mangle]
pub extern "C" fn kumo_mesa_native_control(action: u32) -> u32 {
    let pointer = CONTROL.load(core::sync::atomic::Ordering::Relaxed);
    assert!(!pointer.is_null());
    let state = unsafe { &*pointer };
    let writes = {
        let mut server = state.borrow_mut();
        match action {
            1 | 2 => {
                let point = server.last_point;
                server
                    .session
                    .complete(point, if action == 1 { 0 } else { 0xbeef })
                    .unwrap();
                core::mem::take(&mut server.wakes)
            }
            3 => {
                server.hardware.reject_submit = true;
                Vec::new()
            }
            4 => {
                server.uncertain_submit = true;
                Vec::new()
            }
            5 => server.wakes.clone(),
            6 => core::mem::take(&mut server.wakes),
            7 => return u32::from(server.closed != 1),
            _ => return 1,
        }
    };
    for write in writes {
        if action == 1 || action == 5 {
            // Abandoned read ends are expected; live waiters verify their own wake.
            // — MILVUS
            native::call(
                Syscall::ChannelWrite,
                [write.0 as u64, [1u8].as_ptr() as u64, 1, 0, 0, 0],
            );
        }
        if action != 5 {
            close(write);
        }
    }
    0
}
fn endpoint_proof() {
    let (left, right) = native::call(Syscall::ChannelCreate, [0; 6]);
    let mut channel = native::Channel::new(handle(left));
    let right = handle(right);
    let request = Request {
        id: 1,
        command: Command::GetInfo,
    }
    .encode();
    channel.send(&request).unwrap();
    let mut bytes = [0; REQUEST_LEN];
    assert_eq!(
        native::call(
            Syscall::ChannelRead,
            [
                right.0 as u64,
                bytes.as_mut_ptr() as u64,
                bytes.len() as u64,
                0,
                0,
                0
            ]
        )
        .0,
        REQUEST_LEN as u64
    );
    assert_eq!(bytes, request);
    let reply = Reply {
        id: 1,
        result: Err(Status::Busy),
    }
    .encode();
    assert_eq!(
        native::call(
            Syscall::ChannelWrite,
            [
                right.0 as u64,
                reply.as_ptr() as u64,
                reply.len() as u64,
                0,
                0,
                0
            ]
        )
        .0,
        0
    );
    let mut frame = [0; REPLY_LEN + 1];
    let (len, grant) = channel.receive(&mut frame).unwrap();
    assert_eq!(len, REPLY_LEN);
    assert!(grant.is_none());
    channel.close_session();
    channel.close_session();
    drop(channel);
    assert_eq!(
        native::call(
            Syscall::ChannelRead,
            [
                right.0 as u64,
                bytes.as_mut_ptr() as u64,
                bytes.len() as u64,
                0,
                0,
                0
            ]
        )
        .0,
        0
    );
    close(right);
}
#[no_mangle]
pub extern "C" fn kumo_mesa_device_proof() -> u32 {
    endpoint_proof();
    for mode in 0..12 {
        CURRENT.store(mode, core::sync::atomic::Ordering::Relaxed);
        let server = Rc::new(RefCell::new(Server {
            session: Session::default(),
            hardware: Hardware {
                mode,
                buffers: Vec::new(),
                binds: Vec::new(),
                bind_calls: 0,
                unbind_calls: 0,
                reject_submit: false,
            },
            calls: Vec::new(),
            closed: 0,
            grants_closed: 0,
            release: SessionRelease::NONE,
            wakes: Vec::new(),
            wake_reads: Vec::new(),
            wakes_closed: 0,
            last_request: 0,
            last_point: 0,
            uncertain_submit: false,
        }));
        let mut client = Client::new(Wire {
            server: server.clone(),
            reply: None,
            grant: None,
        });
        let mut memory = native::Memory;
        let mut buffers = Buffers::new(&mut client, &mut memory);
        CONTROL.store(
            Rc::as_ptr(&server).cast_mut(),
            core::sync::atomic::Ordering::Relaxed,
        );
        let result = with_device_api(&mut buffers, |api| unsafe {
            kumo_mesa_device_cases(api, mode)
        });
        CONTROL.store(core::ptr::null_mut(), core::sync::atomic::Ordering::Relaxed);
        if result != 0 {
            return mode * 100 + result;
        }
        assert_eq!(buffers.live_count(), 0);
        let server = server.borrow();
        assert_eq!(server.closed, 1);
        assert!(server.hardware.buffers.is_empty());
        assert!(server.hardware.binds.is_empty());
        let created = server
            .calls
            .iter()
            .filter(|c| matches!(c, Command::BufferCreate { .. }))
            .count();
        assert_eq!(server.grants_closed as usize, created);
        assert_eq!(server.wakes_closed as usize, server.wake_reads.len());
        assert!(server.wakes.is_empty());
        if mode >= 9 {
            assert!(!server
                .calls
                .iter()
                .any(|c| matches!(c, Command::SyncRead { .. })));
            let submitted = server
                .calls
                .iter()
                .filter(|c| matches!(c, Command::Submit(_)))
                .count();
            assert_eq!(submitted, if mode == 9 { 3 } else { 1 });
            if mode == 9 {
                assert_eq!(server.release.buffers, 0);
                assert_eq!(server.release.bindings, 0);
            } else {
                assert_eq!(server.release.buffers, 1);
                assert_eq!(server.release.bindings, 1);
            }
        }
        let vm_creates = server
            .calls
            .iter()
            .filter(|c| **c == Command::VmCreate)
            .count();
        assert_eq!(vm_creates, usize::from(!matches!(mode, 1 | 5 | 6)));
        if mode == 0 {
            assert_eq!(server.release, SessionRelease::NONE);
            assert_eq!(server.hardware.bind_calls, 3);
        }
        if mode == 2 {
            assert_eq!(server.release.buffers, 2);
            assert_eq!(server.release.bindings, 1);
        }
        if mode == 8 {
            // The unmasked EXECUTABLE | WB_MMAP request never reached the wire.
            assert_eq!(created, 3);
            assert_eq!(server.hardware.bind_calls, 3);
        }
        if matches!(mode, 4 | 7) {
            assert_eq!(server.release.buffers, 2);
            assert_eq!(server.release.bindings, 2);
        }
    }
    0
}
