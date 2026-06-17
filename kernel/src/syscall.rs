use alloc::vec::Vec;

use kumo_abi::{BootInfo, Errno, Handle, KoId, ObjectKind, Rights, Status, Syscall};
use kumo_hal::PageFlags;
use kumo_ipc::Message;

use crate::ipc::{IpcError, IpcRegistry, KernelMessage, PortPacket};
use crate::mm::{Mapping, Vmar, Vmo};
use crate::object::{ObjectError, ObjectManager};
use crate::task::{Job, Process, Thread};

#[derive(Clone, Copy, Debug)]
pub enum KernelCall<'a> {
    HandleClose {
        handle: Handle,
    },
    HandleDuplicate {
        handle: Handle,
        rights: Rights,
    },
    ChannelCreate,
    ChannelWrite {
        channel: Handle,
        message: Message<'a>,
    },
    ChannelRead {
        channel: Handle,
    },
    PortCreate,
    PortWait {
        port: Handle,
    },
    VmoRead {
        vmo: Handle,
        offset: u64,
        dest: *mut u8,
        len: usize,
    },
    VmoWrite {
        vmo: Handle,
        offset: u64,
        src: *const u8,
        len: usize,
    },
    ProcessCreate {
        parent_job: Job,
        vmar_base: u64,
        vmar_size: u64,
    },
    VmarMap {
        process_handle: Handle,
        vmo_handle: Handle,
        vmo_offset: u64,
        virt: u64,
        len: u64,
        flags: PageFlags,
    },
    ThreadCreate {
        process_handle: Handle,
    },
    ThreadStart {
        thread_handle: Handle,
        entry: u64,
        sp: u64,
        arg: u64,
    },
    AddressSpaceCreate {
        process_handle: Handle,
        stack_virt: u64,
        stack_size: u64,
    },
    ProcessRun {
        process_handle: Handle,
        entry: u64,
        sp: u64,
        arg: u64,
        arg2: u64,
        /// Bit 0 = async (don't block, admit at low priority).
        flags: u64,
    },
    /// P10-g: block until the async child process exits.
    ProcessWait,
    InterruptCreate {
        irq: u32,
    },
    InterruptWait {
        interrupt: Handle,
    },
    ResourceMintMmio {
        resource: Handle,
        phys_base: u64,
        len: u64,
    },
    PortBind {
        port: Handle,
        object: Handle,
    },
    HandleKoid {
        handle: Handle,
    },
    VmoCreate {
        size: u64,
    },
    Unsupported(Syscall),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KernelCallResult {
    Status(Status),
    Handles { first: Handle, second: Handle },
    Handle(Handle),
    Message(KernelMessage),
    PortPacket(PortPacket),
}

/// An interrupt binding: (irq_number, object_koid, fire_count).
#[derive(Clone, Copy, Debug)]
struct IrqBinding {
    irq: u32,
    koid: KoId,
    count: u64,
}

/// A Resource grant: the holder may mint VMOs from this MMIO range.
#[derive(Clone, Copy, Debug)]
struct ResourceBinding {
    koid: KoId,
    phys_base: u64,
    len: u64,
}

/// Stored VMO with optional pre-allocated frames for anonymous backing.
#[derive(Clone, Debug)]
struct VmoEntry {
    koid: KoId,
    vmo: Vmo,
    #[allow(dead_code)]
    frames: Vec<Option<u64>>,
}

const PHYSMAP_BASE: u64 = 0xffff_9000_0000_0000;

fn phys_ptr_mut(phys: u64) -> *mut u8 {
    (PHYSMAP_BASE + phys) as *mut u8
}

fn alloc_anonymous_frame(boot: &BootInfo) -> Result<u64, Errno> {
    #[cfg(target_os = "none")]
    {
        let saved_ttbr0 = kumo_hal::active::read_user_aspace_root();
        unsafe { kumo_hal::active::set_user_aspace_root(crate::user_thread::kernel_ttbr0()) };
        let frame = unsafe { crate::mm::alloc_zeroed_frame(boot) };
        unsafe { kumo_hal::active::set_user_aspace_root(saved_ttbr0) };
        frame.ok_or(Errno::NoMemory)
    }

    #[cfg(not(target_os = "none"))]
    {
        unsafe { crate::mm::alloc_zeroed_frame(boot) }.ok_or(Errno::NoMemory)
    }
}

fn ensure_anonymous_frame(
    entry: &mut VmoEntry,
    page_index: usize,
    boot: &BootInfo,
) -> Result<u64, Errno> {
    if entry.frames.len() <= page_index {
        entry.frames.resize(page_index + 1, None);
    }
    if let Some(frame) = entry.frames[page_index] {
        return Ok(frame);
    }
    let frame = alloc_anonymous_frame(boot)?;
    entry.frames[page_index] = Some(frame);
    Ok(frame)
}

#[derive(Clone, Debug, Default)]
pub struct SyscallEngine {
    objects: ObjectManager,
    ipc: IpcRegistry,
    vmos: Vec<VmoEntry>,
    processes: Vec<Process>,
    threads: Vec<Thread>,
    boot_info: Option<kumo_abi::BootInfo>,
    interrupts: Vec<IrqBinding>,
    resources: Vec<ResourceBinding>,
    port_bindings: Vec<(KoId, KoId)>,
}

impl SyscallEngine {
    pub fn new() -> Self {
        Self {
            objects: ObjectManager::new(),
            ipc: IpcRegistry::new(),
            vmos: Vec::new(),
            processes: Vec::new(),
            threads: Vec::new(),
            boot_info: None,
            interrupts: Vec::new(),
            resources: Vec::new(),
            port_bindings: Vec::new(),
        }
    }

    /// Create a root Resource covering `[phys_base, phys_base + len)`. Returns a handle
    /// for `caller`. This is the scaffold — Gouchen will mint per-device Resources later.
    pub fn root_resource_create(
        &mut self,
        caller: &mut Process,
        phys_base: u64,
        len: u64,
    ) -> Result<Handle, ObjectError> {
        let object = self.objects.create(ObjectKind::Resource);
        let handle = caller
            .handles_mut()
            .insert(object, Rights::READ | Rights::WRITE | Rights::DUPLICATE)?;
        self.resources.push(ResourceBinding {
            koid: object.koid(),
            phys_base,
            len,
        });
        Ok(handle)
    }

    /// Look up a Resource binding by koid.
    fn resource_by_koid(&self, koid: KoId) -> Option<ResourceBinding> {
        self.resources
            .iter()
            .find_map(|r| if r.koid == koid { Some(*r) } else { None })
    }

    /// Signal all interrupt objects bound to `irq`. Called from the IRQ handler.
    /// Increments the fire count for each matching binding.
    pub fn signal_interrupt(&mut self, irq: u32) {
        for binding in &mut self.interrupts {
            if binding.irq == irq {
                binding.count = binding.count.saturating_add(1);
                #[cfg(target_os = "none")]
                crate::user_thread::wake_child_waiting_on_interrupt(binding.koid);
            }
        }
    }

    /// Bind an object to a port. When an event occurs on that object,
    /// the port is signalled with the corresponding signals and the object koid as source.
    pub fn port_bind(&mut self, port_koid: KoId, object_koid: KoId) {
        self.port_bindings.push((port_koid, object_koid));
    }

    /// Signal all ports bound to `object_koid`.
    pub fn signal_ports(&mut self, object_koid: KoId, signals: kumo_abi::Signals) {
        for &(port_koid, bound_koid) in &self.port_bindings {
            if bound_koid == object_koid {
                // Queue a signal on the port. We need access to the IpcRegistry.
                // The signal is queued via the IPC layer.
                self.ipc
                    .port_queue_signal_by_koid(port_koid, object_koid, signals);
                #[cfg(target_os = "none")]
                crate::user_thread::wake_child_waiting_on_port(port_koid);
            }
        }
    }

    pub fn set_boot_info(&mut self, boot: kumo_abi::BootInfo) {
        self.boot_info = Some(boot);
    }

    pub fn objects_mut(&mut self) -> &mut ObjectManager {
        &mut self.objects
    }

    pub fn ipc_mut(&mut self) -> &mut IpcRegistry {
        &mut self.ipc
    }

    /// Create a channel pair: both endpoints go to `process`. Convenience wrapper
    /// that avoids borrow conflicts between `objects` and `ipc`.
    pub fn channel_create(
        &mut self,
        process: &mut Process,
    ) -> Result<(Handle, Handle), crate::ipc::IpcError> {
        self.ipc.channel_create(&mut self.objects, process)
    }

    /// Create a root channel: one endpoint for `process`, one retained by the kernel.
    /// Convenience wrapper that avoids borrow conflicts between `objects` and `ipc`.
    pub fn root_channel_create(
        &mut self,
        process: &mut Process,
    ) -> Result<(Handle, usize, crate::ipc::ChannelEnd), crate::ipc::IpcError> {
        self.ipc.root_channel_create(&mut self.objects, process)
    }

    /// Create a VMO handle for `process`. The VMO is stored in the engine keyed by koid,
    /// so future syscalls (VmoOp) can resolve it.
    pub fn root_vmo_create(
        &mut self,
        process: &mut Process,
        vmo: Vmo,
        rights: Rights,
    ) -> Result<Handle, ObjectError> {
        let object = self.objects.create(ObjectKind::Vmo);
        let handle = process.handles_mut().insert(object, rights)?;
        self.vmos.push(VmoEntry {
            koid: object.koid(),
            vmo,
            frames: Vec::new(),
        });
        Ok(handle)
    }

    /// Look up a VMO by koid. Returns `None` if not found.
    pub fn vmo_by_koid(&self, koid: KoId) -> Option<Vmo> {
        self.vmos.iter().find_map(|entry| {
            if entry.koid == koid {
                Some(entry.vmo)
            } else {
                None
            }
        })
    }

    fn vmo_entry_by_koid(&self, koid: KoId) -> Option<&VmoEntry> {
        self.vmos.iter().find(|entry| entry.koid == koid)
    }

    fn vmo_entry_by_koid_mut(&mut self, koid: KoId) -> Option<&mut VmoEntry> {
        self.vmos.iter_mut().find(|entry| entry.koid == koid)
    }

    /// Look up a process by koid. Returns `None` if not found.
    pub fn process_by_koid(&self, koid: KoId) -> Option<&Process> {
        self.processes.iter().find(|p| p.koid() == koid)
    }

    /// Look up a process by koid (mutable).
    pub fn process_by_koid_mut(&mut self, koid: KoId) -> Option<&mut Process> {
        self.processes.iter_mut().find(|p| p.koid() == koid)
    }

    /// Look up a thread by koid (mutable).
    pub fn thread_by_koid_mut(&mut self, koid: KoId) -> Option<&mut Thread> {
        self.threads.iter_mut().find(|t| t.koid() == koid)
    }

    /// Create a thread in `target_process`. The thread starts with placeholder context;
    /// [`ThreadStart`] sets the real entry point, stack, and argument.
    pub fn thread_create(
        &mut self,
        caller: &mut Process,
        target_koid: KoId,
    ) -> Result<Handle, ObjectError> {
        // Thread::new only uses process.koid(); extract it before the mutable borrow.
        let proc_koid = {
            let target = self
                .process_by_koid(target_koid)
                .ok_or(ObjectError::BadHandle)?;
            target.koid()
        };
        // Create a minimal Process stub just for the koid. We need &Process for
        // Thread::new's signature, but it only reads .koid(). Use a temporary that
        // stores exactly the koid we extracted.
        // A syscall handler must never panic on behalf of userspace; surface an error
        // instead of unwrapping (the temp Vmar only exists to carry proc_koid).
        let temp_process = Process::from_parts(
            proc_koid,
            crate::mm::Vmar::new(0, crate::mm::PAGE_SIZE).map_err(|_| ObjectError::TableFull)?,
        );
        let thread = Thread::new(
            &mut self.objects,
            &temp_process,
            0,
            0,
            crate::task::DEFAULT_KERNEL_STACK_SIZE,
        )
        .map_err(|_| ObjectError::TableFull)?;
        let handle = caller.handles_mut().insert(
            thread.object(),
            Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::WAIT,
        )?;
        self.threads.push(thread);
        Ok(handle)
    }

    /// Create a child process under `parent_job`. Returns a handle to the new
    /// process inserted into `caller`'s handle table.
    pub fn process_create(
        &mut self,
        caller: &mut Process,
        parent_job: &Job,
        vmar: Vmar,
    ) -> Result<Handle, ObjectError> {
        let job = Job::child(&mut self.objects, parent_job);
        let child = Process::new(&mut self.objects, &job, vmar);
        let handle = caller.handles_mut().insert(
            child.object(),
            Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::WAIT,
        )?;
        self.processes.push(child);
        Ok(handle)
    }

    /// Dispatch a syscall by process koid (avoids borrow conflict when the caller
    /// already holds a reference into `self.processes`).
    pub fn dispatch_by_koid(&mut self, proc_koid: KoId, call: KernelCall<'_>) -> KernelCallResult {
        if let Some(proc) = self.process_by_koid_mut(proc_koid) {
            // Re-borrow: this is safe because we just obtained the mutable ref above.
            let p: &mut Process = unsafe { &mut *(proc as *mut Process) };
            self.dispatch(p, call)
        } else {
            KernelCallResult::Status(Errno::BadHandle.status())
        }
    }

    pub fn dispatch(&mut self, process: &mut Process, call: KernelCall<'_>) -> KernelCallResult {
        match call {
            KernelCall::HandleClose { handle } => {
                let status = match process.handles_mut().remove(handle) {
                    Ok(entry) => {
                        if entry.kind == ObjectKind::Channel {
                            if let Ok(Some(peer_koid)) = self.ipc.close_by_koid(entry.koid) {
                                self.signal_ports(peer_koid, kumo_abi::Signals::PEER_CLOSED);
                            }
                        }
                        Errno::Ok.status()
                    }
                    Err(error) => errno_from_object(error).status(),
                };
                KernelCallResult::Status(status)
            }
            KernelCall::HandleDuplicate { handle, rights } => {
                match process.handles_mut().duplicate(handle, rights) {
                    Ok(handle) => KernelCallResult::Handle(handle),
                    Err(error) => KernelCallResult::Status(errno_from_object(error).status()),
                }
            }
            KernelCall::ChannelCreate => {
                match self.ipc.channel_create(&mut self.objects, process) {
                    Ok((first, second)) => KernelCallResult::Handles { first, second },
                    Err(error) => KernelCallResult::Status(errno_from_ipc(error).status()),
                }
            }
            KernelCall::ChannelWrite { channel, message } => {
                let signal_koid = process
                    .handles()
                    .get(channel)
                    .ok()
                    .and_then(|e| self.ipc.peer_koid_for(e.koid).ok());
                let status = match self.ipc.channel_write(process, channel, message) {
                    Ok(()) => {
                        if let Some(koid) = signal_koid {
                            self.signal_ports(koid, kumo_abi::Signals::READABLE);
                            #[cfg(target_os = "none")]
                            crate::user_thread::wake_child_waiting_on_channel(koid);
                        }
                        Errno::Ok.status()
                    }
                    Err(error) => errno_from_ipc(error).status(),
                };
                KernelCallResult::Status(status)
            }
            KernelCall::ChannelRead { channel } => match self.ipc.channel_read(process, channel) {
                Ok(message) => KernelCallResult::Message(message),
                Err(error) => KernelCallResult::Status(errno_from_ipc(error).status()),
            },
            KernelCall::PortCreate => match self.ipc.port_create(&mut self.objects, process) {
                Ok(handle) => KernelCallResult::Handle(handle),
                Err(error) => KernelCallResult::Status(errno_from_ipc(error).status()),
            },
            KernelCall::PortWait { port } => match self.ipc.port_wait(process, port) {
                Ok(packet) => KernelCallResult::PortPacket(packet),
                Err(error) => KernelCallResult::Status(errno_from_ipc(error).status()),
            },
            KernelCall::VmoRead {
                vmo,
                offset,
                dest,
                len,
            } => {
                let status = match process
                    .handles()
                    .require(vmo, ObjectKind::Vmo, Rights::READ)
                {
                    Ok(handle_entry) => match self.vmo_entry_by_koid(handle_entry.koid) {
                        Some(vmo_entry) => {
                            if offset.checked_add(len as u64).is_none()
                                || offset.saturating_add(len as u64) > vmo_entry.vmo.len()
                            {
                                Errno::InvalidArgs.status()
                            } else {
                                let dest_slice =
                                    unsafe { core::slice::from_raw_parts_mut(dest, len) };
                                match vmo_entry.vmo.backing() {
                                    crate::mm::VmoBacking::Physical { phys_base } => {
                                        kumo_hal::active::read_phys(phys_base + offset, dest_slice);
                                        Errno::Ok.status()
                                    }
                                    crate::mm::VmoBacking::Anonymous => {
                                        let mut copied = 0usize;
                                        while copied < len {
                                            let pos = offset + copied as u64;
                                            let page_index = (pos / crate::mm::PAGE_SIZE) as usize;
                                            let page_off = (pos % crate::mm::PAGE_SIZE) as usize;
                                            let chunk = (len - copied)
                                                .min(crate::mm::PAGE_SIZE as usize - page_off);
                                            if let Some(Some(frame)) =
                                                vmo_entry.frames.get(page_index)
                                            {
                                                kumo_hal::active::read_phys(
                                                    frame + page_off as u64,
                                                    &mut dest_slice[copied..copied + chunk],
                                                );
                                            } else {
                                                dest_slice[copied..copied + chunk].fill(0);
                                            }
                                            copied += chunk;
                                        }
                                        Errno::Ok.status()
                                    }
                                }
                            }
                        }
                        None => Errno::BadHandle.status(),
                    },
                    Err(error) => errno_from_object(error).status(),
                };
                KernelCallResult::Status(status)
            }
            KernelCall::VmoWrite {
                vmo,
                offset,
                src,
                len,
            } => {
                let status = match process
                    .handles()
                    .require(vmo, ObjectKind::Vmo, Rights::WRITE)
                {
                    Ok(handle_entry) => {
                        let Some(vmo_entry) = self.vmo_entry_by_koid(handle_entry.koid) else {
                            return KernelCallResult::Status(Errno::BadHandle.status());
                        };
                        if offset.checked_add(len as u64).is_none()
                            || offset.saturating_add(len as u64) > vmo_entry.vmo.len()
                        {
                            Errno::InvalidArgs.status()
                        } else {
                            let backing = vmo_entry.vmo.backing();
                            let src_slice = unsafe { core::slice::from_raw_parts(src, len) };
                            match backing {
                                crate::mm::VmoBacking::Physical { phys_base } => {
                                    unsafe {
                                        core::ptr::copy_nonoverlapping(
                                            src_slice.as_ptr(),
                                            phys_ptr_mut(phys_base + offset),
                                            len,
                                        );
                                    }
                                    Errno::Ok.status()
                                }
                                crate::mm::VmoBacking::Anonymous => {
                                    let Some(boot) = self.boot_info else {
                                        return KernelCallResult::Status(Errno::Internal.status());
                                    };
                                    let Some(vmo_entry) =
                                        self.vmo_entry_by_koid_mut(handle_entry.koid)
                                    else {
                                        return KernelCallResult::Status(Errno::BadHandle.status());
                                    };
                                    let mut copied = 0usize;
                                    let mut status = Errno::Ok.status();
                                    while copied < len {
                                        let pos = offset + copied as u64;
                                        let page_index = (pos / crate::mm::PAGE_SIZE) as usize;
                                        let page_off = (pos % crate::mm::PAGE_SIZE) as usize;
                                        let chunk = (len - copied)
                                            .min(crate::mm::PAGE_SIZE as usize - page_off);
                                        let frame = match ensure_anonymous_frame(
                                            vmo_entry, page_index, &boot,
                                        ) {
                                            Ok(frame) => frame,
                                            Err(errno) => {
                                                status = errno.status();
                                                break;
                                            }
                                        };
                                        unsafe {
                                            core::ptr::copy_nonoverlapping(
                                                src_slice[copied..copied + chunk].as_ptr(),
                                                phys_ptr_mut(frame + page_off as u64),
                                                chunk,
                                            );
                                        }
                                        copied += chunk;
                                    }
                                    status
                                }
                            }
                        }
                    }
                    Err(error) => errno_from_object(error).status(),
                };
                KernelCallResult::Status(status)
            }
            KernelCall::ProcessCreate {
                parent_job,
                vmar_base,
                vmar_size,
            } => match Vmar::new(vmar_base, vmar_size) {
                Ok(vmar) => match self.process_create(process, &parent_job, vmar) {
                    Ok(handle) => KernelCallResult::Handle(handle),
                    Err(error) => KernelCallResult::Status(errno_from_object(error).status()),
                },
                Err(_) => KernelCallResult::Status(Errno::InvalidArgs.status()),
            },
            KernelCall::VmarMap {
                process_handle,
                vmo_handle,
                vmo_offset,
                virt,
                len,
                flags,
            } => {
                // Look up the target process koid and VMO before mutably borrowing self.
                let proc_koid = match process.handles().require(
                    process_handle,
                    ObjectKind::Process,
                    Rights::WRITE,
                ) {
                    Ok(entry) => entry.koid,
                    Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                };
                let (vmo_koid, vmo) =
                    match process
                        .handles()
                        .require(vmo_handle, ObjectKind::Vmo, Rights::READ)
                    {
                        Ok(entry) => (entry.koid, self.vmo_by_koid(entry.koid)),
                        Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                    };
                let Some(vmo) = vmo else {
                    return KernelCallResult::Status(Errno::BadHandle.status());
                };
                let Some(target) = self.process_by_koid_mut(proc_koid) else {
                    return KernelCallResult::Status(Errno::BadHandle.status());
                };
                let status = match target.root_vmar().map(vmo, vmo_offset, virt, len, flags) {
                    Ok(mapping) => {
                        target.add_mapping(mapping, vmo_koid);
                        Errno::Ok.status()
                    }
                    Err(_) => Errno::InvalidArgs.status(),
                };
                KernelCallResult::Status(status)
            }
            KernelCall::ThreadCreate { process_handle } => {
                let proc_koid = match process.handles().require(
                    process_handle,
                    ObjectKind::Process,
                    Rights::WRITE,
                ) {
                    Ok(entry) => entry.koid,
                    Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                };
                match self.thread_create(process, proc_koid) {
                    Ok(handle) => KernelCallResult::Handle(handle),
                    Err(e) => KernelCallResult::Status(errno_from_object(e).status()),
                }
            }
            KernelCall::ThreadStart {
                thread_handle,
                entry,
                sp,
                arg,
            } => {
                let thread_koid = match process.handles().require(
                    thread_handle,
                    ObjectKind::Thread,
                    Rights::WRITE,
                ) {
                    Ok(e) => e.koid,
                    Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                };
                // Two-pass: first extract the process koid (mutable borrow), then
                // look up the process's ttbr0 (immutable borrow).
                let proc_koid = {
                    let Some(thread) = self.thread_by_koid_mut(thread_koid) else {
                        return KernelCallResult::Status(Errno::BadHandle.status());
                    };
                    thread.process()
                };
                let proc_ttbr0 = self.process_by_koid(proc_koid).and_then(|p| p.ttbr0);
                let Some(thread) = self.thread_by_koid_mut(thread_koid) else {
                    return KernelCallResult::Status(Errno::BadHandle.status());
                };
                #[cfg(target_os = "none")]
                if let Some(ttbr0) = proc_ttbr0 {
                    // P8-k: user-mode thread. Build a UserState and create a context
                    // that enters via kumo_user_enter.
                    let mut user_state = kumo_hal::active::UserState {
                        x: [0u64; 31],
                        elr: entry,
                        spsr: 0,
                        sp_el0: sp,
                        ttbr0,
                    };
                    user_state.x[0] = arg; // bootstrap arg
                    let kernel_sp = thread.stack().top();
                    extern "C" {
                        fn kumo_user_enter();
                    }
                    let mut ctx = kumo_hal::active::ThreadContext::default();
                    unsafe {
                        let raw = &mut ctx as *mut kumo_hal::active::ThreadContext as *mut u64;
                        *raw = &user_state as *const kumo_hal::active::UserState as *const ()
                            as usize as u64; // x19_entry
                        *raw.add(11) = kumo_user_enter as *const () as usize as u64; // x30_lr
                        *raw.add(12) = kernel_sp as u64; // sp
                        *raw.add(13) = 1; // user = true
                    }
                    thread.user_state = Some(user_state);
                    *thread.context_mut() = ctx;
                }
                #[cfg(not(target_os = "none"))]
                {
                    // Kernel thread (backward-compatible scaffold).
                    *thread.context_mut() = kumo_hal::active::ThreadContext::new(
                        entry as usize,
                        arg as usize,
                        sp as usize,
                        false,
                    );
                }
                #[cfg(target_os = "none")]
                if proc_ttbr0.is_none() {
                    // Kernel thread fallback when no TTBR0 is set.
                    *thread.context_mut() = kumo_hal::active::ThreadContext::new(
                        entry as usize,
                        arg as usize,
                        sp as usize,
                        false,
                    );
                }
                thread.ready();
                KernelCallResult::Status(Errno::Ok.status())
            }
            KernelCall::AddressSpaceCreate {
                process_handle,
                stack_virt,
                stack_size,
            } => {
                let proc_koid = match process.handles().require(
                    process_handle,
                    ObjectKind::Process,
                    Rights::WRITE,
                ) {
                    Ok(e) => e.koid,
                    Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                };
                // Extract mappings and boot_info before mutably borrowing self.
                let boot = self.boot_info;
                let Some(ref boot_ref) = boot else {
                    return KernelCallResult::Status(Errno::Internal.status());
                };
                let process_mappings: Vec<(Mapping, KoId)> = {
                    let Some(target) = self.process_by_koid(proc_koid) else {
                        return KernelCallResult::Status(Errno::BadHandle.status());
                    };
                    target.mappings().to_vec()
                };
                let mut user_mappings = Vec::new();
                for (mapping, vmo_koid) in process_mappings {
                    let executable = mapping.flags.contains(PageFlags::EXECUTE);
                    // W^X: an executable mapping is RX, never writable.
                    let writable = mapping.flags.contains(PageFlags::WRITE) && !executable;
                    let device = mapping.flags.contains(PageFlags::DEVICE);
                    let Some(vmo_entry) = self.vmo_entry_by_koid(vmo_koid) else {
                        return KernelCallResult::Status(Errno::BadHandle.status());
                    };
                    match vmo_entry.vmo.backing() {
                        crate::mm::VmoBacking::Physical { phys_base } => {
                            // Executable code maps at its exact page-aligned VA (4 KiB pages);
                            // device/framebuffer mappings ride a 2 MiB-aligned block slot.
                            let virt_addr = if executable {
                                mapping.virt
                            } else {
                                const BLOCK_MASK: u64 = (1 << 21) - 1;
                                mapping.virt & !BLOCK_MASK
                            };
                            user_mappings.push(kumo_hal::active::UserMapping {
                                phys_base: phys_base + mapping.vmo_offset,
                                virt_addr,
                                len: mapping.len,
                                writable,
                                device,
                                executable,
                            });
                        }
                        crate::mm::VmoBacking::Anonymous => {
                            if mapping.len % crate::mm::PAGE_SIZE != 0 {
                                return KernelCallResult::Status(Errno::InvalidArgs.status());
                            }
                            let pages = mapping.len / crate::mm::PAGE_SIZE;
                            let mut page = 0;
                            while page < pages {
                                let off = page * crate::mm::PAGE_SIZE;
                                let vmo_page =
                                    ((mapping.vmo_offset + off) / crate::mm::PAGE_SIZE) as usize;
                                let Some(vmo_entry) = self.vmo_entry_by_koid_mut(vmo_koid) else {
                                    return KernelCallResult::Status(Errno::BadHandle.status());
                                };
                                let phys_base =
                                    match ensure_anonymous_frame(vmo_entry, vmo_page, boot_ref) {
                                        Ok(frame) => frame,
                                        Err(errno) => {
                                            return KernelCallResult::Status(errno.status());
                                        }
                                    };
                                user_mappings.push(kumo_hal::active::UserMapping {
                                    phys_base,
                                    virt_addr: mapping.virt + off,
                                    len: crate::mm::PAGE_SIZE,
                                    writable,
                                    device: false,
                                    executable,
                                });
                                page += 1;
                            }
                        }
                    }
                }
                let image = kumo_hal::active::UserImage {
                    entry: 0,
                    stack_top: stack_virt,
                    stack_size,
                    bootstrap: 0,
                    segments: &[],
                    extra_mappings: &user_mappings,
                };
                let mut alloc = || unsafe { crate::mm::alloc_zeroed_frame(boot_ref) };
                // `build_user_tables` writes page-table frames and flushes code pages by raw
                // physical address, which assume the kernel identity map. But this syscall runs
                // on the user thread with the caller's (Sora's) tree live in TTBR0. Switch to the
                // kernel identity map for the build, then restore the caller's tree so the
                // eventual `eret` returns to EL0 correctly.
                #[cfg(target_os = "none")]
                let result = {
                    let saved_ttbr0 = kumo_hal::active::read_user_aspace_root();
                    unsafe {
                        kumo_hal::active::set_user_aspace_root(crate::user_thread::kernel_ttbr0())
                    };
                    let r = kumo_hal::active::build_user_tables(&image, &mut alloc);
                    unsafe { kumo_hal::active::set_user_aspace_root(saved_ttbr0) };
                    r
                };
                #[cfg(not(target_os = "none"))]
                let result = kumo_hal::active::build_user_tables(&image, &mut alloc);
                match result {
                    Ok(ttbr0) => {
                        if let Some(target) = self.process_by_koid_mut(proc_koid) {
                            target.ttbr0 = Some(ttbr0);
                        }
                        KernelCallResult::Handle(Handle(ttbr0 as u32))
                    }
                    Err(_) => KernelCallResult::Status(Errno::InvalidArgs.status()),
                }
            }
            KernelCall::ProcessRun {
                process_handle,
                entry,
                sp,
                arg,
                arg2,
                flags,
            } => {
                #[cfg(target_os = "none")]
                {
                    let proc_koid = match process.handles().require(
                        process_handle,
                        ObjectKind::Process,
                        Rights::WRITE,
                    ) {
                        Ok(e) => e.koid,
                        Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                    };
                    let Some(target) = self.process_by_koid(proc_koid) else {
                        return KernelCallResult::Status(Errno::BadHandle.status());
                    };
                    let Some(ttbr0) = target.ttbr0 else {
                        return KernelCallResult::Status(Errno::InvalidArgs.status());
                    };
                    let root_vmar = target.root_vmar();
                    // Duplicate bootstrap handles from the caller's table
                    // into the child's handle namespace.
                    let dup = |a: u64| -> u64 {
                        if a != 0 {
                            process
                                .handles()
                                .get(Handle(a as u32))
                                .ok()
                                .and_then(|entry| {
                                    self.process_by_koid(proc_koid)
                                        .map(|cp| cp as *const Process as *mut Process)
                                        .and_then(|child_ptr| unsafe {
                                            (*child_ptr).handles_mut().insert_entry(entry).ok()
                                        })
                                })
                                .map(|h| h.0 as u64)
                                .unwrap_or(a)
                        } else {
                            a
                        }
                    };
                    let child_arg = dup(arg);
                    let child_arg2 = dup(arg2);
                    // P10-g: flags & 1 = async (non-blocking, low-priority child).
                    let status = if flags & 1 != 0 {
                        crate::user_thread::spawn_child_async(
                            &mut self.objects,
                            proc_koid,
                            root_vmar,
                            ttbr0,
                            entry,
                            sp,
                            child_arg,
                            child_arg2,
                        )
                    } else {
                        crate::user_thread::run_child(
                            &mut self.objects,
                            proc_koid,
                            root_vmar,
                            ttbr0,
                            entry,
                            sp,
                            child_arg,
                            child_arg2,
                        )
                    };
                    KernelCallResult::Status(status)
                }
                #[cfg(not(target_os = "none"))]
                {
                    let _ = (process_handle, entry, sp, arg, arg2, flags);
                    KernelCallResult::Status(Errno::NotSupported.status())
                }
            }
            KernelCall::ProcessWait => KernelCallResult::Status(crate::user_thread::process_wait()),
            KernelCall::InterruptCreate { irq } => {
                let object = self.objects.create(ObjectKind::Interrupt);
                let handle = match process
                    .handles_mut()
                    .insert(object, Rights::READ | Rights::WAIT | Rights::DUPLICATE)
                {
                    Ok(h) => h,
                    Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                };
                self.interrupts.push(IrqBinding {
                    irq,
                    koid: object.koid(),
                    count: 0,
                });
                KernelCallResult::Handle(handle)
            }
            KernelCall::InterruptWait { interrupt } => {
                let entry =
                    match process
                        .handles()
                        .require(interrupt, ObjectKind::Interrupt, Rights::WAIT)
                    {
                        Ok(e) => e,
                        Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                    };
                // Find the binding and check fire count. If > 0, return the count.
                // If 0, return ShouldWait — the caller parks and the IRQ handler
                // will wake it.
                for binding in &mut self.interrupts {
                    if binding.koid == entry.koid {
                        if binding.count > 0 {
                            let n = binding.count;
                            binding.count = 0;
                            return KernelCallResult::Handle(Handle(n as u32));
                        }
                        return KernelCallResult::Status(Errno::ShouldWait.status());
                    }
                }
                KernelCallResult::Status(Errno::BadHandle.status())
            }
            KernelCall::ResourceMintMmio {
                resource,
                phys_base,
                len,
            } => {
                let res_entry =
                    match process
                        .handles()
                        .require(resource, ObjectKind::Resource, Rights::WRITE)
                    {
                        Ok(e) => e,
                        Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                    };
                let Some(res) = self.resource_by_koid(res_entry.koid) else {
                    return KernelCallResult::Status(Errno::BadHandle.status());
                };
                // Check the requested range is within the Resource grant.
                let end = match phys_base.checked_add(len) {
                    Some(e) => e,
                    None => return KernelCallResult::Status(Errno::InvalidArgs.status()),
                };
                let res_end = match res.phys_base.checked_add(res.len) {
                    Some(e) => e,
                    None => return KernelCallResult::Status(Errno::InvalidArgs.status()),
                };
                if phys_base < res.phys_base || end > res_end {
                    return KernelCallResult::Status(Errno::AccessDenied.status());
                }
                // Create a Physical VMO from the MMIO range.
                match crate::mm::Vmo::from_physical_range(phys_base, len) {
                    Ok(vmo) => match self.root_vmo_create(
                        process,
                        vmo,
                        Rights::READ | Rights::WRITE | Rights::DUPLICATE,
                    ) {
                        Ok(handle) => KernelCallResult::Handle(handle),
                        Err(e) => KernelCallResult::Status(errno_from_object(e).status()),
                    },
                    Err(_) => KernelCallResult::Status(Errno::InvalidArgs.status()),
                }
            }
            KernelCall::PortBind { port, object } => {
                let port_koid =
                    match process
                        .handles()
                        .require(port, ObjectKind::Port, Rights::WAIT)
                    {
                        Ok(e) => e.koid,
                        Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                    };
                let obj_koid = match process.handles().get(object) {
                    Ok(e) => {
                        if !e.rights.contains(Rights::WAIT) {
                            return KernelCallResult::Status(Errno::AccessDenied.status());
                        }
                        e.koid
                    }
                    Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                };
                self.port_bind(port_koid, obj_koid);
                KernelCallResult::Status(Errno::Ok.status())
            }
            KernelCall::HandleKoid { handle } => match process.handles().get(handle) {
                Ok(entry) => KernelCallResult::Handle(Handle(entry.koid.0 as u32)),
                Err(e) => KernelCallResult::Status(errno_from_object(e).status()),
            },
            KernelCall::VmoCreate { size } => match crate::mm::Vmo::new(size) {
                Ok(vmo) => match self.root_vmo_create(
                    process,
                    vmo,
                    Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER,
                ) {
                    Ok(handle) => KernelCallResult::Handle(handle),
                    Err(e) => KernelCallResult::Status(errno_from_object(e).status()),
                },
                Err(_) => KernelCallResult::Status(Errno::InvalidArgs.status()),
            },
            KernelCall::Unsupported(_) => KernelCallResult::Status(Errno::NotSupported.status()),
        }
    }

    pub fn port_queue_signal(
        &mut self,
        process: &Process,
        port: Handle,
        source: kumo_abi::KoId,
        signals: kumo_abi::Signals,
    ) -> Status {
        match self.ipc.port_queue_signal(process, port, source, signals) {
            Ok(()) => Errno::Ok.status(),
            Err(error) => errno_from_ipc(error).status(),
        }
    }

    pub fn channel_call<F>(
        &mut self,
        process: &mut Process,
        channel: Handle,
        request: Message<'_>,
        server: F,
    ) -> Result<KernelMessage, Errno>
    where
        F: FnOnce(KernelMessage) -> Result<KernelMessage, IpcError>,
    {
        self.ipc
            .channel_call(process, channel, request, server)
            .map_err(errno_from_ipc)
    }
}

pub const fn errno_from_ipc(error: IpcError) -> Errno {
    match error {
        IpcError::BadHandle | IpcError::NotChannel => Errno::BadHandle,
        IpcError::WrongType => Errno::WrongType,
        IpcError::AccessDenied => Errno::AccessDenied,
        IpcError::TableFull => Errno::NoMemory,
        IpcError::TooManyBytes | IpcError::TooManyHandles => Errno::InvalidArgs,
        IpcError::ShouldWait => Errno::ShouldWait,
        IpcError::PeerClosed => Errno::PeerClosed,
    }
}

pub const fn errno_from_object(error: ObjectError) -> Errno {
    match error {
        ObjectError::BadHandle => Errno::BadHandle,
        ObjectError::WrongType => Errno::WrongType,
        ObjectError::AccessDenied => Errno::AccessDenied,
        ObjectError::TableFull => Errno::NoMemory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kumo_abi::{ObjectKind, Rights};

    fn test_process(engine: &mut SyscallEngine) -> Process {
        let job = crate::task::Job::root(engine.objects_mut());
        let vmar = crate::mm::Vmar::new(0xffff_0000_0000_0000, crate::mm::PAGE_SIZE * 16).unwrap();
        Process::new(engine.objects_mut(), &job, vmar)
    }

    fn create_channel(engine: &mut SyscallEngine, process: &mut Process) -> (Handle, Handle) {
        match engine.dispatch(process, KernelCall::ChannelCreate) {
            KernelCallResult::Handles { first, second } => (first, second),
            other => panic!("expected channel handles, got {other:?}"),
        }
    }

    fn create_port(engine: &mut SyscallEngine, process: &mut Process) -> Handle {
        match engine.dispatch(process, KernelCall::PortCreate) {
            KernelCallResult::Handle(handle) => handle,
            other => panic!("expected port handle, got {other:?}"),
        }
    }

    #[test]
    fn maps_ipc_errors_to_abi_errno() {
        assert_eq!(errno_from_ipc(IpcError::BadHandle), Errno::BadHandle);
        assert_eq!(errno_from_ipc(IpcError::WrongType), Errno::WrongType);
        assert_eq!(errno_from_ipc(IpcError::AccessDenied), Errno::AccessDenied);
        assert_eq!(errno_from_ipc(IpcError::TableFull), Errno::NoMemory);
        assert_eq!(errno_from_ipc(IpcError::TooManyBytes), Errno::InvalidArgs);
        assert_eq!(errno_from_ipc(IpcError::TooManyHandles), Errno::InvalidArgs);
        assert_eq!(errno_from_ipc(IpcError::ShouldWait), Errno::ShouldWait);
        assert_eq!(errno_from_ipc(IpcError::PeerClosed), Errno::PeerClosed);
        assert_eq!(errno_from_ipc(IpcError::NotChannel), Errno::BadHandle);
    }

    #[test]
    fn maps_object_errors_to_abi_errno() {
        assert_eq!(errno_from_object(ObjectError::BadHandle), Errno::BadHandle);
        assert_eq!(errno_from_object(ObjectError::WrongType), Errno::WrongType);
        assert_eq!(
            errno_from_object(ObjectError::AccessDenied),
            Errno::AccessDenied
        );
        assert_eq!(errno_from_object(ObjectError::TableFull), Errno::NoMemory);
    }

    #[test]
    fn dispatches_handle_duplicate_and_close() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let (left, _right) = create_channel(&mut engine, &mut process);

        let duplicated = engine.dispatch(
            &mut process,
            KernelCall::HandleDuplicate {
                handle: left,
                rights: Rights::READ,
            },
        );
        let KernelCallResult::Handle(read_only) = duplicated else {
            panic!("expected duplicated handle");
        };
        assert!(process
            .handles()
            .require(read_only, ObjectKind::Channel, Rights::READ)
            .is_ok());
        assert_eq!(
            process
                .handles()
                .require(read_only, ObjectKind::Channel, Rights::WRITE),
            Err(ObjectError::AccessDenied)
        );

        let closed = engine.dispatch(&mut process, KernelCall::HandleClose { handle: read_only });
        assert_eq!(closed, KernelCallResult::Status(Errno::Ok.status()));
        assert_eq!(
            process.handles().get(read_only),
            Err(ObjectError::BadHandle)
        );
    }

    #[test]
    fn duplicate_cannot_widen_rights() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let (left, _right) = create_channel(&mut engine, &mut process);
        let read_only = process.handles_mut().duplicate(left, Rights::READ).unwrap();

        let widened = engine.dispatch(
            &mut process,
            KernelCall::HandleDuplicate {
                handle: read_only,
                rights: Rights::READ | Rights::WRITE,
            },
        );

        assert_eq!(
            widened,
            KernelCallResult::Status(Errno::AccessDenied.status())
        );
    }

    #[test]
    fn dispatches_channel_create_write_and_read() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let (left, right) = create_channel(&mut engine, &mut process);

        let write = engine.dispatch(
            &mut process,
            KernelCall::ChannelWrite {
                channel: left,
                message: Message::new(4, b"hello", &[]).unwrap(),
            },
        );
        assert_eq!(write, KernelCallResult::Status(Errno::Ok.status()));

        let read = engine.dispatch(&mut process, KernelCall::ChannelRead { channel: right });
        let KernelCallResult::Message(message) = read else {
            panic!("expected message");
        };
        assert_eq!(message.header.ordinal, 4);
        assert_eq!(message.bytes(), b"hello");
    }

    #[test]
    fn channel_write_signals_peer_endpoint_port() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let peer_port = create_port(&mut engine, &mut process);
        let writer_port = create_port(&mut engine, &mut process);
        let (left, right) = create_channel(&mut engine, &mut process);

        let bind = engine.dispatch(
            &mut process,
            KernelCall::PortBind {
                port: peer_port,
                object: right,
            },
        );
        assert_eq!(bind, KernelCallResult::Status(Errno::Ok.status()));

        let bind_writer = engine.dispatch(
            &mut process,
            KernelCall::PortBind {
                port: writer_port,
                object: left,
            },
        );
        assert_eq!(bind_writer, KernelCallResult::Status(Errno::Ok.status()));

        let write = engine.dispatch(
            &mut process,
            KernelCall::ChannelWrite {
                channel: left,
                message: Message::new(4, b"x", &[]).unwrap(),
            },
        );
        assert_eq!(write, KernelCallResult::Status(Errno::Ok.status()));

        assert_eq!(
            engine.dispatch(&mut process, KernelCall::PortWait { port: writer_port }),
            KernelCallResult::Status(Errno::ShouldWait.status())
        );

        let result = engine.dispatch(&mut process, KernelCall::PortWait { port: peer_port });
        let KernelCallResult::PortPacket(packet) = result else {
            panic!("expected peer endpoint port packet");
        };
        let right_koid = process.handles().get(right).unwrap().koid;
        assert_eq!(packet.source, right_koid);
        assert!(packet.signals.contains(kumo_abi::Signals::READABLE));
    }

    #[test]
    fn dispatch_reports_should_wait_on_empty_channel() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let (_left, right) = create_channel(&mut engine, &mut process);

        let read = engine.dispatch(&mut process, KernelCall::ChannelRead { channel: right });

        assert_eq!(read, KernelCallResult::Status(Errno::ShouldWait.status()));
    }

    #[test]
    fn dispatch_preserves_transfer_semantics() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let (left, right) = create_channel(&mut engine, &mut process);
        let event = engine.objects_mut().create(ObjectKind::Event);
        let event_handle = process
            .handles_mut()
            .insert(event, Rights::WAIT | Rights::TRANSFER)
            .unwrap();
        let transfers = [event_handle];

        let write = engine.dispatch(
            &mut process,
            KernelCall::ChannelWrite {
                channel: left,
                message: Message::new(5, b"event", &transfers).unwrap(),
            },
        );
        assert_eq!(write, KernelCallResult::Status(Errno::Ok.status()));
        assert_eq!(
            process.handles().get(event_handle),
            Err(crate::object::ObjectError::BadHandle)
        );

        let read = engine.dispatch(&mut process, KernelCall::ChannelRead { channel: right });
        let KernelCallResult::Message(message) = read else {
            panic!("expected message with transferred handle");
        };
        let received_handle = message.handles()[0];
        assert_ne!(received_handle, event_handle);
        assert!(process
            .handles()
            .require(received_handle, ObjectKind::Event, Rights::WAIT)
            .is_ok());
    }

    #[test]
    fn channel_call_facade_returns_server_reply() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let (left, _right) = create_channel(&mut engine, &mut process);

        let reply = engine
            .channel_call(
                &mut process,
                left,
                Message::call(7, b"ping", &[]).unwrap(),
                |request| {
                    assert_eq!(request.bytes(), b"ping");
                    KernelMessage::reply_to(&request, b"pong", &[])
                },
            )
            .unwrap();

        assert_eq!(reply.bytes(), b"pong");
    }

    #[test]
    fn dispatches_port_create_and_wait() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let port = create_port(&mut engine, &mut process);

        assert_eq!(
            engine.dispatch(&mut process, KernelCall::PortWait { port }),
            KernelCallResult::Status(Errno::ShouldWait.status())
        );

        let source = kumo_abi::KoId(100);
        let status = engine.port_queue_signal(&process, port, source, kumo_abi::Signals::READABLE);
        assert_eq!(status, Errno::Ok.status());

        let result = engine.dispatch(&mut process, KernelCall::PortWait { port });
        let KernelCallResult::PortPacket(packet) = result else {
            panic!("expected port packet");
        };
        assert_eq!(packet.source, source);
        assert!(packet.signals.contains(kumo_abi::Signals::READABLE));
    }

    #[test]
    fn unsupported_syscalls_return_not_supported() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);

        let result = engine.dispatch(
            &mut process,
            KernelCall::Unsupported(Syscall::ProcessCreate),
        );

        assert_eq!(
            result,
            KernelCallResult::Status(Errno::NotSupported.status())
        );
    }
}
