//j751
//j752
//! Native C operations. One BO owner retains all sync metadata and wake/status handles.
//! C receives opaque, never-reused IDs; no kernel handle or caller-supplied fence crosses.

use alloc::vec::Vec;
use core::{
    ffi::c_void,
    mem::size_of,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    bo::{Buffers, Mapper, WaitOutcome},
    c,
    client::{Client, FenceStatus, SessionTransport, Transport},
    sync::{self, PreparedSubmit, Snapshot, SubmitProgress, SyncHandle, SyncObjects},
    Submission,
};

pub const CREATE_BINARY: u32 = 1;
pub const CREATE_TIMELINE: u32 = 2;
pub const DROP: u32 = 3;
pub const RESET: u32 = 4;
pub const SIGNAL: u32 = 5;
pub const PENDING: u32 = 6;
pub const SNAPSHOT: u32 = 7;
pub const ARM: u32 = 8;
pub const WAIT: u32 = 9;
pub const GROUP_CREATE: u32 = 10;
pub const PREPARE: u32 = 11;
pub const STEP: u32 = 12;
pub const GROUP_STATE: u32 = 13;

pub const UNAVAILABLE: u32 = 0;
pub const AVAILABLE: u32 = 1;
pub const COMPLETE: u32 = 2;
pub const SUBMITTED: u32 = 3;
pub const WOKEN: u32 = 4;
pub const TIMED_OUT: u32 = 5;
pub const CLOSED: u32 = 6;
pub const UNKNOWN_OBJECT: u32 = 10;
pub const UNSUBMITTED: u32 = 11;
pub const WAIT_FAILED: u32 = 12;
pub const MAX_OBJECTS: usize = 256;
pub const MAX_TIMEOUT_NS: u64 = 5_000_000_000;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct Point {
    pub id: u64,
    pub value: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct Job {
    pub vm: u32,
    pub stream_bytes: u32,
    pub stream_va: u64,
    pub signal_point: u64,
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct Request {
    pub operation: u32,
    pub flags: u32,
    pub id: u64,
    pub value: u64,
    pub job: Job,
    pub waits: *const Point,
    pub signals: *const Point,
    pub wait_count: u32,
    pub signal_count: u32,
}
impl Default for Request {
    fn default() -> Self {
        Self {
            operation: 0,
            flags: 0,
            id: 0,
            value: 0,
            job: Job::default(),
            waits: core::ptr::null(),
            signals: core::ptr::null(),
            wait_count: 0,
            signal_count: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct Reply {
    pub id: u64,
    pub state: u32,
    pub gpu_status: u32,
    pub value: u64,
}

enum Object<H> {
    Sync(SyncHandle),
    Snapshot(Snapshot),
    Group {
        vm: u32,
        group: u32,
        watch: crate::GroupWatch<H>,
        report: crate::GroupReport,
    },
    Job {
        vm: u32,
        submission: Submission,
        prepared: PreparedSubmit,
    },
    Wake(H),
}
struct Record<H> {
    id: u64,
    object: Object<H>,
}

pub(crate) struct Context<H> {
    syncs: Option<sync::State>,
    objects: Vec<Record<H>>,
}
impl<H> Default for Context<H> {
    fn default() -> Self {
        Self {
            syncs: None,
            objects: Vec::new(),
        }
    }
}

enum Fault {
    Sync(sync::Error),
    Bo(crate::bo::Error),
    Unknown,
    Argument,
    Unsupported,
    Wait,
}
impl From<sync::Error> for Fault {
    fn from(e: sync::Error) -> Self {
        Self::Sync(e)
    }
}
impl From<crate::bo::Error> for Fault {
    fn from(e: crate::bo::Error) -> Self {
        Self::Bo(e)
    }
}
impl From<crate::client::Error> for Fault {
    fn from(e: crate::client::Error) -> Self {
        Self::Sync(sync::Error::Client(e))
    }
}

impl<H> Context<H> {
    pub(crate) fn clear<T: Transport<Handle = H>>(&mut self, client: &mut Client<T>) {
        for record in self.objects.drain(..) {
            match record.object {
                Object::Wake(handle) => client.close_buffer_handle(handle),
                Object::Group { watch, .. } => client.close_buffer_handle(watch.handle),
                _ => (),
            }
        }
        self.syncs = None;
    }
    pub(crate) fn capture_group<M: Mapper<H>>(
        &mut self,
        mapper: &mut M,
    ) -> Option<(u64, crate::GroupReport)> {
        for record in &mut self.objects {
            if let Object::Group { watch, report, .. } = &mut record.object {
                let mut bytes = [0; crate::group::REPORT_BYTES];
                let read = mapper
                    .read(&watch.handle, &mut bytes)
                    .ok()
                    .and_then(|()| crate::GroupReport::decode(&bytes, watch.group));
                *report = retain_report(*report, read, false);
                return Some((record.id, *report));
            }
        }
        None
    }
    fn index(&self, id: u64) -> Result<usize, Fault> {
        self.objects
            .iter()
            .position(|r| r.id == id)
            .ok_or(Fault::Unknown)
    }
    fn sync(&self, id: u64) -> Result<SyncHandle, Fault> {
        match self.objects[self.index(id)?].object {
            Object::Sync(handle) => Ok(handle),
            _ => Err(Fault::Unknown),
        }
    }
    fn snapshot(&self, id: u64) -> Result<Snapshot, Fault> {
        match self.objects[self.index(id)?].object {
            Object::Snapshot(snapshot) => Ok(snapshot),
            _ => Err(Fault::Unknown),
        }
    }
    fn reserve(&mut self) -> Result<u64, Fault> {
        if self.objects.len() == MAX_OBJECTS {
            return Err(sync::Error::NoMemory.into());
        }
        self.objects
            .try_reserve(1)
            .map_err(|_| sync::Error::NoMemory)?;
        NEXT_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| sync::Error::NoMemory.into())
    }
    fn insert(&mut self, id: u64, object: Object<H>, state: u32) -> Reply {
        self.objects.push(Record { id, object });
        Reply {
            id,
            state,
            ..Reply::default()
        }
    }
    fn points(&self, raw: &[Point]) -> Result<Vec<sync::Point>, Fault> {
        let mut result = Vec::new();
        result
            .try_reserve(raw.len())
            .map_err(|_| sync::Error::NoMemory)?;
        for p in raw {
            result.push(sync::Point {
                sync: self.sync(p.id)?,
                value: p.value,
            });
        }
        Ok(result)
    }

    fn execute<T: SessionTransport<Handle = H>, M: Mapper<H>>(
        &mut self,
        buffers: &mut Buffers<'_, T, M>,
        request: Request,
        waits: &[Point],
        signals: &[Point],
    ) -> Result<Reply, Fault> {
        match request.operation {
            CREATE_BINARY | CREATE_TIMELINE => {
                let id = self.reserve()?;
                let handle = SyncObjects::with_state(buffers.client, &mut self.syncs, |s| {
                    if request.operation == CREATE_BINARY {
                        s.create_binary(request.value != 0)
                    } else {
                        s.create_timeline(request.value)
                    }
                })?;
                Ok(self.insert(id, Object::Sync(handle), UNAVAILABLE))
            }
            DROP => {
                let index = self.index(request.id)?;
                match self.objects[index].object {
                    Object::Sync(handle) => {
                        SyncObjects::with_state(buffers.client, &mut self.syncs, |s| {
                            s.destroy(handle)
                        })?;
                    }
                    Object::Group { .. } => return Err(Fault::Unsupported),
                    _ => (),
                }
                if let Object::Wake(handle) = self.objects.swap_remove(index).object {
                    buffers.client.close_buffer_handle(handle);
                }
                Ok(Reply::default())
            }
            RESET | SIGNAL | PENDING | SNAPSHOT => {
                let handle = self.sync(request.id)?;
                let point = sync::Point {
                    sync: handle,
                    value: request.value,
                };
                if request.operation == SNAPSHOT {
                    let id = self.reserve()?;
                    let snapshot = SyncObjects::with_state(buffers.client, &mut self.syncs, |s| {
                        s.snapshot(point)
                    })?;
                    return Ok(match snapshot {
                        Some(snapshot) => self.insert(id, Object::Snapshot(snapshot), AVAILABLE),
                        None => Reply::default(),
                    });
                }
                SyncObjects::with_state(buffers.client, &mut self.syncs, |s| {
                    match request.operation {
                        RESET => {
                            s.reset(handle)?;
                            Ok(Reply::default())
                        }
                        SIGNAL => {
                            s.signal(point)?;
                            Ok(Reply::default())
                        }
                        _ => {
                            let available = s.wait_pending(point)?;
                            Ok(Reply {
                                state: if available { AVAILABLE } else { UNAVAILABLE },
                                value: u64::from(available),
                                ..Reply::default()
                            })
                        }
                    }
                })
                .map_err(Into::into)
            }
            ARM => {
                let snapshot = self.snapshot(request.id)?;
                let id = self.reserve()?;
                let status =
                    SyncObjects::with_state(buffers.client, &mut self.syncs, |s| s.arm(snapshot))?;
                Ok(match status {
                    FenceStatus::Done { gpu_status: 0 } => Reply {
                        state: COMPLETE,
                        ..Reply::default()
                    },
                    FenceStatus::Armed(handle) => self.insert(id, Object::Wake(handle), AVAILABLE),
                    _ => unreachable!("arm refuses GPU errors"),
                })
            }
            WAIT => {
                let index = self.index(request.id)?;
                if !matches!(self.objects[index].object, Object::Wake(_)) {
                    return Err(Fault::Unknown);
                }
                let Object::Wake(handle) = self.objects.swap_remove(index).object else {
                    unreachable!()
                };
                let result = buffers.wait_native(&handle, request.value);
                buffers.client.close_buffer_handle(handle);
                let state = match result {
                    WaitOutcome::Woken => WOKEN,
                    WaitOutcome::TimedOut => TIMED_OUT,
                    WaitOutcome::Closed => CLOSED,
                    WaitOutcome::Unsupported => return Err(Fault::Unsupported),
                    WaitOutcome::Failed => return Err(Fault::Wait),
                };
                Ok(Reply {
                    state,
                    ..Reply::default()
                })
            }
            GROUP_CREATE => {
                let vm = request.value as u32;
                buffers.check_native_vm(vm)?;
                if self
                    .objects
                    .iter()
                    .any(|r| matches!(r.object, Object::Group { .. }))
                {
                    return Err(sync::Error::InvalidRequest.into());
                }
                let id = self.reserve()?;
                let group = buffers.client.create_group(1)?;
                if group != 1 {
                    buffers.client.abandon();
                    return Err(crate::client::Error::Protocol.into());
                }
                let watch = match buffers.client.watch_group(group) {
                    Ok(watch) => watch,
                    Err(error) => {
                        buffers.client.abandon();
                        return Err(error.into());
                    }
                };
                let mut bytes = [0; crate::group::REPORT_BYTES];
                let report = buffers
                    .read_native(&watch.handle, &mut bytes)
                    .ok()
                    .and_then(|()| crate::GroupReport::decode(&bytes, group));
                let Some(report) = report else {
                    buffers.client.close_buffer_handle(watch.handle);
                    buffers.client.abandon();
                    return Err(crate::client::Error::Protocol.into());
                };
                let report = retain_report(watch.report, Some(report), true);
                if report.state != crate::GroupState::Usable {
                    buffers.client.abandon();
                }
                let mut reply = self.insert(
                    id,
                    Object::Group {
                        vm,
                        group,
                        watch,
                        report,
                    },
                    UNAVAILABLE,
                );
                reply.value = report.state as u64;
                Ok(reply)
            }
            GROUP_STATE => {
                let report = if let Ok(index) = self.index(request.id) {
                    let Object::Group {
                        vm, watch, report, ..
                    } = &mut self.objects[index].object
                    else {
                        return Err(Fault::Unknown);
                    };
                    let live = buffers.device_ready() && buffers.check_native_vm(*vm).is_ok();
                    let mut bytes = [0; crate::group::REPORT_BYTES];
                    let read = buffers
                        .read_native(&watch.handle, &mut bytes)
                        .ok()
                        .and_then(|()| crate::GroupReport::decode(&bytes, watch.group));
                    *report = retain_report(*report, read, live);
                    *report
                } else {
                    buffers
                        .group_report
                        .filter(|(id, _)| *id == request.id)
                        .map(|(_, report)| report)
                        .ok_or(Fault::Unknown)?
                };
                if report.state != crate::GroupState::Usable {
                    buffers.client.abandon();
                }
                Ok(Reply {
                    state: AVAILABLE,
                    value: report.state as u64,
                    gpu_status: report.gpu_status,
                    ..Reply::default()
                })
            }
            PREPARE => {
                let Object::Group { vm, group, .. } = self.objects[self.index(request.id)?].object
                else {
                    return Err(Fault::Unknown);
                };
                if vm != request.job.vm {
                    return Err(sync::Error::InvalidRequest.into());
                }
                let submission = Submission {
                    group,
                    stream_va: request.job.stream_va,
                    stream_bytes: request.job.stream_bytes,
                    signal_point: request.job.signal_point,
                };
                buffers.check_native_submission(vm, submission)?;
                let waits = self.points(waits)?;
                let signals = self.points(signals)?;
                let id = self.reserve()?;
                let prepared = SyncObjects::with_state(buffers.client, &mut self.syncs, |s| {
                    s.prepare(submission, &waits, &signals)
                })?;
                Ok(self.insert(
                    id,
                    Object::Job {
                        vm,
                        submission,
                        prepared,
                    },
                    UNAVAILABLE,
                ))
            }
            STEP => {
                let index = self.index(request.id)?;
                let Object::Job { vm, submission, .. } = self.objects[index].object else {
                    return Err(Fault::Unknown);
                };
                buffers.check_native_submission(vm, submission)?;
                let id = self.reserve()?;
                // Retain the intent on recoverable refusal. All capacity/ID reservations
                // precede transport I/O, including the returned snapshot or wake. — MILVUS
                let mut record = self.objects.swap_remove(index);
                let Object::Job { prepared, .. } = &mut record.object else {
                    unreachable!()
                };
                let result = SyncObjects::with_state(buffers.client, &mut self.syncs, |s| {
                    s.submit_prepared(prepared)
                });
                self.objects.push(record);
                Ok(match result? {
                    SubmitProgress::Submitted(snapshot) => {
                        self.insert(id, Object::Snapshot(snapshot), SUBMITTED)
                    }
                    SubmitProgress::Blocked { wake, .. } => {
                        self.insert(id, Object::Wake(wake), AVAILABLE)
                    }
                })
            }
            _ => Err(Fault::Argument),
        }
    }
}

fn retain_report(
    previous: crate::GroupReport,
    read: Option<crate::GroupReport>,
    live: bool,
) -> crate::GroupReport {
    // Positive driver facts survive subsequent close/read failure. Lack of evidence can
    // never turn a stale usable record into a healthy closed group. — MILVUS
    if matches!(
        previous.state,
        crate::GroupState::Fatal | crate::GroupState::TimedOut
    ) {
        previous
    } else {
        read.unwrap_or(crate::GroupReport::UNKNOWN).observed(live)
    }
}

fn valid(request: &Request) -> bool {
    if request.flags != 0 {
        return false;
    }
    if request.operation == PREPARE {
        return request.id != 0
            && request.value == 0
            && request.wait_count as usize <= sync::MAX_SUBMIT_WAITS
            && request.signal_count as usize <= sync::MAX_SYNCS;
    }
    if request.job != Job::default()
        || request.wait_count != 0
        || request.signal_count != 0
        || !request.waits.is_null()
        || !request.signals.is_null()
    {
        return false;
    }
    match request.operation {
        CREATE_BINARY => request.id == 0 && request.value <= 1,
        CREATE_TIMELINE => request.id == 0,
        GROUP_CREATE => request.id == 0 && request.value != 0 && request.value <= u32::MAX as u64,
        DROP | RESET | ARM | STEP | GROUP_STATE => request.id != 0 && request.value == 0,
        SIGNAL | PENDING | SNAPSHOT => request.id != 0,
        WAIT => request.id != 0 && request.value != 0 && request.value <= MAX_TIMEOUT_NS,
        _ => false,
    }
}

unsafe fn points<'a>(pointer: *const Point, count: u32) -> Result<&'a [Point], Fault> {
    if count == 0 {
        return Ok(&[]);
    }
    if pointer.is_null() || (pointer as usize) % core::mem::align_of::<Point>() != 0 {
        return Err(Fault::Argument);
    }
    // The borrowed C API requires readable live arrays for the bounded count. They are
    // copied into Rust metadata before this callback returns; no C pointer is retained.
    // — MILVUS
    Ok(unsafe { core::slice::from_raw_parts(pointer, count as usize) })
}

pub(crate) unsafe extern "C" fn callback<T: SessionTransport, M: Mapper<T::Handle>>(
    context: *mut c_void,
    request: *const Request,
    out: *mut Reply,
) -> u32 {
    if out.is_null() {
        return c::BAD_ARGUMENT;
    }
    unsafe { out.write(Reply::default()) };
    if context.is_null() || request.is_null() {
        return c::BAD_ARGUMENT;
    }
    let request = unsafe { *request };
    if !valid(&request) {
        return c::BAD_ARGUMENT;
    }
    let arrays = unsafe { points(request.waits, request.wait_count) }.and_then(|waits| {
        unsafe { points(request.signals, request.signal_count) }.map(|signals| (waits, signals))
    });
    let Ok((waits, signals)) = arrays else {
        return c::BAD_ARGUMENT;
    };
    let buffers = unsafe { &mut *context.cast::<Buffers<'_, T, M>>() };
    if !buffers.device_ready() && request.operation != GROUP_STATE {
        return c::DEVICE_LOST;
    }
    let mut native = buffers.native.take().unwrap_or_default();
    buffers.barrier_native();
    let result = native.execute(buffers, request, waits, signals);
    buffers.barrier_native();
    buffers.native = Some(native);
    match result {
        Ok(reply) => {
            unsafe { out.write(reply) };
            c::OK
        }
        Err(Fault::Sync(error)) => match error {
            sync::Error::InvalidRequest => c::INVALID_REQUEST,
            sync::Error::NotFound => UNKNOWN_OBJECT,
            sync::Error::NoMemory => c::NO_MEMORY,
            sync::Error::Busy => c::SERVER_ERROR + crate::Status::Busy as u32,
            sync::Error::Unsubmitted { wait_index } => {
                unsafe { (*out).value = u64::from(wait_index) };
                UNSUBMITTED
            }
            sync::Error::Client(error) => c::client_error(error),
            sync::Error::GpuFault(status) => {
                unsafe { (*out).gpu_status = status };
                c::DEVICE_LOST
            }
        },
        Err(Fault::Bo(error)) => c::bo_error(error),
        Err(Fault::Unknown) => UNKNOWN_OBJECT,
        Err(Fault::Argument) => c::BAD_ARGUMENT,
        Err(Fault::Unsupported) => c::SERVER_ERROR + crate::Status::NotSupported as u32,
        Err(Fault::Wait) => WAIT_FAILED,
    }
}

const _: () = assert!(size_of::<Point>() == 16);
const _: () = assert!(size_of::<Job>() == 24);
const _: () = assert!(size_of::<Request>() == 72);
const _: () = assert!(size_of::<Reply>() == 24);
const _: () = assert!(core::mem::offset_of!(Request, waits) == 48);
const _: () = assert!(core::mem::offset_of!(Request, wait_count) == 64);
