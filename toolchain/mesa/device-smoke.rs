//j772
//j773
//j775
//j776
//j777
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
    SyncState, PAGE_BYTES, REPLY_LEN, REQUEST_LEN,
};
mod native;
struct LibcAllocator;
extern "C" {
    fn posix_memalign(out: *mut *mut c_void, alignment: usize, size: usize) -> i32;
    fn free(p: *mut c_void);
    fn kumo_mesa_device_cases(api: *const Api, mode: u32) -> u32;
    fn kumo_mesa_device_modes() -> u32;
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
    status_vmo: Option<Handle>,
    report: kumo_gpu::GroupReport,
    /// G7q-c: live modeled heaps. — TIMBERDOODLE
    heaps: Vec<u32>,
}
impl Backend for Hardware {
    //j757
    fn create_heap(
        &mut self,
        id: u32,
        spec: kumo_gpu::HeapSpec,
    ) -> Result<kumo_gpu::HeapPlacement, Status> {
        if self.mode != 15 && self.mode < 19 {
            return Err(Status::NotSupported);
        }
        self.heaps.push(id);
        // The real driver's layout: the context page, then the chunks, the last one the head.
        let context_va = kumo_gpu::HEAP_VA_START + u64::from(id) * 0x100_0000;
        Ok(kumo_gpu::HeapPlacement {
            context_va,
            first_chunk_va: context_va
                + PAGE_BYTES
                + u64::from(spec.chunk_bytes) * u64::from(spec.initial_chunks - 1),
        })
    }
    fn destroy_heap(&mut self, id: u32) -> Result<(), Status> {
        let before = self.heaps.len();
        self.heaps.retain(|heap| *heap != id);
        if self.heaps.len() == before {
            return Err(Status::NotFound);
        }
        Ok(())
    }
    fn watch_group(&mut self) -> Result<kumo_gpu::GroupReport, Status> {
        if self.status_vmo.is_none() {
            self.status_vmo = Some(handle(
                native::call(Syscall::VmoCreate, [PAGE_BYTES, 0, 0, 0, 0, 0]).0,
            ));
            self.publish_status(self.report);
        }
        Ok(self.report)
    }
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
        // VmoCreatePaged writes u64 physical addresses. The raw pointer cast must not
        // leave this buffer inferred as Vec<i32>. — KESTREL
        let mut pages = vec![0u64; (bytes / PAGE_BYTES) as usize];
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
            // Mode 16 maps data, code and a large BO wherever AUTO_VA put them. — TIMBERDOODLE
            (16.., _) => binding.flags.0,
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
    fn submit(&mut self, job: Submission) -> Result<(), SubmitError> {
        if self.mode < 9 {
            return Err(SubmitError::Unpublished(Status::NotSupported));
        }
        if self.mode == 19 {
            // Inspect Gallium's actual init BO, using Mesa 26.2.5 v10.xml's encodings.
            // This checks the native heap address and a live handler binding; it models
            // acceptance/completion rather than executing GPU instructions. — KESTREL
            assert_eq!(job.stream_bytes, 40);
            let binding = self.binds.iter().find(|b| b.va == job.stream_va).unwrap();
            let vmo = self
                .buffers
                .iter()
                .find(|b| b.0 == binding.buffer)
                .unwrap()
                .1;
            let mut words = [0u64; 5];
            assert_eq!(
                native::call(
                    Syscall::VmoRead,
                    [
                        vmo.0 as u64,
                        binding.offset,
                        words.as_mut_ptr() as u64,
                        40,
                        0,
                        0
                    ]
                )
                .0,
                0
            );
            let context_va = kumo_gpu::HEAP_VA_START + u64::from(self.heaps[0]) * 0x100_0000;
            assert_eq!(words[0], (1u64 << 56) | (72u64 << 48) | context_va);
            assert_eq!(words[1], (48u64 << 56) | (72u64 << 40));
            assert_eq!(words[2] >> 48, (1u64 << 8) | 86);
            assert_eq!(words[3] >> 32, (2u64 << 24) | (88u64 << 16));
            assert_eq!(words[4], (25u64 << 56) | (86u64 << 40) | (88u64 << 32) | 2);
            let handler = words[2] & 0xffff_ffff_ffff;
            let length = words[3] & 0xffff_ffff;
            assert!(length > 0 && length % 8 == 0);
            assert!(self.binds.iter().any(|b| b.va <= handler
                && handler + length <= b.va + b.bytes
                && b.flags.0 & BindFlags::NO_EXECUTE != 0));
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
        if let Some(vmo) = self.status_vmo.take() {
            close(vmo);
        }
    }
}
impl Hardware {
    fn publish_status(&mut self, report: kumo_gpu::GroupReport) {
        self.report = report;
        let bytes = report.encode(1);
        assert_eq!(
            native::call(
                Syscall::VmoWrite,
                [
                    self.status_vmo.unwrap().0 as u64,
                    0,
                    bytes.as_ptr() as u64,
                    bytes.len() as u64,
                    0,
                    0
                ]
            )
            .0,
            0
        );
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
    status_reads: Vec<Handle>,
    status_grants_closed: u32,
    peer_dead: bool,
    denied_sends: u32,
    /// GL1-d: while set, an armed wait does not complete the last point. — TIMBERDOODLE
    hold: bool,
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
        if server.peer_dead {
            server.denied_sends += 1;
            return Err(TransportError::Closed);
        }
        assert_eq!(server.closed, 0);
        assert_eq!(request.id, server.last_request + 1);
        server.last_request = request.id;
        // RE4: mode 28 churns textures with no CPU wait between its upload, draw and readback
        // batches. drv-gpu runs one point at a time, so the next Submit waits on the previous
        // point, and this producer completes only an ARMed point; Mali finishes it alone.
        // Model that in-order completion: the previous point completes before the next Submit
        // is dispatched, when no armed wait is pending. — TIMBERDOODLE
        if server.hardware.mode == 28
            && matches!(request.command, Command::Submit(_))
            && server.last_point != 0
            && server.wakes.is_empty()
            && !matches!(
                server.session.fence_state(server.last_point),
                Ok(SyncState::Complete { .. })
            )
        {
            let point = server.last_point;
            server.session.complete(point, 0).unwrap();
        }
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
        if let Ok(Response::GroupWatch { .. }) = result {
            let owner = hardware.status_vmo.unwrap();
            let grant = handle(
                native::call(
                    Syscall::HandleDuplicate,
                    [
                        owner.0 as u64,
                        (Rights::READ | Rights::DUPLICATE | Rights::TRANSFER).0 as u64,
                        0,
                        0,
                        0,
                        0,
                    ],
                )
                .0,
            );
            let bytes = [0x42u8];
            assert_ne!(
                native::call(
                    Syscall::VmoWrite,
                    [grant.0 as u64, 0, bytes.as_ptr() as u64, 1, 0, 0]
                )
                .0,
                0
            );
            assert_ne!(
                native::call(
                    Syscall::VmarMap,
                    [
                        0,
                        grant.0 as u64,
                        0,
                        0,
                        PAGE_BYTES,
                        (kumo_abi::VmarFlags::READ | kumo_abi::VmarFlags::WRITE).0
                    ]
                )
                .0,
                0
            );
            self.grant = Some(grant);
            server.status_reads.push(grant);
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
        let complete = (matches!(
            server.hardware.mode,
            19 | 21 | 22 | 23 | 24 | 25 | 26 | 27 | 28
        ) || server.hardware.mode == 20 && !server.hold)
            && matches!(result, Ok(Response::WaitArmed { .. }));
        drop(server);
        if complete {
            // The modeled producer completes only after ARM established a real wake channel.
            // Drop the server borrow before the control path re-enters it. — KESTREL
            assert_eq!(kumo_mesa_native_control(1), 0);
        }
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
        } else if server.status_reads.contains(&vmo) {
            server.status_grants_closed += 1;
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
                if action == 2 {
                    server.hardware.publish_status(kumo_gpu::GroupReport {
                        state: kumo_gpu::GroupState::Fatal,
                        gpu_status: 0xbeef,
                    });
                }
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
            8 | 9 => {
                let report = drv_gpu::classify_group_failure(
                    drv_gpu::JobWait::Stalled,
                    0,
                    0,
                    0,
                    action == 9,
                );
                server.hardware.publish_status(report);
                server.peer_dead = true;
                Vec::new()
            }
            10 => {
                server.peer_dead = true;
                Vec::new()
            }
            11 => return u32::from(server.denied_sends != 1),
            12 | 13 => {
                server.hold = action == 12;
                return 0;
            }
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

fn closed_status_cap_proof() {
    use kumo_gpu::{bo::Mapper, GroupReport, GroupState};
    for published in [
        GroupReport::TIMED_OUT,
        GroupReport {
            state: GroupState::Fatal,
            gpu_status: 0,
        },
        GroupReport::USABLE,
    ] {
        let owner = handle(native::call(Syscall::VmoCreate, [PAGE_BYTES, 0, 0, 0, 0, 0]).0);
        let initial = GroupReport::USABLE.encode(1);
        assert_eq!(
            native::call(
                Syscall::VmoWrite,
                [
                    owner.0 as u64,
                    0,
                    initial.as_ptr() as u64,
                    initial.len() as u64,
                    0,
                    0
                ]
            )
            .0,
            0
        );
        let reader = handle(
            native::call(
                Syscall::HandleDuplicate,
                [
                    owner.0 as u64,
                    (Rights::READ | Rights::TRANSFER).0 as u64,
                    0,
                    0,
                    0,
                    0,
                ],
            )
            .0,
        );
        let (left, right) = native::call(Syscall::ChannelCreate, [0; 6]);
        let right = handle(right);
        let reply = Reply {
            id: 1,
            result: Ok(Response::GroupWatch {
                group: 1,
                report: GroupReport::USABLE,
            }),
        }
        .encode();
        assert_eq!(
            native::call(
                Syscall::ChannelWrite,
                [
                    right.0 as u64,
                    reply.as_ptr() as u64,
                    reply.len() as u64,
                    reader.0 as u64,
                    0,
                    0
                ]
            )
            .0,
            0
        );
        let mut client = Client::new(native::Channel::new(handle(left)));
        let watch = client.watch_group(1).unwrap();
        let bytes = published.encode(1);
        assert_eq!(
            native::call(
                Syscall::VmoWrite,
                [
                    owner.0 as u64,
                    0,
                    bytes.as_ptr() as u64,
                    bytes.len() as u64,
                    0,
                    0
                ]
            )
            .0,
            0
        );
        // Both the real command peer and the only writer close. The transferred READ
        // grant alone retains the record, without writable authority. — MILVUS
        close(right);
        close(owner);
        assert!(client.info().is_err());
        assert!(client.is_lost());
        assert_ne!(
            native::call(
                Syscall::VmoWrite,
                [
                    watch.handle.0 as u64,
                    0,
                    bytes.as_ptr() as u64,
                    bytes.len() as u64,
                    0,
                    0
                ]
            )
            .0,
            0
        );
        let mut read = [0; kumo_gpu::group::REPORT_BYTES];
        native::Memory.read(&watch.handle, &mut read).unwrap();
        assert_eq!(
            GroupReport::decode(&read, 1)
                .unwrap()
                .observed(!client.is_lost()),
            published.observed(false)
        );
        client.close_buffer_handle(watch.handle);
        client.close_session();
    }
}
#[no_mangle]
pub extern "C" fn kumo_mesa_device_proof() -> u32 {
    endpoint_proof();
    closed_status_cap_proof();
    // The GL build adds the screen and context-sync ownership proofs. — KESTREL
    for mode in 0..unsafe { kumo_mesa_device_modes() } {
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
                status_vmo: None,
                report: kumo_gpu::GroupReport::USABLE,
                heaps: Vec::new(),
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
            status_reads: Vec::new(),
            status_grants_closed: 0,
            peer_dead: false,
            denied_sends: 0,
            hold: false,
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
        assert_eq!(
            server.status_grants_closed as usize,
            server.status_reads.len()
        );
        assert!(server.hardware.status_vmo.is_none());
        assert!(server.wakes.is_empty());
        if mode == 18 {
            assert_eq!(server.release, SessionRelease::NONE);
            assert!(created > 1);
            assert!(!server.calls.iter().any(|c| matches!(
                c,
                Command::GroupCreate { .. } | Command::Submit(_) | Command::SyncRead { .. }
            )));
        }
        if mode == 19 {
            unsafe {
                printf(
                    b"GL1-c model release: buffers=%u bindings=%u waits=%u heaps=%u bytes=%llu\n\0"
                        .as_ptr(),
                    server.release.buffers,
                    server.release.bindings,
                    server.release.waits,
                    server.release.heaps,
                    server.release.bytes,
                );
            }
            assert_eq!(server.release.buffers, 0);
            assert_eq!(server.release.bindings, 0);
            assert_eq!(server.release.heaps, 0);
            assert_eq!(server.release.bytes, 0);
            // The completed wait's table row is pruned on the next wire ARM or close.
            // Its endpoint is already consumed; the post-wake ARM used cached completion.
            // — KESTREL
            assert_eq!(server.release.waits, 1);
            assert!(server.hardware.heaps.is_empty());
            let count = |f: fn(&Command) -> bool| server.calls.iter().filter(|c| f(c)).count();
            assert_eq!(count(|c| matches!(c, Command::GroupCreate { .. })), 1);
            assert_eq!(count(|c| matches!(c, Command::HeapCreate(_))), 1);
            assert_eq!(count(|c| matches!(c, Command::HeapDestroy { .. })), 1);
            assert_eq!(count(|c| matches!(c, Command::Submit(_))), 1);
            assert_eq!(count(|c| matches!(c, Command::SyncRead { .. })), 0);
            assert_eq!(server.last_point, 1);
            assert_eq!(server.wake_reads.len(), 1);
        }
        if (9..15).contains(&mode) {
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
        if mode == 16 {
            // Six maps crossed (three, the hole refilled, two fills); the caller-chosen and the
            // window-full maps never did. Every address is page-aligned inside the window, and the
            // only explicit unbind is the code page's. — TIMBERDOODLE
            // GL1-d adds a deferred unbind of the refilled page and a seventh map that reuses it.
            // — TIMBERDOODLE
            let vas: alloc::vec::Vec<u64> = server
                .calls
                .iter()
                .filter_map(|c| match c {
                    Command::VmBind(b) => Some(b.va),
                    _ => None,
                })
                .collect();
            assert_eq!(vas.len(), 7);
            assert_eq!(vas[3], vas[1]);
            assert_eq!(vas[4], vas[1]);
            assert_eq!(server.hardware.bind_calls, 7);
            assert_eq!(server.hardware.unbind_calls, 2);
            assert!(vas
                .iter()
                .all(|va| va % PAGE_BYTES == 0
                    && (kumo_gpu::VA_START..kumo_gpu::VA_END).contains(va)));
        }
        if matches!(mode, 17 | 19 | 20 | 21 | 22 | 23 | 24 | 25 | 26 | 27 | 28) {
            // GL1-d: Gallium frees every BO with a deferred unmap that now crosses, so each
            // binding it made is explicitly unbound before its buffer goes. — TIMBERDOODLE
            let count = |f: fn(&Command) -> bool| server.calls.iter().filter(|c| f(c)).count();
            let binds = count(|c| matches!(c, Command::VmBind(_)));
            assert!(binds > 0);
            assert_eq!(count(|c| matches!(c, Command::VmUnbind(_))), binds);
        }
        if matches!(mode, 21 | 22) {
            // The frontend's GLES2 constructor submits the real init job. Merely making
            // current and flushing an untouched drawable adds no rendering job. — KESTREL
            let count = |f: fn(&Command) -> bool| server.calls.iter().filter(|c| f(c)).count();
            let label = if mode == 21 { b"GL1-e\0" } else { b"GL1-f\0" };
            unsafe {
                printf(b"%s model release: buffers=%u bindings=%u waits=%u heaps=%u bytes=%llu submits=%u\n\0".as_ptr(),
                       label.as_ptr(), server.release.buffers, server.release.bindings, server.release.waits,
                       server.release.heaps, server.release.bytes,
                       count(|c| matches!(c, Command::Submit(_))) as u32);
            }
            assert_eq!(server.release.buffers, 0);
            assert_eq!(server.release.bindings, 0);
            assert_eq!(server.release.waits, 1);
            assert_eq!(server.release.heaps, 0);
            assert_eq!(server.release.bytes, 0);
            assert_eq!(count(|c| matches!(c, Command::GroupCreate { .. })), 1);
            assert_eq!(count(|c| matches!(c, Command::HeapCreate(_))), 1);
            assert_eq!(count(|c| matches!(c, Command::HeapDestroy { .. })), 1);
            assert_eq!(count(|c| matches!(c, Command::Submit(_))), 1);
            assert_eq!(count(|c| matches!(c, Command::SyncRead { .. })), 0);
            assert_eq!(server.last_point, 1);
            assert_eq!(server.wake_reads.len(), 1);
        }
        if matches!(mode, 23..=27) {
            // Real GL dispatch; the producer signals completion without executing a GPU
            // or altering the framebuffer/SSBO contents. — KESTREL
            // GL3-a's triangle shares the shape; its Submit count is measured. — TIMBERDOODLE
            // GL3-c's instanced draw is predicted to share GL3-a's three Submits (init, the
            // clear and draw batch, the AFBC readback blit), and GL4-a's texture clear GL2-b's
            // three; the first QEMU run confirms both.
            let count = |f: fn(&Command) -> bool| server.calls.iter().filter(|c| f(c)).count();
            let label = match mode {
                23 => b"GL2-a\0",
                24 => b"GL2-b\0",
                25 => b"GL3-a\0",
                26 => b"GL3-c\0",
                _ => b"GL4-a\0",
            };
            unsafe {
                printf(b"%s model release: buffers=%u bindings=%u waits=%u heaps=%u bytes=%llu submits=%u\n\0".as_ptr(),
                       label.as_ptr(), server.release.buffers, server.release.bindings, server.release.waits,
                       server.release.heaps, server.release.bytes,
                       count(|c| matches!(c, Command::Submit(_))) as u32);
            }
            assert_eq!(server.release.buffers, 0);
            assert_eq!(server.release.bindings, 0);
            assert_eq!(server.release.waits, 1);
            assert_eq!(server.release.heaps, 0);
            assert_eq!(server.release.bytes, 0);
            assert_eq!(count(|c| matches!(c, Command::GroupCreate { .. })), 1);
            assert_eq!(count(|c| matches!(c, Command::HeapCreate(_))), 1);
            assert_eq!(count(|c| matches!(c, Command::HeapDestroy { .. })), 1);
            let submits = if mode == 23 { 2 } else { 3 };
            assert_eq!(count(|c| matches!(c, Command::Submit(_))), submits);
            assert_eq!(count(|c| matches!(c, Command::SyncRead { .. })), 0);
            assert_eq!(server.last_point, submits as u64);
            assert_eq!(server.wake_reads.len(), submits);
        }
        if mode == 28 {
            // GL4: 100 frames of texture churn through the modeled service: every buffer,
            // binding, heap and byte is back. Measured on the first completing run (RE4):
            // 301 Submits (init, then each frame's upload blit, draw and readback blit) and 25
            // BufferCreates, because Gallium's BO cache recycles each frame's freed 160 KiB
            // texture rather than crossing it anew. On metal the census holds the churn flat.
            // — TIMBERDOODLE
            let count = |f: fn(&Command) -> bool| server.calls.iter().filter(|c| f(c)).count();
            let created = count(|c| matches!(c, Command::BufferCreate { .. }));
            let submits = count(|c| matches!(c, Command::Submit(_)));
            unsafe {
                printf(
                    b"GL4 model release: buffers=%u bindings=%u waits=%u heaps=%u bytes=%llu submits=%u creates=%u\n\0"
                        .as_ptr(),
                    server.release.buffers,
                    server.release.bindings,
                    server.release.waits,
                    server.release.heaps,
                    server.release.bytes,
                    submits as u32,
                    created as u32,
                );
            }
            assert_eq!(server.release.buffers, 0);
            assert_eq!(server.release.bindings, 0);
            assert_eq!(server.release.heaps, 0);
            assert_eq!(server.release.bytes, 0);
            assert_eq!(count(|c| matches!(c, Command::GroupCreate { .. })), 1);
            assert_eq!(count(|c| matches!(c, Command::HeapCreate(_))), 1);
            assert_eq!(count(|c| matches!(c, Command::HeapDestroy { .. })), 1);
            assert_eq!(count(|c| matches!(c, Command::SyncRead { .. })), 0);
            assert_eq!((created, submits), (25, 301));
        }
        if mode == 26 {
            // GL3-c: the proof-only driconf overrides, set before screen creation, are the
            // heap Gallium asked the session for: one 256 KiB chunk, at most two, Gallium's
            // own target_in_flight. This is the shape the metal proof depends on.
            let Some(spec) = server.calls.iter().find_map(|c| match c {
                Command::HeapCreate(spec) => Some(*spec),
                _ => None,
            }) else {
                panic!("GL3-c created no heap");
            };
            unsafe {
                printf(
                    b"GL3-c model heap: chunk_bytes=%u initial_chunks=%u max_chunks=%u target_in_flight=%u\n\0"
                        .as_ptr(),
                    spec.chunk_bytes,
                    spec.initial_chunks,
                    spec.max_chunks,
                    spec.target_in_flight,
                );
            }
            assert_eq!(
                (
                    spec.chunk_bytes,
                    spec.initial_chunks,
                    spec.max_chunks,
                    spec.target_in_flight
                ),
                (262_144, 1, 2, 65_535)
            );
        }
        if mode == 20 {
            // The init job took point 1 and the clear's batch point 2. The held poll armed a
            // wake and gave it back, the infinite wait armed another, and the read map's bo_wait
            // and every deferred unmap were answered from observed completion. — TIMBERDOODLE
            let count = |f: fn(&Command) -> bool| server.calls.iter().filter(|c| f(c)).count();
            unsafe {
                printf(
                    b"GL1-d model release: buffers=%u bindings=%u waits=%u heaps=%u bytes=%llu submits=%u wakes=%u\n\0"
                        .as_ptr(),
                    server.release.buffers,
                    server.release.bindings,
                    server.release.waits,
                    server.release.heaps,
                    server.release.bytes,
                    count(|c| matches!(c, Command::Submit(_))) as u32,
                    server.wake_reads.len() as u32,
                );
            }
            assert_eq!(server.release.buffers, 0);
            assert_eq!(server.release.bindings, 0);
            assert_eq!(server.release.heaps, 0);
            assert_eq!(server.release.bytes, 0);
            assert_eq!(count(|c| matches!(c, Command::GroupCreate { .. })), 1);
            assert_eq!(count(|c| matches!(c, Command::HeapCreate(_))), 1);
            assert_eq!(count(|c| matches!(c, Command::HeapDestroy { .. })), 1);
            assert_eq!(count(|c| matches!(c, Command::Submit(_))), 2);
            assert_eq!(count(|c| matches!(c, Command::SyncRead { .. })), 0);
            assert_eq!(server.last_point, 2);
            assert_eq!(server.wake_reads.len(), 3);
        }
        if mode == 15 {
            // Every refused shape stayed local; three heaps crossed and all three were destroyed,
            // two of them by the logical VM's teardown.
            let count = |f: fn(&Command) -> bool| server.calls.iter().filter(|c| f(c)).count();
            assert_eq!(count(|c| matches!(c, Command::HeapCreate(_))), 3);
            assert_eq!(count(|c| matches!(c, Command::HeapDestroy { .. })), 3);
            assert!(server
                .calls
                .iter()
                .all(|c| !matches!(c, Command::HeapCreate(spec) if spec.vm != 1)));
            assert!(server.hardware.heaps.is_empty());
            assert_eq!(server.release.heaps, 0);
        }
    }
    0
}
