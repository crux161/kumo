//j745
//j751
//j752
//j757
//j779

//! Borrowed C query/BO/device boundary for the pinned Mesa backend. Kernel handles and BO ownership
//! stay in Rust; C receives session-local IDs and scoped CPU pointers. — KESTREL

use core::{ffi::c_void, mem::size_of};

use crate::client::{Client, Error, Transport};
use crate::DeviceThreads;

/// ABI 16 (VK2-a, PLAN/015 §8.2): the native request's appended stream array and
/// GROUP_CREATE's three-queue flag, for panvk's GROUP_SUBMIT. The table is unchanged.
pub const VERSION: u32 = 16;
pub const OK: u32 = 0;
pub const BAD_ARGUMENT: u32 = 1;
pub const INVALID_REQUEST: u32 = 2;
pub const TRANSPORT: u32 = 3;
pub const PROTOCOL: u32 = 4;
pub const DEVICE_LOST: u32 = 5;
pub const NO_MEMORY: u32 = 6;
pub const MAP_FAILED: u32 = 7;
pub const UNMAP_FAILED: u32 = 8;
pub const UNKNOWN_BUFFER: u32 = 9;
pub const SERVER_ERROR: u32 = 16;

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Info {
    pub gpu_id: u32,
    pub cs_features: u32,
    /// This service's own window, not the hardware MMU's address range. `mmu_features`
    /// carries the attested register word the consumer derives that range from. — SAKER
    pub va_start: u64,
    pub va_end: u64,
    pub max_buffer_bytes: u64,
    pub shader_present: u64,
    /// Raw `MMU_FEATURES`; bits [7:0] are the hardware virtual-address width.
    pub mmu_features: u32,
    /// Raw `TILER_FEATURES`, or zero when the backend reports it unsupported. Zero is the
    /// only field here with an "absent" meaning, so C must not decode it as a shape. — SAKER
    pub tiler_features: u32,
    /// Raw `TEXTURE_FEATURES[0]`, or zero when absent. Element 0 is the optional-absence
    /// gate; when it is zero the whole array below is zero too. — TIMBERDOODLE
    pub texture_features_0: u32,
    /// Raw `CORE_FEATURES`, crossed verbatim, zero included: the Orange Pi 5 Plus measures
    /// exactly 0x00000000, whose variant byte 0 is the pinned Mesa table's G610 entry. The
    /// query is required — any failure aborts the report — but a zero word is a valid
    /// attestation, not a failed read. — TIMBERDOODLE
    pub core_features: u32,
    /// Raw `THREAD_MAX_THREADS` (GPU_CONTROL +0x0A0), or zero when the thread group is
    /// absent. Upstream copies it to `max_threads_per_core`. — TIMBERDOODLE
    pub thread_max_threads: u32,
    /// Raw `THREAD_MAX_WORKGROUP_SIZE` (+0x0A4), or zero when absent. Upstream copies it to
    /// `max_threads_per_wg`. — TIMBERDOODLE
    pub thread_max_workgroup_size: u32,
    /// Raw `THREAD_FEATURES` (+0x0AC), or zero when absent: `[21:0]` registers per core and
    /// `[31:24]` maximum tasks per core. The derivations are upstream's, not KUMO's.
    /// — TIMBERDOODLE
    pub thread_features: u32,
    /// Raw `TEXTURE_FEATURES[1..3]`, appended so every earlier field keeps its version7
    /// offset. They cross verbatim, zero included, and no consumer here decodes them. An
    /// absent texture attestation leaves all three zero with `texture_features_0`. — TIMBERDOODLE
    pub texture_features_1: u32,
    pub texture_features_2: u32,
    pub texture_features_3: u32,
    /// Version10's memory-system words (G7o-a), raw: `L2_FEATURES`, `MEM_FEATURES` and
    /// `COHERENCY_FEATURES`, then the `GPU_COHERENCY_PROTOCOL` value the driver programmed
    /// (31 is "none"). The query is required, and every word crosses verbatim, zero included.
    /// Upstream derives slices, bus width and `is_io_coherent` from these. — TIMBERDOODLE
    pub l2_features: u32,
    pub mem_features: u32,
    pub coherency_features: u32,
    pub coherency_protocol: u32,
    /// This protocol's VM page granule, which is also the only leaf size drv-gpu writes. Service
    /// policy like the window above, not a hardware report; C turns it into `pgsize_bitmap`.
    /// — TIMBERDOODLE
    pub page_bytes: u64,
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct Api {
    pub version: u32,
    pub bytes: u32,
    pub context: *mut c_void,
    pub query: Option<unsafe extern "C" fn(*mut c_void, *mut Info) -> u32>,
    pub buffer_create:
        Option<unsafe extern "C" fn(*mut c_void, u64, u32, *mut crate::bo::Info) -> u32>,
    pub buffer_destroy: Option<unsafe extern "C" fn(*mut c_void, u32) -> u32>,
    pub buffer_map: Option<unsafe extern "C" fn(*mut c_void, u32, *mut *mut c_void) -> u32>,
    pub buffer_unmap: Option<unsafe extern "C" fn(*mut c_void, u32) -> u32>,
    pub device_open: Option<unsafe extern "C" fn(*mut c_void) -> u32>,
    pub device_close: Option<unsafe extern "C" fn(*mut c_void) -> u32>,
    pub vm_create: Option<unsafe extern "C" fn(*mut c_void, u64, u64, *mut u32) -> u32>,
    pub vm_destroy: Option<unsafe extern "C" fn(*mut c_void, u32) -> u32>,
    pub vm_bind: Option<unsafe extern "C" fn(*mut c_void, *const crate::bo::VmBinding) -> u32>,
    pub vm_unbind: Option<unsafe extern "C" fn(*mut c_void, u32, u64, u64) -> u32>,
    pub device_status: Option<unsafe extern "C" fn(*mut c_void) -> u32>,
    pub native: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *const crate::c_sync::Request,
            *mut crate::c_sync::Reply,
        ) -> u32,
    >,
    /// ABI 15 (G7q-c): device-scope tiler heaps.
    pub heap_create:
        Option<unsafe extern "C" fn(*mut c_void, *const HeapRequest, *mut HeapInfo) -> u32>,
    pub heap_destroy: Option<unsafe extern "C" fn(*mut c_void, u32, *mut u64) -> u32>,
}

//j757
/// TILER_HEAP_CREATE's fields for the device's logical VM; `flags` must be zero. — TIMBERDOODLE
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct HeapRequest {
    pub vm: u32,
    pub chunk_bytes: u32,
    pub initial_chunks: u32,
    pub max_chunks: u32,
    pub target_in_flight: u32,
    pub flags: u32,
}

/// What TILER_HEAP_CREATE returns: the heap ID, its context VA and its first chunk's VA.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct HeapInfo {
    pub heap: u32,
    pub reserved: u32,
    pub context_va: u64,
    pub first_chunk_va: u64,
}
const _: () = assert!(size_of::<HeapRequest>() == 24 && size_of::<HeapInfo>() == 24);

/// The C consumer may call query synchronously during `f` only. It must not retain any
/// pointer, reenter the client, race another call or unwind across C. Calling the raw
/// callback requires a live exclusive context and a valid aligned writable Info.
pub fn with_api<T: Transport, R>(client: &mut Client<T>, f: impl FnOnce(&Api) -> R) -> R {
    f(&Api {
        version: VERSION,
        bytes: size_of::<Api>() as u32,
        context: (client as *mut Client<T>).cast(),
        query: Some(query::<T>),
        buffer_create: None,
        buffer_destroy: None,
        buffer_map: None,
        buffer_unmap: None,
        device_open: None,
        device_close: None,
        vm_create: None,
        vm_destroy: None,
        vm_bind: None,
        vm_unbind: None,
        device_status: None,
        native: None,
        heap_create: None,
        heap_destroy: None,
    })
}

/// Query and BO callbacks borrow the same session and Rust-owned handles. No API, BO or
/// mapped pointer may outlive f, and calls must be serial and non-reentrant. — KESTREL
pub fn with_buffer_api<T: Transport, M: crate::bo::Mapper<T::Handle>, R>(
    buffers: &mut crate::bo::Buffers<'_, T, M>,
    f: impl FnOnce(&Api) -> R,
) -> R {
    f(&Api {
        version: VERSION,
        bytes: size_of::<Api>() as u32,
        context: (buffers as *mut crate::bo::Buffers<'_, T, M>).cast(),
        query: Some(buffer_query::<T, M>),
        buffer_create: Some(buffer_create::<T, M>),
        buffer_destroy: Some(buffer_destroy::<T, M>),
        buffer_map: Some(buffer_map::<T, M>),
        buffer_unmap: Some(buffer_unmap::<T, M>),
        device_open: None,
        device_close: None,
        vm_create: None,
        vm_destroy: None,
        vm_bind: None,
        vm_unbind: None,
        device_status: None,
        native: None,
        heap_create: None,
        heap_destroy: None,
    })
}

/// Own one native session for the callback scope. Close the endpoint on return as well as
/// explicit device destruction; all Mesa objects and pointers must die inside f. — KESTREL
pub fn with_device_api<T: crate::client::SessionTransport, M: crate::bo::Mapper<T::Handle>, R>(
    buffers: &mut crate::bo::Buffers<'_, T, M>,
    f: impl FnOnce(&Api) -> R,
) -> R {
    let mut api = with_buffer_api(buffers, |api| *api);
    api.device_open = Some(device_open::<T, M>);
    api.device_close = Some(device_close::<T, M>);
    api.vm_create = Some(vm_create::<T, M>);
    api.vm_destroy = Some(vm_destroy::<T, M>);
    api.vm_bind = Some(vm_bind::<T, M>);
    api.vm_unbind = Some(vm_unbind::<T, M>);
    api.device_status = Some(device_status::<T, M>);
    api.native = Some(crate::c_sync::callback::<T, M>);
    api.heap_create = Some(heap_create::<T, M>);
    api.heap_destroy = Some(heap_destroy::<T, M>);
    let result = f(&api);
    buffers.close_device();
    result
}
//j757
unsafe extern "C" fn heap_create<
    T: crate::client::SessionTransport,
    M: crate::bo::Mapper<T::Handle>,
>(
    context: *mut c_void,
    request: *const HeapRequest,
    out: *mut HeapInfo,
) -> u32 {
    if context.is_null() || request.is_null() || out.is_null() {
        return BAD_ARGUMENT;
    }
    // Every failure leaves a zeroed answer, so C never reads a stale heap ID.
    unsafe { out.write(HeapInfo::default()) };
    let request = unsafe { *request };
    if request.flags != 0 {
        return INVALID_REQUEST;
    }
    match unsafe { &mut *context.cast::<crate::bo::Buffers<'_, T, M>>() }.create_native_heap(
        request.vm,
        request.chunk_bytes,
        request.initial_chunks,
        request.max_chunks,
        request.target_in_flight,
    ) {
        Ok((heap, placement)) => {
            unsafe {
                out.write(HeapInfo {
                    heap,
                    reserved: 0,
                    context_va: placement.context_va,
                    first_chunk_va: placement.first_chunk_va,
                })
            };
            OK
        }
        Err(error) => bo_error(error),
    }
}
unsafe extern "C" fn heap_destroy<
    T: crate::client::SessionTransport,
    M: crate::bo::Mapper<T::Handle>,
>(
    context: *mut c_void,
    heap: u32,
    bytes: *mut u64,
) -> u32 {
    if context.is_null() || bytes.is_null() {
        return BAD_ARGUMENT;
    }
    unsafe { bytes.write(0) };
    match unsafe { &mut *context.cast::<crate::bo::Buffers<'_, T, M>>() }.destroy_native_heap(heap)
    {
        Ok(released) => {
            unsafe { bytes.write(released) };
            OK
        }
        // A heap this owner never created, or already destroyed, is an unknown object.
        Err(crate::bo::Error::NotFound) => crate::c_sync::UNKNOWN_OBJECT,
        Err(error) => bo_error(error),
    }
}
unsafe extern "C" fn device_open<
    T: crate::client::SessionTransport,
    M: crate::bo::Mapper<T::Handle>,
>(
    context: *mut c_void,
) -> u32 {
    if context.is_null() {
        return BAD_ARGUMENT;
    }
    unsafe { &mut *context.cast::<crate::bo::Buffers<'_, T, M>>() }
        .open_device()
        .map_or_else(bo_error, |()| OK)
}
unsafe extern "C" fn device_close<
    T: crate::client::SessionTransport,
    M: crate::bo::Mapper<T::Handle>,
>(
    context: *mut c_void,
) -> u32 {
    if context.is_null() {
        return BAD_ARGUMENT;
    }
    unsafe { &mut *context.cast::<crate::bo::Buffers<'_, T, M>>() }.close_device();
    OK
}
unsafe extern "C" fn device_status<
    T: crate::client::SessionTransport,
    M: crate::bo::Mapper<T::Handle>,
>(
    context: *mut c_void,
) -> u32 {
    if context.is_null() {
        return BAD_ARGUMENT;
    }
    if unsafe { &*context.cast::<crate::bo::Buffers<'_, T, M>>() }.device_ready() {
        OK
    } else {
        DEVICE_LOST
    }
}
unsafe extern "C" fn vm_create<
    T: crate::client::SessionTransport,
    M: crate::bo::Mapper<T::Handle>,
>(
    context: *mut c_void,
    start: u64,
    bytes: u64,
    out: *mut u32,
) -> u32 {
    if context.is_null() || out.is_null() {
        return BAD_ARGUMENT;
    }
    unsafe { out.write(0) };
    match unsafe { &mut *context.cast::<crate::bo::Buffers<'_, T, M>>() }
        .create_native_vm(start, bytes)
    {
        Ok(id) => {
            unsafe { out.write(id) };
            OK
        }
        Err(error) => bo_error(error),
    }
}
unsafe extern "C" fn vm_destroy<
    T: crate::client::SessionTransport,
    M: crate::bo::Mapper<T::Handle>,
>(
    context: *mut c_void,
    id: u32,
) -> u32 {
    if context.is_null() {
        return BAD_ARGUMENT;
    }
    unsafe { &mut *context.cast::<crate::bo::Buffers<'_, T, M>>() }
        .destroy_native_vm(id)
        .map_or_else(bo_error, |()| OK)
}
unsafe extern "C" fn vm_bind<
    T: crate::client::SessionTransport,
    M: crate::bo::Mapper<T::Handle>,
>(
    context: *mut c_void,
    request: *const crate::bo::VmBinding,
) -> u32 {
    if context.is_null() || request.is_null() {
        return BAD_ARGUMENT;
    }
    unsafe { &mut *context.cast::<crate::bo::Buffers<'_, T, M>>() }
        .bind_native(unsafe { *request })
        .map_or_else(bo_error, |()| OK)
}
unsafe extern "C" fn vm_unbind<
    T: crate::client::SessionTransport,
    M: crate::bo::Mapper<T::Handle>,
>(
    context: *mut c_void,
    id: u32,
    va: u64,
    bytes: u64,
) -> u32 {
    if context.is_null() {
        return BAD_ARGUMENT;
    }
    unsafe { &mut *context.cast::<crate::bo::Buffers<'_, T, M>>() }
        .unbind_native(id, va, bytes)
        .map_or_else(bo_error, |()| OK)
}

pub(crate) fn bo_error(error: crate::bo::Error) -> u32 {
    use crate::bo::Error as E;
    match error {
        E::Client(error) => client_error(error),
        E::InvalidRequest => INVALID_REQUEST,
        E::NotFound => UNKNOWN_BUFFER,
        E::NoMemory => NO_MEMORY,
        E::Map(crate::bo::MapError::NoMemory) => NO_MEMORY,
        E::Map(_) => MAP_FAILED,
        E::Unmap(_) => UNMAP_FAILED,
    }
}
pub(crate) fn client_error(error: Error) -> u32 {
    match error {
        Error::InvalidRequest => INVALID_REQUEST,
        Error::Transport(_) => TRANSPORT,
        Error::Protocol => PROTOCOL,
        Error::DeviceLost | Error::RequestIdsExhausted => DEVICE_LOST,
        Error::Server(status) => SERVER_ERROR + status as u32,
    }
}
unsafe extern "C" fn buffer_query<T: Transport, M: crate::bo::Mapper<T::Handle>>(
    context: *mut c_void,
    out: *mut Info,
) -> u32 {
    if context.is_null() || out.is_null() {
        return BAD_ARGUMENT;
    }
    let buffers = unsafe { &mut *context.cast::<crate::bo::Buffers<'_, T, M>>() };
    unsafe { query::<T>((buffers.client as *mut Client<T>).cast(), out) }
}
unsafe extern "C" fn buffer_create<T: Transport, M: crate::bo::Mapper<T::Handle>>(
    context: *mut c_void,
    bytes: u64,
    flags: u32,
    out: *mut crate::bo::Info,
) -> u32 {
    if context.is_null() || out.is_null() {
        return BAD_ARGUMENT;
    }
    unsafe { out.write(crate::bo::Info::default()) };
    let buffers = unsafe { &mut *context.cast::<crate::bo::Buffers<'_, T, M>>() };
    match buffers.create(bytes, flags) {
        Ok(info) => {
            unsafe { out.write(info) };
            OK
        }
        Err(error) => bo_error(error),
    }
}
unsafe extern "C" fn buffer_destroy<T: Transport, M: crate::bo::Mapper<T::Handle>>(
    context: *mut c_void,
    id: u32,
) -> u32 {
    if context.is_null() {
        return BAD_ARGUMENT;
    }
    let buffers = unsafe { &mut *context.cast::<crate::bo::Buffers<'_, T, M>>() };
    buffers.destroy(id).map_or_else(bo_error, |()| OK)
}
unsafe extern "C" fn buffer_map<T: Transport, M: crate::bo::Mapper<T::Handle>>(
    context: *mut c_void,
    id: u32,
    out: *mut *mut c_void,
) -> u32 {
    if context.is_null() || out.is_null() {
        return BAD_ARGUMENT;
    }
    unsafe { out.write(core::ptr::null_mut()) };
    let buffers = unsafe { &mut *context.cast::<crate::bo::Buffers<'_, T, M>>() };
    match buffers.map(id) {
        Ok(address) => {
            unsafe { out.write(address.as_ptr().cast()) };
            OK
        }
        Err(error) => bo_error(error),
    }
}
unsafe extern "C" fn buffer_unmap<T: Transport, M: crate::bo::Mapper<T::Handle>>(
    context: *mut c_void,
    id: u32,
) -> u32 {
    if context.is_null() {
        return BAD_ARGUMENT;
    }
    let buffers = unsafe { &mut *context.cast::<crate::bo::Buffers<'_, T, M>>() };
    buffers.unmap(id).map_or_else(bo_error, |()| OK)
}
const _: () = assert!(size_of::<Api>() == 136);
const _: () = assert!(core::mem::offset_of!(Api, native) == 112);
const _: () = assert!(core::mem::offset_of!(Api, buffer_create) == 24);
const _: () = assert!(core::mem::offset_of!(Api, buffer_destroy) == 32);
const _: () = assert!(core::mem::offset_of!(Api, buffer_map) == 40);
const _: () = assert!(core::mem::offset_of!(Api, buffer_unmap) == 48);
const _: () = assert!(core::mem::offset_of!(Api, device_open) == 56);
const _: () = assert!(core::mem::offset_of!(Api, device_status) == 104);
const _: () = assert!(size_of::<crate::bo::VmBinding>() == 32);
const _: () = assert!(size_of::<crate::bo::Info>() == 16);

unsafe extern "C" fn query<T: Transport>(context: *mut c_void, out: *mut Info) -> u32 {
    if context.is_null() || out.is_null() {
        return BAD_ARGUMENT;
    }
    // The scoped C contract supplies exclusive live pointers; null refuses before I/O.
    // Clear the result even on failure so an old identity cannot look successful. — KESTREL
    unsafe { out.write(Info::default()) };
    let client = unsafe { &mut *context.cast::<Client<T>>() };
    let result: Result<Info, Error> = (|| {
        let info = client.info()?;
        let topology = client.topology()?;
        let mmu = client.mmu()?;
        // Tiler and texture properties are optional. NotSupported means the property is
        // unknown, not that the device is unusable, so the query still succeeds and reports
        // zero for C to skip; any other failure still aborts and publishes nothing. The
        // texture arm carries all four attested words, zero elements 1..3 included. — SAKER
        let tiler_features = match client.tiler() {
            Ok(tiler) => tiler.tiler_features,
            Err(Error::Server(crate::Status::NotSupported)) => 0,
            Err(error) => return Err(error),
        };
        let texture_features = match client.texture() {
            Ok(texture) => texture.texture_features,
            Err(Error::Server(crate::Status::NotSupported)) => [0; 4],
            Err(error) => return Err(error),
        };
        // Core features are required, so any failure — NotSupported included — aborts the
        // whole query and publishes nothing, exactly like MMU features; the word itself
        // crosses verbatim, zero included. — TIMBERDOODLE
        let core = client.core()?;
        // Thread properties are optional. NotSupported means the group is absent, not that
        // the device is unusable, so the query still succeeds with three zero words and C
        // leaves missing-property bit 4 set; any other failure aborts and publishes nothing.
        // — TIMBERDOODLE
        let threads = match client.threads() {
            Ok(threads) => threads,
            Err(Error::Server(crate::Status::NotSupported)) => DeviceThreads {
                max_threads: 0,
                max_workgroup_size: 0,
                thread_features: 0,
            },
            Err(error) => return Err(error),
        };
        // Memory properties are required, so any failure — NotSupported included — aborts the
        // whole query, like core features. Asked last, so every earlier failure path keeps
        // its request count. — TIMBERDOODLE
        let memory = client.memory()?;
        Ok(Info {
            gpu_id: info.gpu_id,
            cs_features: info.cs_features,
            va_start: crate::VA_START,
            va_end: crate::VA_END,
            max_buffer_bytes: crate::MAX_BUFFER_BYTES,
            shader_present: topology.shader_present,
            mmu_features: mmu.mmu_features,
            tiler_features,
            texture_features_0: texture_features[0],
            core_features: core.core_features,
            thread_max_threads: threads.max_threads,
            thread_max_workgroup_size: threads.max_workgroup_size,
            thread_features: threads.thread_features,
            texture_features_1: texture_features[1],
            texture_features_2: texture_features[2],
            texture_features_3: texture_features[3],
            l2_features: memory.l2_features,
            mem_features: memory.mem_features,
            coherency_features: memory.coherency_features,
            coherency_protocol: memory.coherency_protocol,
            page_bytes: crate::PAGE_BYTES,
        })
    })();
    match result {
        Ok(info) => {
            unsafe { out.write(info) };
            OK
        }
        Err(error) => client_error(error),
    }
}

const _: () = assert!(size_of::<Info>() == 104);
// Pin both tail offsets. `tiler_features` exactly fills the padding that followed
// `mmu_features`, so version4's Info is the same *size* as version3's and a size check alone
// cannot distinguish them — only the version field and these offsets can. Version7 appended
// the thread words after `core_features`; version8 appends the remaining texture words
// immediately after those (u32 fields pack at 68/72/76), so every earlier offset is unchanged
// and the structure ends exactly at 80 bytes with no tail padding. — SAKER, TIMBERDOODLE
const _: () = assert!(core::mem::offset_of!(Info, mmu_features) == 40);
const _: () = assert!(core::mem::offset_of!(Info, tiler_features) == 44);
const _: () = assert!(core::mem::offset_of!(Info, texture_features_0) == 48);
const _: () = assert!(core::mem::offset_of!(Info, core_features) == 52);
// Version7's three thread words follow core_features. — TIMBERDOODLE
const _: () = assert!(core::mem::offset_of!(Info, thread_max_threads) == 56);
const _: () = assert!(core::mem::offset_of!(Info, thread_max_workgroup_size) == 60);
const _: () = assert!(core::mem::offset_of!(Info, thread_features) == 64);
// Version8 appends the remaining texture words after the thread block; both are u32 fields,
// so they pack at 68/72/76 and the structure ends exactly at 80 bytes. — TIMBERDOODLE
const _: () = assert!(core::mem::offset_of!(Info, texture_features_1) == 68);
const _: () = assert!(core::mem::offset_of!(Info, texture_features_2) == 72);
const _: () = assert!(core::mem::offset_of!(Info, texture_features_3) == 76);
// Version10 appends four u32 memory words at 80..96, then the u64 page granule at its natural
// 96, so the structure ends at 104 with no padding anywhere. — TIMBERDOODLE
const _: () = assert!(core::mem::offset_of!(Info, l2_features) == 80);
const _: () = assert!(core::mem::offset_of!(Info, mem_features) == 84);
const _: () = assert!(core::mem::offset_of!(Info, coherency_features) == 88);
const _: () = assert!(core::mem::offset_of!(Info, coherency_protocol) == 92);
const _: () = assert!(core::mem::offset_of!(Info, page_bytes) == 96);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        client::TransportError, Command, DeviceCore, DeviceInfo, DeviceMemory, DeviceMmu,
        DeviceTexture, DeviceThreads, DeviceTiler, DeviceTopology, Reply, Request, Response,
        Status, REPLY_LEN, REQUEST_LEN,
    };

    #[derive(Default)]
    struct Wire {
        sent: u32,
        command: Option<Command>,
        closed: u32,
        failure: bool,
        mmu_failure: bool,
        tiler_unsupported: bool,
        texture_unsupported: bool,
        texture_failure: bool,
        core_failure: bool,
        threads_unsupported: bool,
        threads_failure: bool,
        memory_unsupported: bool,
        grant: bool,
    }
    impl Transport for Wire {
        type Handle = u32;
        fn send(&mut self, bytes: &[u8; REQUEST_LEN]) -> Result<(), TransportError> {
            let request = Request::decode(bytes).unwrap();
            assert_eq!(request.id, self.sent + 1);
            self.sent = request.id;
            self.command = Some(request.command);
            Ok(())
        }
        fn receive(
            &mut self,
            bytes: &mut [u8; REPLY_LEN + 1],
        ) -> Result<(usize, Option<u32>), TransportError> {
            let result = if self.failure {
                Err(Status::Busy)
            } else if self.mmu_failure && self.command == Some(Command::GetMmuFeatures) {
                Err(Status::Busy)
            } else if self.command == Some(Command::GetTopology) {
                Ok(Response::Topology(DeviceTopology {
                    shader_present: 0x500000005,
                }))
            } else if self.command == Some(Command::GetMmuFeatures) {
                Ok(Response::Mmu(DeviceMmu {
                    mmu_features: 0x2830,
                }))
            } else if self.command == Some(Command::GetTilerFeatures) {
                if self.tiler_unsupported {
                    Err(Status::NotSupported)
                } else {
                    Ok(Response::Tiler(DeviceTiler {
                        tiler_features: 0x809,
                    }))
                }
            } else if self.command == Some(Command::GetTextureFeatures) {
                if self.texture_failure {
                    Err(Status::Busy)
                } else if self.texture_unsupported {
                    Err(Status::NotSupported)
                } else {
                    Ok(Response::Texture(DeviceTexture {
                        // A modeled full array: element 1 is an attested zero, element 2 carries
                        // the top bit and element 3 is all ones, so nothing narrows early. — TIMBERDOODLE
                        texture_features: [0x0200_0001, 0, 0x8000_0000, 0xffff_ffff],
                    }))
                }
            } else if self.command == Some(Command::GetCoreFeatures) {
                if self.core_failure {
                    Err(Status::Busy)
                } else {
                    Ok(Response::Core(DeviceCore {
                        core_features: 0x0000_1000,
                    }))
                }
            } else if self.command == Some(Command::GetThreadProperties) {
                if self.threads_failure {
                    Err(Status::Busy)
                } else if self.threads_unsupported {
                    Err(Status::NotSupported)
                } else {
                    Ok(Response::Threads(DeviceThreads {
                        max_threads: 1024,
                        max_workgroup_size: 512,
                        thread_features: 0x0310_0005,
                    }))
                }
            } else if self.command == Some(Command::GetMemoryProperties) {
                if self.memory_unsupported {
                    Err(Status::NotSupported)
                } else {
                    // The Orange Pi's measured words (J653) and the "none" protocol. — TIMBERDOODLE
                    Ok(Response::Memory(DeviceMemory {
                        l2_features: 0x0712_0306,
                        mem_features: 0x301,
                        coherency_features: 0,
                        coherency_protocol: 31,
                    }))
                }
            } else {
                Ok(Response::Info(DeviceInfo {
                    gpu_id: 0xa8670005,
                    cs_features: 0x7085f,
                }))
            };
            bytes[..REPLY_LEN].copy_from_slice(
                &Reply {
                    id: self.sent,
                    result,
                }
                .encode(),
            );
            self.failure = false;
            Ok((REPLY_LEN, self.grant.then_some(77)))
        }
        fn close_handle(&mut self, handle: u32) {
            assert_eq!(handle, 77);
            self.closed += 1;
        }
    }

    #[test]
    fn borrowed_queries_keep_ids_and_preserve_recoverable_server_errors() {
        let mut client = Client::new(Wire {
            failure: true,
            ..Wire::default()
        });
        with_api(&mut client, |api| {
            assert_eq!(api.version, VERSION);
            assert_eq!(api.bytes as usize, size_of::<Api>());
            let mut out = Info {
                gpu_id: 99,
                ..Info::default()
            };
            let query = api.query.unwrap();
            assert_eq!(
                unsafe { query(api.context, &mut out) },
                SERVER_ERROR + Status::Busy as u32
            );
            assert_eq!(out.gpu_id, 0);
            for _ in 0..2 {
                assert_eq!(unsafe { query(api.context, &mut out) }, OK);
                assert_eq!(out.gpu_id, 0xa8670005);
                assert_eq!(
                    (out.va_start, out.va_end, out.max_buffer_bytes),
                    (crate::VA_START, crate::VA_END, crate::MAX_BUFFER_BYTES)
                );
                assert_eq!(out.shader_present, 0x500000005);
                assert_eq!(out.mmu_features, 0x2830);
                assert_eq!(out.tiler_features, 0x809);
                assert_eq!(out.texture_features_0, 0x0200_0001);
                assert_eq!(out.texture_features_1, 0);
                assert_eq!(out.texture_features_2, 0x8000_0000);
                assert_eq!(out.texture_features_3, 0xffff_ffff);
                assert_eq!(out.core_features, 0x0000_1000);
                assert_eq!(out.thread_max_threads, 1024);
                assert_eq!(out.thread_max_workgroup_size, 512);
                assert_eq!(out.thread_features, 0x0310_0005);
                assert_eq!(
                    (
                        out.l2_features,
                        out.mem_features,
                        out.coherency_features,
                        out.coherency_protocol
                    ),
                    (0x0712_0306, 0x301, 0, 31)
                );
                assert_eq!(out.page_bytes, crate::PAGE_BYTES);
            }
        });
        assert!(client.info().is_ok());
    }

    #[test]
    fn an_unsupported_memory_query_is_required_and_publishes_nothing() {
        let mut client = Client::new(Wire {
            memory_unsupported: true,
            ..Wire::default()
        });
        with_api(&mut client, |api| {
            let mut out = Info {
                gpu_id: 99,
                thread_features: 99,
                l2_features: 99,
                coherency_protocol: 99,
                page_bytes: 99,
                ..Info::default()
            };
            let query = api.query.unwrap();
            // All seven earlier queries succeed. Unlike tiler, texture or threads, NotSupported
            // here is not an absence: the words are required, so the whole report is withheld.
            // — TIMBERDOODLE
            assert_eq!(
                unsafe { query(api.context, &mut out) },
                SERVER_ERROR + Status::NotSupported as u32
            );
            assert_eq!(
                (
                    out.gpu_id,
                    out.thread_features,
                    out.l2_features,
                    out.coherency_protocol,
                    out.page_bytes
                ),
                (0, 0, 0, 0, 0)
            );
            assert_eq!((out.va_start, out.va_end), (0, 0));
        });
        // A refused query is recoverable: the client stays live. — TIMBERDOODLE
        assert!(!client.is_lost());
        assert!(client.info().is_ok());
    }

    #[test]
    fn unsupported_optional_queries_degrade_to_zero_without_failing_the_report() {
        let mut client = Client::new(Wire {
            tiler_unsupported: true,
            texture_unsupported: true,
            threads_unsupported: true,
            ..Wire::default()
        });
        with_api(&mut client, |api| {
            let mut out = Info {
                tiler_features: 99,
                texture_features_0: 99,
                thread_max_threads: 99,
                thread_max_workgroup_size: 99,
                thread_features: 99,
                texture_features_1: 99,
                texture_features_2: 99,
                texture_features_3: 99,
                ..Info::default()
            };
            let query = api.query.unwrap();
            // NotSupported is the one status that means "this optional property is absent",
            // so the rest of the report is still published and the client stays live. All
            // four texture words are zero because the absence gate is element 0. — TIMBERDOODLE
            assert_eq!(unsafe { query(api.context, &mut out) }, OK);
            assert_eq!(out.tiler_features, 0);
            assert_eq!(
                (
                    out.texture_features_0,
                    out.texture_features_1,
                    out.texture_features_2,
                    out.texture_features_3
                ),
                (0, 0, 0, 0)
            );
            assert_eq!(out.thread_max_threads, 0);
            assert_eq!(out.thread_max_workgroup_size, 0);
            assert_eq!(out.thread_features, 0);
            assert_eq!(out.core_features, 0x0000_1000);
            assert_eq!(out.gpu_id, 0xa8670005);
            assert_eq!(out.mmu_features, 0x2830);
            assert_eq!(out.shader_present, 0x500000005);
        });
        assert!(!client.is_lost());
        assert!(client.info().is_ok());
    }

    #[test]
    fn a_failing_final_query_publishes_no_partially_filled_report() {
        let mut client = Client::new(Wire {
            mmu_failure: true,
            ..Wire::default()
        });
        with_api(&mut client, |api| {
            let mut out = Info {
                gpu_id: 99,
                shader_present: 99,
                mmu_features: 99,
                ..Info::default()
            };
            let query = api.query.unwrap();
            // GetInfo and GetTopology both succeed, so a naive implementation would already
            // have written identity and topology. Nothing may be published. — SAKER
            assert_eq!(
                unsafe { query(api.context, &mut out) },
                SERVER_ERROR + Status::Busy as u32
            );
            assert_eq!(
                (out.gpu_id, out.shader_present, out.mmu_features),
                (0, 0, 0)
            );
            assert_eq!((out.va_start, out.va_end), (0, 0));
        });
        // Busy is recoverable: the client stays live and the next full query succeeds.
        assert!(!client.is_lost());
        assert!(client.info().is_ok());
    }

    #[test]
    fn a_failing_core_query_publishes_no_partially_filled_report() {
        let mut client = Client::new(Wire {
            core_failure: true,
            ..Wire::default()
        });
        with_api(&mut client, |api| {
            let mut out = Info {
                gpu_id: 99,
                core_features: 99,
                ..Info::default()
            };
            let query = api.query.unwrap();
            // All five earlier queries succeed, so a naive implementation would already have
            // written identity, topology, MMU, tiler and texture. Nothing may be published. — TIMBERDOODLE
            assert_eq!(
                unsafe { query(api.context, &mut out) },
                SERVER_ERROR + Status::Busy as u32
            );
            assert_eq!(
                (
                    out.gpu_id,
                    out.shader_present,
                    out.mmu_features,
                    out.core_features
                ),
                (0, 0, 0, 0)
            );
            assert_eq!((out.va_start, out.va_end), (0, 0));
        });
        // Busy is recoverable: the client stays live and the next full query succeeds.
        assert!(!client.is_lost());
        assert!(client.info().is_ok());
    }

    #[test]
    fn a_failing_thread_query_publishes_no_partially_filled_report() {
        let mut client = Client::new(Wire {
            threads_failure: true,
            ..Wire::default()
        });
        with_api(&mut client, |api| {
            let mut out = Info {
                gpu_id: 99,
                core_features: 99,
                thread_max_threads: 99,
                thread_max_workgroup_size: 99,
                thread_features: 99,
                ..Info::default()
            };
            let query = api.query.unwrap();
            // All six earlier queries succeed, so a naive implementation would already have
            // written every earlier field. A non-NotSupported thread failure must publish
            // nothing, unlike the absent-group NotSupported path. — TIMBERDOODLE
            assert_eq!(
                unsafe { query(api.context, &mut out) },
                SERVER_ERROR + Status::Busy as u32
            );
            assert_eq!(
                (
                    out.gpu_id,
                    out.shader_present,
                    out.mmu_features,
                    out.core_features,
                    out.thread_max_threads,
                    out.thread_max_workgroup_size,
                    out.thread_features
                ),
                (0, 0, 0, 0, 0, 0, 0)
            );
            assert_eq!((out.va_start, out.va_end), (0, 0));
        });
        // Busy is recoverable: the client stays live and the next full query succeeds.
        assert!(!client.is_lost());
        assert!(client.info().is_ok());
    }

    #[test]
    fn a_failing_texture_query_publishes_no_partially_filled_report() {
        let mut client = Client::new(Wire {
            texture_failure: true,
            ..Wire::default()
        });
        with_api(&mut client, |api| {
            let mut out = Info {
                gpu_id: 99,
                tiler_features: 99,
                texture_features_0: 99,
                texture_features_1: 99,
                texture_features_2: 99,
                texture_features_3: 99,
                ..Info::default()
            };
            let query = api.query.unwrap();
            // GetInfo, GetTopology, GetMmu and GetTiler all succeed, so a naive implementation
            // would already have written identity, topology and the earlier optional words. A
            // texture failure that is not NotSupported aborts everything. — TIMBERDOODLE
            assert_eq!(
                unsafe { query(api.context, &mut out) },
                SERVER_ERROR + Status::Busy as u32
            );
            assert_eq!(
                (
                    out.gpu_id,
                    out.shader_present,
                    out.mmu_features,
                    out.tiler_features,
                    out.texture_features_0,
                    out.texture_features_1,
                    out.texture_features_2,
                    out.texture_features_3
                ),
                (0, 0, 0, 0, 0, 0, 0, 0)
            );
            assert_eq!((out.va_start, out.va_end), (0, 0));
        });
        // Busy is recoverable: the client stays live and the next full query succeeds.
        assert!(!client.is_lost());
        assert!(client.info().is_ok());
    }

    #[test]
    fn rejected_grants_and_loss_keep_the_shared_client_contract() {
        let mut client = Client::new(Wire {
            grant: true,
            ..Wire::default()
        });
        with_api(&mut client, |api| {
            let mut out = Info::default();
            let query = api.query.unwrap();
            assert_eq!(unsafe { query(api.context, &mut out) }, PROTOCOL);
            assert_eq!(unsafe { query(api.context, &mut out) }, DEVICE_LOST);
        });
        assert!(client.is_lost());
        // Client's existing tests prove exact rejected-handle closure and no further I/O;
        // this test proves C observes the same loss rather than a recreated session. — KESTREL
    }

    #[test]
    fn null_arguments_refuse_without_consuming_request_ids() {
        let mut client = Client::new(Wire::default());
        with_api(&mut client, |api| {
            let query = api.query.unwrap();
            let mut out = Info::default();
            assert_eq!(
                unsafe { query(core::ptr::null_mut(), &mut out) },
                BAD_ARGUMENT
            );
            assert_eq!(
                unsafe { query(api.context, core::ptr::null_mut()) },
                BAD_ARGUMENT
            );
            assert_eq!(unsafe { query(api.context, &mut out) }, OK);
        });
    }
}
