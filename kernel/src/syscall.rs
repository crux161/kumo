//j628
//j629
//j630
//j631
//j632
use alloc::{boxed::Box, rc::Rc, vec::Vec};
use core::cell::{Cell, RefCell};
use core::mem;

#[cfg(target_os = "none")]
use kumo_abi::ProcessRunFlags;
use kumo_abi::{
    decode_tlmm_gpio_irq, interrupt_authority_key, sys::DeviceCtxInfo, BootInfo, DevicePowerStatus,
    Errno, Handle, KoId, ObjectKind, Rights, Status, Syscall,
};
use kumo_hal::active::{
    DevicePowerError, DevicePowerReadout, DevicePowerUnitInfo, DevicePowerUpOutcome,
};
use kumo_hal::PageFlags;
use kumo_ipc::Message;

use crate::ipc::{IpcError, IpcRegistry, KernelMessage, PortPacket};
use crate::mm::{Mapping, Vmar, Vmo};
use crate::object::{HandleReferences, HandleTable, ObjectError, ObjectManager, StagedHandleGrant};
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
    TimerCreate {
        deadline_ns: u64,
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
        /// See [`kumo_abi::ProcessRunFlags`].
        flags: u64,
    },
    /// P10-g: block until the async child process exits.
    ProcessWait,
    InterruptCreate {
        resource: Handle,
        irq: u32,
    },
    InterruptWait {
        interrupt: Handle,
    },
    InterruptComplete {
        interrupt: Handle,
    },
    ResourceMintMmio {
        resource: Handle,
        phys_base: u64,
        len: u64,
    },
    FramebufferClaim {
        resource: Handle,
        phys_base: u64,
        len: u64,
    },
    ResourceCreateChild {
        parent: Handle,
        phys_base: u64,
        len: u64,
        irq_base: u32,
        irq_count: u32,
    },
    PortBind {
        port: Handle,
        object: Handle,
    },
    PortUnbind {
        port: Handle,
        object: Handle,
    },
    HandleKoid {
        handle: Handle,
    },
    VmoCreate {
        size: u64,
    },
    VmoCreateContiguous {
        size: u64,
    },
    IoMmuFromResource {
        resource: Handle,
        kind: u32,
        phys_base: u64,
        len: u64,
    },
    DeviceCtxActivate {
        ctx: Handle,
    },
    DeviceCtxCreate {
        iommu: Handle,
        stream_or_rid: u64,
    },
    DeviceVmarMap {
        ctx: Handle,
        vmo: Handle,
        vmo_offset: u64,
        len: u64,
        iova_hint: u64,
        rights: Rights,
    },
    DeviceVmarUnmap {
        ctx: Handle,
        iova: u64,
        len: u64,
    },
    DeviceCtxWaitFault {
        ctx: Handle,
    },
    DeviceCtxInfo {
        ctx: Handle,
        user_ptr: u64,
        user_len: u64,
    },
    /// DESIGN/020: raise the power domain of the device at `aperture_base`.
    DevicePowerUp {
        resource: Handle,
        aperture_base: u64,
    },
    /// DESIGN/020: copy the device's [`DevicePowerStatus`] to `user_ptr`.
    DevicePowerStatus {
        resource: Handle,
        aperture_base: u64,
        user_ptr: u64,
        user_len: u64,
    },
    DeviceRegisterWrite {
        resource: Handle,
        aperture_base: u64,
        index: u64,
        value: u64,
    },
    DeviceResetSet {
        resource: Handle,
        aperture_base: u64,
        line: u64,
        asserted: u64,
    },
    Unsupported(Syscall),
}

/// Stage one `ProcessRun` argument. Plain integers pass through unchanged;
/// values naming handles are copied unless their transfer flag is set.
pub(crate) fn stage_process_arg(
    source: &HandleTable,
    target: &mut HandleTable,
    raw: u64,
    transfer: bool,
) -> Result<(u64, Option<StagedHandleGrant>), ObjectError> {
    if raw == 0 {
        return Ok((0, None));
    }
    let handle = Handle(raw as u32);
    if !transfer && matches!(source.get(handle), Err(ObjectError::BadHandle)) {
        return Ok((raw, None));
    }
    let grant = source.stage_grant_to(target, handle, transfer)?;
    Ok((grant.target().0 as u64, Some(grant)))
}

pub(crate) fn rollback_process_grants(
    target: &mut HandleTable,
    grants: &[Option<StagedHandleGrant>],
) -> Result<(), ObjectError> {
    for grant in grants.iter().flatten().rev() {
        target.rollback_grant(*grant)?;
    }
    Ok(())
}

pub(crate) fn commit_process_grants(
    source: &mut HandleTable,
    grants: &[Option<StagedHandleGrant>],
) -> Result<(), ObjectError> {
    for grant in grants.iter().flatten() {
        source.commit_grant(*grant)?;
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KernelCallResult {
    Status(Status),
    Handles {
        first: Handle,
        second: Handle,
    },
    Handle(Handle),
    /// A new handle plus a scalar returned in the secondary register — used by
    /// `VmoCreateContiguous` to report the DMA VMO handle and its physical base together.
    HandleAndValue {
        handle: Handle,
        value: u64,
    },
    /// A status plus a scalar in the secondary register — used by `VmarMap` to report the address
    /// the kernel chose when the caller passed `virt == 0`.
    StatusAndValue {
        status: Status,
        value: u64,
    },
    Message(KernelMessage),
    PortPacket(PortPacket),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IrqState {
    Bound,
    MaskFailed,
    Released,
}

/// One source reservation; only capability owners keep it live. — KESTREL
#[derive(Clone, Debug)]
struct IrqBinding {
    references: HandleReferences,
    state: Rc<Cell<IrqState>>,
    irq: u32,
    koid: KoId,
    count: u64,
    requires_complete: bool,
    outstanding: bool,
}

impl IrqBinding {
    fn complete(&mut self, complete: impl FnOnce(u32) -> Result<(), Errno>) -> Result<(), Errno> {
        // Plain SPIs must recheck the kernel source gate even without an outstanding
        // notification: an early delivery may have masked the line before binding.
        // GPIO completion without a delivery remains a no-op. — KESTREL
        if !self.outstanding && self.requires_complete {
            return Ok(());
        }
        complete(self.irq)?;
        self.outstanding = false;
        Ok(())
    }
}

/// A one-shot monotonic timer. `fired` retains the level after its deadline so a
/// port bound just after expiry still observes the signal instead of losing a race.
#[derive(Clone, Copy, Debug)]
struct TimerBinding {
    koid: KoId,
    deadline_ns: u64,
    fired: bool,
}

#[derive(Clone, Copy, Debug)]
struct IoMmuBinding {
    koid: KoId,
    iommu_kind: u32,
    phys_base: u64,
    len: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeviceCtxState {
    Bound,
    Quarantined,
    Released,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DeviceCtxRetirement {
    pub released: usize,
    pub quarantined: usize,
}

#[derive(Clone, Debug)]
struct DeviceCtxBinding {
    koid: KoId,
    iommu_koid: KoId,
    stream_id: u32,
    pgd_phys: u64,
    references: HandleReferences,
    state: Rc<Cell<DeviceCtxState>>,
}

#[cfg(feature = "arch_aarch64")]
#[derive(Default)]
struct DisplayFaults {
    latched: Cell<bool>,
    slots: [Cell<Option<DeviceCtxInfo>>; 2],
}

#[cfg(feature = "arch_aarch64")]
#[derive(Clone)]
struct DisplayContext {
    koid: KoId,
    faults: Rc<DisplayFaults>,
    epoch: Rc<Cell<u64>>,
    published: Rc<
        RefCell<Option<crate::device_memory::PublishedMappings<crate::device_memory::KernelPages>>>,
    >,
    mappings: Rc<
        RefCell<Option<crate::device_memory::OfflineMappings<crate::device_memory::KernelPages>>>,
    >,
}
#[cfg(feature = "arch_aarch64")]
impl core::fmt::Debug for DisplayContext {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DisplayContext")
            .field("koid", &self.koid)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
struct DeviceVmarMapping {
    ctx_koid: KoId,
    iova: u64,
    len: u64,
    vmo_offset: u64,
    backing: Rc<VmoEntry>,
}

#[derive(Clone, Copy, Debug)]
struct DeviceCtxFault {
    ctx_koid: KoId,
    info: DeviceCtxInfo,
}

/// A Resource grant: the holder may mint VMOs from this MMIO range and create
/// Interrupt objects for IRQs in `[irq_base, irq_base + irq_count)`.
#[derive(Clone, Copy, Debug)]
struct ResourceBinding {
    koid: KoId,
    phys_base: u64,
    len: u64,
    irq_base: u32,
    irq_count: u32,
}

fn resource_contains(parent: ResourceBinding, phys_base: u64, len: u64) -> bool {
    let Some(end) = phys_base.checked_add(len) else {
        return false;
    };
    let Some(parent_end) = parent.phys_base.checked_add(parent.len) else {
        return false;
    };
    phys_base >= parent.phys_base && end <= parent_end
}

/// True when `[irq_base, irq_base + irq_count)` is wholly inside the parent's IRQ
/// window. Widened in `u64` so `irq_count == u32::MAX` (the root grant) cannot wrap.
fn resource_contains_irq_window(parent: ResourceBinding, irq_base: u32, irq_count: u32) -> bool {
    let start = irq_base as u64;
    let end = start + irq_count as u64;
    let parent_start = parent.irq_base as u64;
    let parent_end = parent_start + parent.irq_count as u64;
    start >= parent_start && end <= parent_end
}

/// True when the single IRQ `irq` falls within the resource's IRQ window.
fn resource_contains_irq(parent: ResourceBinding, irq: u32) -> bool {
    let irq = interrupt_authority_key(irq) as u64;
    let parent_start = parent.irq_base as u64;
    let parent_end = parent_start + parent.irq_count as u64;
    irq >= parent_start && irq < parent_end
}

/// The ABI record for a HAL power readout (DESIGN/020 §6).
fn device_power_status_record(readout: &DevicePowerReadout) -> DevicePowerStatus {
    let flags = [
        (readout.powered, DevicePowerStatus::POWERED),
        (readout.repair_done, DevicePowerStatus::REPAIR_DONE),
        (readout.bus_idle, DevicePowerStatus::BUS_IDLE),
        (
            readout.bus_idle_requested,
            DevicePowerStatus::BUS_IDLE_REQUESTED,
        ),
        (
            readout.clock_at_boot_rate,
            DevicePowerStatus::CLOCK_AT_BOOT_RATE,
        ),
        (readout.aperture_ready, DevicePowerStatus::APERTURE_MINTABLE),
    ]
    .into_iter()
    .filter(|(set, _)| *set)
    .fold(0, |flags, (_, flag)| flags | flag);
    DevicePowerStatus {
        version: DevicePowerStatus::VERSION,
        flags,
        pmu: readout.pmu,
        cru: readout.cru,
    }
}

fn ranges_overlap(a_start: u64, a_len: u64, b_start: u64, b_len: u64) -> bool {
    let Some(a_end) = a_start.checked_add(a_len) else {
        return true;
    };
    let Some(b_end) = b_start.checked_add(b_len) else {
        return true;
    };
    a_start < b_end && b_start < a_end
}

fn device_dma_rights_valid(rights: Rights) -> bool {
    let allowed = (Rights::READ | Rights::WRITE).bits();
    let bits = rights.bits();
    bits != 0 && (bits & !allowed) == 0
}

fn vmo_rights_for_device_dma(rights: Rights) -> Rights {
    let mut needed = Rights::empty();
    if rights.contains(Rights::READ) {
        needed |= Rights::READ;
    }
    if rights.contains(Rights::WRITE) {
        needed |= Rights::WRITE;
    }
    needed
}

fn configure_interrupt_source(irq: u32) -> Result<u32, Errno> {
    let key = interrupt_authority_key(irq);
    if let Some(gpio) = decode_tlmm_gpio_irq(irq) {
        if !kumo_hal::active::configure_tlmm_gpio_interrupt(gpio.pin, gpio.flags, key) {
            return Err(Errno::NotSupported);
        }
    } else if !kumo_hal::active::configure_spi_interrupt(irq) {
        // A plain device SPI (e.g. the xHCI controller) — enable it in the GIC so the bound
        // driver's InterruptWait can actually receive it.
        return Err(Errno::NotSupported);
    }
    Ok(key)
}

fn mask_interrupt_source(irq: u32) -> bool {
    if decode_tlmm_gpio_irq(irq).is_some() {
        kumo_hal::active::mask_tlmm_gpio_interrupt(irq)
    } else {
        irq >= 32 && kumo_hal::active::mask_spi_interrupt(irq)
    }
}

fn complete_interrupt_source(irq: u32) -> Result<(), Errno> {
    if decode_tlmm_gpio_irq(irq).is_some() {
        if !kumo_hal::active::complete_tlmm_gpio_interrupt(irq) {
            return Err(Errno::NotSupported);
        }
    } else if irq >= 32 {
        // Re-enable the device SPI masked when it fired; the driver has now serviced the device
        // and cleared the condition holding the line asserted.
        if !kumo_hal::active::unmask_spi_interrupt(irq) {
            return Err(Errno::BadState);
        }
    }
    Ok(())
}

/// Stored VMO with optional pre-allocated frames for anonymous backing.
#[derive(Debug)]
struct VmoEntry {
    koid: KoId,
    vmo: Vmo,
    #[allow(dead_code)]
    frames: RefCell<Vec<Option<u64>>>,
}

/// Own the VMO identity and its materialized page list, independent of user handles.
/// This pin is for unpublished, read-only device tables. MMIO needs the separate
/// exact GOP reserved-RAM proof; release after live DMA remains gated. — KESTREL
#[cfg(feature = "arch_aarch64")]
pub(crate) struct VmoPin {
    entry: Rc<VmoEntry>,
    offset: u64,
    len: u64,
    pages: Vec<u64>,
}

#[cfg(feature = "arch_aarch64")]
impl VmoPin {
    fn new(
        entry: Rc<VmoEntry>,
        offset: u64,
        len: u64,
        allocate: impl FnMut() -> Result<u64, Errno>,
    ) -> Result<Self, Errno> {
        Self::new_inner(entry, offset, len, None, allocate)
    }

    fn new_inner(
        entry: Rc<VmoEntry>,
        offset: u64,
        len: u64,
        framebuffer: Option<crate::device_memory::FramebufferRam>,
        mut allocate: impl FnMut() -> Result<u64, Errno>,
    ) -> Result<Self, Errno> {
        use crate::mm::{VmoBacking, PAGE_SIZE};
        if len == 0
            || offset % PAGE_SIZE != 0
            || len % PAGE_SIZE != 0
            || offset
                .checked_add(len)
                .is_none_or(|end| end > entry.vmo.len())
        {
            return Err(Errno::InvalidArgs);
        }
        let base = match entry.vmo.backing() {
            VmoBacking::PhysicalRam { phys_base } | VmoBacking::Dma { phys_base } => {
                if phys_base
                    .checked_add(offset)
                    .and_then(|p| p.checked_add(len))
                    .is_none_or(|end| end > 1 << 32)
                {
                    return Err(Errno::NotSupported);
                }
                Some(phys_base)
            }
            VmoBacking::Anonymous => None,
            VmoBacking::Mmio { phys_base } => {
                if !framebuffer.is_some_and(|proof| proof.allows(entry.vmo, offset, len)) {
                    return Err(Errno::NotSupported);
                }
                Some(phys_base)
            }
        };
        let count = usize::try_from(len / PAGE_SIZE).map_err(|_| Errno::InvalidArgs)?;
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(count)
            .map_err(|_| Errno::NoMemory)?;
        for n in 0..count {
            let pos = offset + n as u64 * PAGE_SIZE;
            let phys = match base {
                Some(base) => base + pos,
                None => ensure_vmo_frame(&entry, (pos / PAGE_SIZE) as usize, &mut allocate)?,
            };
            if phys % PAGE_SIZE != 0 || phys >= 1 << 32 {
                return Err(Errno::NotSupported);
            }
            pages.push(phys);
        }
        Ok(Self {
            entry,
            offset,
            len,
            pages,
        })
    }

    pub fn koid(&self) -> KoId {
        self.entry.koid
    }
    pub fn offset(&self) -> u64 {
        self.offset
    }
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn pages(&self) -> &[u64] {
        &self.pages
    }
}

const PHYSMAP_BASE: u64 = 0xffff_9000_0000_0000;

pub(crate) fn phys_ptr_mut(phys: u64) -> *mut u8 {
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

/// Allocate a physically-contiguous, zeroed run of `count` frames for device DMA, returning
/// its physical base. Runs with the kernel identity map active in TTBR0 (like
/// [`alloc_anonymous_frame`]) so the run can be zeroed and cleaned to PoC by physical address.
fn alloc_contiguous(
    boot: &BootInfo,
    count: u64,
    excluded: &[kumo_abi::Range],
) -> Result<u64, Errno> {
    #[cfg(target_os = "none")]
    {
        let saved_ttbr0 = kumo_hal::active::read_user_aspace_root();
        unsafe { kumo_hal::active::set_user_aspace_root(crate::user_thread::kernel_ttbr0()) };
        let base = unsafe { crate::mm::alloc_contiguous_frames_excluding(boot, count, excluded) };
        unsafe { kumo_hal::active::set_user_aspace_root(saved_ttbr0) };
        base.ok_or(Errno::NoMemory)
    }

    #[cfg(not(target_os = "none"))]
    {
        unsafe { crate::mm::alloc_contiguous_frames_excluding(boot, count, excluded) }
            .ok_or(Errno::NoMemory)
    }
}

fn ensure_anonymous_frame(
    entry: &VmoEntry,
    page_index: usize,
    boot: &BootInfo,
) -> Result<u64, Errno> {
    ensure_vmo_frame(entry, page_index, || alloc_anonymous_frame(boot))
}

fn ensure_vmo_frame(
    entry: &VmoEntry,
    page_index: usize,
    mut allocate: impl FnMut() -> Result<u64, Errno>,
) -> Result<u64, Errno> {
    let mut frames = entry.frames.borrow_mut();
    if frames.len() <= page_index {
        let additional = page_index + 1 - frames.len();
        frames
            .try_reserve(additional)
            .map_err(|_| Errno::NoMemory)?;
        frames.resize(page_index + 1, None);
    }
    if let Some(frame) = frames[page_index] {
        return Ok(frame);
    }
    // Retained-domain checks inspect this cache during physical allocation. Do not
    // hold its mutable borrow across that check or an address-space switch. — KESTREL
    drop(frames);
    let frame = allocate()?;
    let mut frames = entry.frames.borrow_mut();
    if let Some(existing) = frames[page_index] {
        return Ok(existing);
    }
    frames[page_index] = Some(frame);
    Ok(frame)
}

#[derive(Clone, Debug, Default)]
struct DeviceState {
    iommus: Vec<IoMmuBinding>,
    device_ctxs: Vec<DeviceCtxBinding>,
    #[cfg(feature = "arch_aarch64")]
    display_contexts: Vec<DisplayContext>,
    #[cfg(feature = "arch_aarch64")]
    display_reservation: Option<Rc<()>>,
    device_vmar_mappings: Vec<DeviceVmarMapping>,
    device_ctx_faults: Vec<DeviceCtxFault>,
    port_bindings: Vec<(KoId, KoId)>,
    next: Option<Box<DeviceState>>,
}

impl DeviceState {
    fn unit_reserved(&self, base: u64, len: u64) -> bool {
        self.device_ctxs.iter().any(|ctx| {
            ctx.state.get() == DeviceCtxState::Quarantined
                && self
                    .iommus
                    .iter()
                    .find(|unit| unit.koid == ctx.iommu_koid)
                    .is_none_or(|unit| ranges_overlap(base, len, unit.phys_base, unit.len))
        })
    }

    fn ram_allowed(&self, base: u64, len: u64, authorized: Option<&VmoEntry>) -> bool {
        for ctx in &self.device_ctxs {
            if ctx.state.get() != DeviceCtxState::Quarantined {
                continue;
            }
            if ctx.pgd_phys != 0 && ranges_overlap(base, len, ctx.pgd_phys, crate::mm::PAGE_SIZE) {
                return false;
            }
            #[cfg(feature = "arch_aarch64")]
            if let Some(domain) = self.display_contexts.iter().find(|d| d.koid == ctx.koid) {
                let Ok(maps) = domain.mappings.try_borrow() else {
                    return false;
                };
                let Ok(published) = domain.published.try_borrow() else {
                    return false;
                };
                let offline_pins = maps.as_ref().into_iter().flat_map(|m| m.pins());
                let live_pins = published.as_ref().into_iter().flat_map(|m| m.pins());
                {
                    for pin in offline_pins.chain(live_pins) {
                        if authorized.is_some_and(|entry| core::ptr::eq(entry, pin.entry.as_ref()))
                        {
                            continue;
                        }
                        if pin
                            .pages()
                            .iter()
                            .any(|p| ranges_overlap(base, len, *p, crate::mm::PAGE_SIZE))
                        {
                            return false;
                        }
                    }
                }
            }
            for map in self
                .device_vmar_mappings
                .iter()
                .filter(|m| m.ctx_koid == ctx.koid)
            {
                if authorized.is_some_and(|entry| core::ptr::eq(entry, map.backing.as_ref())) {
                    continue;
                }
                use crate::mm::{VmoBacking, PAGE_SIZE};
                match map.backing.vmo.backing() {
                    VmoBacking::PhysicalRam { phys_base }
                    | VmoBacking::Mmio { phys_base }
                    | VmoBacking::Dma { phys_base } => {
                        let Some(start) = phys_base.checked_add(map.vmo_offset) else {
                            return false;
                        };
                        if ranges_overlap(base, len, start, map.len) {
                            return false;
                        }
                    }
                    VmoBacking::Anonymous => {
                        let Ok(frames) = map.backing.frames.try_borrow() else {
                            return false;
                        };
                        let Some(end) = map.vmo_offset.checked_add(map.len) else {
                            return false;
                        };
                        for offset in (map.vmo_offset..end).step_by(PAGE_SIZE as usize) {
                            let Some(Some(frame)) = frames.get((offset / PAGE_SIZE) as usize)
                            else {
                                return false;
                            };
                            if ranges_overlap(base, len, *frame, PAGE_SIZE) {
                                return false;
                            }
                        }
                    }
                }
            }
        }
        true
    }
}

/// Box the device payload when the engine is created. Destruction can link that same
/// allocation into kernel retention without allocating or touching hardware. — KESTREL
#[derive(Clone, Debug)]
struct EngineDevices(Option<Box<DeviceState>>);
impl Default for EngineDevices {
    fn default() -> Self {
        Self(Some(Box::new(DeviceState::default())))
    }
}
impl core::ops::Deref for EngineDevices {
    type Target = DeviceState;
    fn deref(&self) -> &DeviceState {
        self.0.as_deref().expect("live device payload")
    }
}
impl core::ops::DerefMut for EngineDevices {
    fn deref_mut(&mut self) -> &mut DeviceState {
        self.0.as_deref_mut().expect("live device payload")
    }
}

#[derive(Debug)]
struct DeviceRetention {
    retained: RefCell<Option<Box<DeviceState>>>,
    frozen: Cell<bool>,
    #[cfg(feature = "arch_aarch64")]
    table_pages: crate::device_memory::TablePool<crate::device_memory::KernelPages>,
    #[cfg(feature = "arch_aarch64")]
    display_reservation: RefCell<alloc::rc::Weak<()>>,
    #[cfg(feature = "arch_aarch64")]
    display_controller: RefCell<Option<kumo_hal::active::DisplayController>>,
}

#[derive(Clone, Debug)]
struct KernelDeviceOwner(Rc<DeviceRetention>);
impl KernelDeviceOwner {
    fn new() -> Self {
        Self(Rc::new(DeviceRetention {
            retained: RefCell::new(None),
            frozen: Cell::new(false),
            #[cfg(feature = "arch_aarch64")]
            table_pages: Default::default(),
            #[cfg(feature = "arch_aarch64")]
            display_reservation: RefCell::new(alloc::rc::Weak::new()),
            #[cfg(feature = "arch_aarch64")]
            display_controller: RefCell::new(None),
        }))
    }

    fn retain(&self, mut state: Box<DeviceState>) {
        for ctx in &state.device_ctxs {
            if ctx.state.get() != DeviceCtxState::Released {
                ctx.state.set(DeviceCtxState::Quarantined);
            }
        }
        if let Ok(mut retained) = self.0.retained.try_borrow_mut() {
            state.next = retained.take();
            *retained = Some(state);
        } else {
            // A reentrant/uncertain owner cannot permit a restart to reuse memory or
            // units. Leak the already-owned payload and deny all subsequent admission.
            // No allocator, wake or HAL call is made on this fallback. — KESTREL
            self.0.frozen.set(true);
            core::mem::forget(state);
        }
    }

    fn permits(&self, mut check: impl FnMut(&DeviceState) -> bool) -> bool {
        if self.0.frozen.get() {
            return false;
        }
        let Ok(retained) = self.0.retained.try_borrow() else {
            return false;
        };
        let mut state = retained.as_deref();
        while let Some(payload) = state {
            if !check(payload) {
                return false;
            }
            state = payload.next.as_deref();
        }
        true
    }
    fn ram_allowed(&self, base: u64, len: u64, authorized: Option<&VmoEntry>) -> bool {
        self.permits(|state| state.ram_allowed(base, len, authorized))
    }
    fn unit_allowed(&self, base: u64, len: u64) -> bool {
        self.permits(|state| !state.unit_reserved(base, len))
    }
}

#[cfg(target_os = "none")]
struct KernelDeviceCell(core::cell::UnsafeCell<Option<KernelDeviceOwner>>);
// Engine creation belongs to the serialized boot/runtime thread; IRQ handlers neither
// initialize nor drop engines. The kernel keeps this owner beyond every Sora attempt.
// Its retained-state borrows never allocate, wake, or switch. — KESTREL
#[cfg(target_os = "none")]
unsafe impl Sync for KernelDeviceCell {}
#[cfg(target_os = "none")]
static KERNEL_DEVICES: KernelDeviceCell = KernelDeviceCell(core::cell::UnsafeCell::new(None));

#[cfg(target_os = "none")]
fn kernel_device_owner() -> KernelDeviceOwner {
    let slot = unsafe { &mut *KERNEL_DEVICES.0.get() };
    slot.get_or_insert_with(KernelDeviceOwner::new).clone()
}

/// No initialization/allocation on the frame-allocation path. Before any engine exists
/// there are no retained domains. Apply exclusions before zero/cache maintenance. — KESTREL
pub(crate) fn kernel_device_frame_allowed(base: u64, len: u64) -> bool {
    #[cfg(target_os = "none")]
    {
        unsafe { (&*KERNEL_DEVICES.0.get()).as_ref() }
            .is_none_or(|owner| owner.ram_allowed(base, len, None))
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (base, len);
        true
    }
}

#[derive(Clone, Debug)]
pub struct SyscallEngine {
    devices: EngineDevices,
    device_owner: KernelDeviceOwner,
    objects: ObjectManager,
    ipc: IpcRegistry,
    // Registry clones and device pins own the same immutable VMO identity and the same
    // demand-page cache. Copying the cache could allocate divergent backing. — KESTREL
    vmos: Vec<Rc<VmoEntry>>,
    processes: Vec<Process>,
    threads: Vec<Thread>,
    boot_info: Option<kumo_abi::BootInfo>,
    #[cfg(feature = "arch_aarch64")]
    framebuffer_ram: Option<crate::device_memory::FramebufferRam>,
    #[cfg(feature = "arch_aarch64")]
    display_unit: Option<kumo_hal::active::DisplayIoMmu>,
    interrupts: Vec<IrqBinding>,
    timers: Vec<TimerBinding>,
    resources: Vec<ResourceBinding>,
    #[cfg(feature = "arch_aarch64")]
    table_pages: crate::device_memory::TablePool<crate::device_memory::KernelPages>,
}

impl Default for SyscallEngine {
    fn default() -> Self {
        Self::new()
    }
}
impl Drop for SyscallEngine {
    fn drop(&mut self) {
        if self
            .devices
            .device_ctxs
            .iter()
            .any(|ctx| ctx.state.get() != DeviceCtxState::Released)
        {
            if let Some(state) = self.devices.0.take() {
                self.device_owner.retain(state);
            }
        }
    }
}

impl SyscallEngine {
    pub fn new() -> Self {
        #[cfg(target_os = "none")]
        let owner = kernel_device_owner();
        #[cfg(not(target_os = "none"))]
        let owner = KernelDeviceOwner::new();
        Self::with_device_owner(owner)
    }

    fn with_device_owner(owner: KernelDeviceOwner) -> Self {
        Self {
            devices: EngineDevices::default(),
            device_owner: owner.clone(),
            objects: ObjectManager::new(),
            ipc: IpcRegistry::new(),
            vmos: Vec::new(),
            processes: Vec::new(),
            threads: Vec::new(),
            boot_info: None,
            #[cfg(feature = "arch_aarch64")]
            framebuffer_ram: None,
            #[cfg(feature = "arch_aarch64")]
            display_unit: None,
            interrupts: Vec::new(),
            timers: Vec::new(),
            resources: Vec::new(),
            #[cfg(feature = "arch_aarch64")]
            table_pages: owner.0.table_pages.clone(),
        }
    }

    /// Create a root Resource covering MMIO `[phys_base, phys_base + len)` and the IRQ
    /// window `[irq_base, irq_base + irq_count)`. Returns a handle for `caller`. Sora
    /// narrows this into per-device Resources before spawning drivers.
    pub fn root_resource_create(
        &mut self,
        caller: &mut Process,
        phys_base: u64,
        len: u64,
        irq_base: u32,
        irq_count: u32,
    ) -> Result<Handle, ObjectError> {
        let object = self.objects.create(ObjectKind::Resource);
        let handle = caller.handles_mut().insert(
            object,
            Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER | Rights::MANAGE,
        )?;
        self.resources.push(ResourceBinding {
            koid: object.koid(),
            phys_base,
            len,
            irq_base,
            irq_count,
        });
        Ok(handle)
    }

    fn resource_create_child(
        &mut self,
        caller: &mut Process,
        parent: Handle,
        phys_base: u64,
        len: u64,
        irq_base: u32,
        irq_count: u32,
    ) -> Result<Handle, Errno> {
        if len == 0 {
            return Err(Errno::InvalidArgs);
        }
        let parent_entry = caller
            .handles()
            .require(parent, ObjectKind::Resource, Rights::MANAGE)
            .map_err(errno_from_object)?;
        let parent = self
            .resource_by_koid(parent_entry.koid)
            .ok_or(Errno::BadHandle)?;
        if !resource_contains(parent, phys_base, len) {
            return Err(Errno::AccessDenied);
        }
        if !resource_contains_irq_window(parent, irq_base, irq_count) {
            return Err(Errno::AccessDenied);
        }

        let object = self.objects.create(ObjectKind::Resource);
        let handle = caller
            .handles_mut()
            .insert(
                object,
                Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER,
            )
            .map_err(errno_from_object)?;
        self.resources.push(ResourceBinding {
            koid: object.koid(),
            phys_base,
            len,
            irq_base,
            irq_count,
        });
        Ok(handle)
    }

    /// Look up a Resource binding by koid.
    fn resource_by_koid(&self, koid: KoId) -> Option<ResourceBinding> {
        self.resources
            .iter()
            .find_map(|r| if r.koid == koid { Some(*r) } else { None })
    }

    /// Final ownership loss masks the source before removing its binding and watches.
    /// A failed mask retains the reservation without retry; a cloned engine observes
    /// the same outcome and cannot mask a subsequently rebound source. — KESTREL
    pub(crate) fn retire_interrupts(&mut self) -> usize {
        self.retire_interrupts_with(mask_interrupt_source)
    }

    fn retire_interrupts_with(&mut self, mut mask: impl FnMut(u32) -> bool) -> usize {
        let mut removed = 0;
        let mut index = 0;
        while index < self.interrupts.len() {
            let binding = &self.interrupts[index];
            if binding.state.get() == IrqState::Bound && !binding.references.is_referenced() {
                binding.state.set(if mask(binding.irq) {
                    IrqState::Released
                } else {
                    IrqState::MaskFailed
                });
            }
            if binding.state.get() == IrqState::Released {
                let koid = binding.koid;
                self.release_port_bindings(koid);
                self.interrupts.remove(index);
                removed += 1;
            } else {
                index += 1;
            }
        }
        removed
    }

    /// Collect the distinct IRQ lines that currently have a live interrupt binding into
    /// `out`, returning how many were written. Used by the power path to mask every device
    /// line a driver owns before the machine is brought down: once masked, an IRQ-driven
    /// driver child can no longer be woken, which is how userspace is quiesced without a
    /// shutdown protocol it would have to honour.
    ///
    /// Fills a caller-supplied slice rather than returning a `Vec` on purpose — the caller
    /// runs inside the `SoraState` borrow, and allocating there is what froze the machine
    /// when `poll_root_command` still built a `String`.
    pub fn bound_interrupt_irqs(&self, out: &mut [u32]) -> usize {
        let mut len = 0;
        for binding in &self.interrupts {
            if binding.state.get() == IrqState::Released {
                continue;
            }
            if len == out.len() {
                break;
            }
            if out[..len].contains(&binding.irq) {
                continue;
            }
            out[len] = binding.irq;
            len += 1;
        }
        len
    }

    /// Cancel a pending one-shot timer. Timer handles are intentionally not
    /// duplicable, so closing the handle is sufficient to end its lifetime.
    pub fn release_timer(&mut self, koid: KoId) -> bool {
        let before = self.timers.len();
        self.timers.retain(|binding| binding.koid != koid);
        self.timers.len() != before
    }

    /// Reclaim the Resource grant for `koid` at process teardown, so a restarted
    /// driver's re-carved grant does not accumulate orphaned bindings. Returns true
    /// if a binding was removed.
    pub fn release_resource(&mut self, koid: KoId) -> bool {
        let before = self.resources.len();
        self.resources.retain(|binding| binding.koid != koid);
        self.resources.len() != before
    }

    /// A binding is an observer, not a user capability. Unknown contexts must never
    /// be mistaken for a confirmed last close. Release still requires a separate
    /// checked hardware outcome. — KESTREL
    pub(crate) fn device_ctx_has_capabilities(&self, koid: KoId) -> Result<bool, Errno> {
        self.devices
            .device_ctxs
            .iter()
            .find(|ctx| ctx.koid == koid)
            .map(|ctx| ctx.references.is_referenced())
            .ok_or(Errno::BadHandle)
    }

    fn iommu_unit_quarantined(&self, base: u64, len: u64) -> bool {
        self.devices.unit_reserved(base, len) || !self.device_owner.unit_allowed(base, len)
    }

    #[cfg(feature = "arch_aarch64")]
    fn display_context(&self, koid: KoId) -> Option<DisplayContext> {
        self.devices
            .display_contexts
            .iter()
            .find(|d| d.koid == koid)
            .cloned()
    }

    #[cfg(feature = "arch_aarch64")]
    fn display_iommu_supported(&self, base: u64, len: u64) -> bool {
        if !self
            .display_unit
            .is_some_and(|u| u.matches_range(base, len))
        {
            return false;
        }
        #[cfg(target_os = "none")]
        {
            kumo_hal::active::display_iommu_unit_ready(self.dtb(), base, len)
        }
        // Host tests supply the descriptor explicitly; production host builds never
        // discover a physical unit. No host MMIO or physical pointer access. — KESTREL
        #[cfg(not(target_os = "none"))]
        {
            cfg!(test)
        }
    }

    fn physical_access_allowed(
        &self,
        entry: &VmoEntry,
        offset: u64,
        len: u64,
        coarse: bool,
    ) -> bool {
        self.vmo_span_allowed(entry.vmo, offset, len, coarse, Some(entry))
    }

    fn vmo_span_allowed(
        &self,
        vmo: Vmo,
        offset: u64,
        len: u64,
        coarse: bool,
        authorized: Option<&VmoEntry>,
    ) -> bool {
        use crate::mm::VmoBacking;
        let base = match vmo.backing() {
            VmoBacking::Anonymous => return true,
            VmoBacking::PhysicalRam { phys_base }
            | VmoBacking::Mmio { phys_base }
            | VmoBacking::Dma { phys_base } => phys_base,
        };
        let Some(start) = base.checked_add(offset) else {
            return false;
        };
        let Some(end) = start.checked_add(len) else {
            return false;
        };
        let (start, end) = if coarse
            && matches!(
                vmo.backing(),
                VmoBacking::Mmio { .. } | VmoBacking::Dma { .. }
            ) {
            const MASK: u64 = (1 << 21) - 1;
            let Some(end) = end.checked_add(MASK) else {
                return false;
            };
            (start & !MASK, end & !MASK)
        } else {
            (start, end)
        };
        self.devices.ram_allowed(start, end - start, authorized)
            && self
                .device_owner
                .ram_allowed(start, end - start, authorized)
    }

    /// Called only after removed/drained entries have dropped their owners. A false
    /// backend result retains the entire context and forbids automatic retries/reuse.
    /// This engine-local quarantine is not cross-Sora retention. — KESTREL
    pub(crate) fn retire_device_contexts(&mut self) -> DeviceCtxRetirement {
        self.retire_device_contexts_with(|unit, ctx| {
            kumo_hal::active::iommu_destroy_device_context(
                unit.iommu_kind,
                unit.phys_base,
                ctx.stream_id,
            )
        })
    }

    fn retire_device_contexts_with(
        &mut self,
        mut block_and_detach: impl FnMut(&IoMmuBinding, &DeviceCtxBinding) -> bool,
    ) -> DeviceCtxRetirement {
        let mut report = DeviceCtxRetirement::default();
        let mut index = 0;
        while index < self.devices.device_ctxs.len() {
            let ctx = self.devices.device_ctxs[index].clone();
            if ctx.state.get() == DeviceCtxState::Bound && !ctx.references.is_referenced() {
                let unit = self
                    .devices
                    .iommus
                    .iter()
                    .find(|unit| unit.koid == ctx.iommu_koid)
                    .copied();
                let unit_quarantined =
                    unit.is_some_and(|unit| self.iommu_unit_quarantined(unit.phys_base, unit.len));
                // Fail closed before calling the backend. No logging, wake or context
                // switch may re-enter Sora from this checked hardware operation. — KESTREL
                ctx.state.set(DeviceCtxState::Quarantined);
                let released = !unit_quarantined
                    && unit.is_some_and(|unit| {
                        #[cfg(feature = "arch_aarch64")]
                        if unit.iommu_kind == kumo_abi::sys::IoMmuKind::RockchipV2 as u32 {
                            return self.display_context(ctx.koid).is_some_and(|d| {
                                let Ok(mut published) = d.published.try_borrow_mut() else {
                                    return false;
                                };
                                if let Some(live) = published.as_ref() {
                                    let Ok(mut controller) =
                                        self.device_owner.0.display_controller.try_borrow_mut()
                                    else {
                                        return false;
                                    };
                                    let Some(controller) = controller.as_mut() else {
                                        return false;
                                    };
                                    let Ok(proof) = kumo_hal::active::display_controller_detach(
                                        controller,
                                        live.root_ref(),
                                        d.epoch.get(),
                                    ) else {
                                        return false;
                                    };
                                    let owned = published.take().expect("checked publication");
                                    match owned.reclaim(proof) {
                                        Ok(offline) => {
                                            drop(offline);
                                            #[cfg(target_os="none")]
                                            crate::klog_at!(kumo_abi::sys::log_level::NOTICE,"DISPLAY DETACH Check       final holder gone, generation {} both roots reset, pins/tables released; producer remains held, no bypass resume\n",d.epoch.get());
                                        },
                                        Err(owned) => {
                                            *published = Some(owned);
                                            return false;
                                        }
                                    }
                                    return true;
                                }
                                d.mappings
                                    .try_borrow_mut()
                                    .is_ok_and(|mut m| m.take().is_some())
                            });
                        }
                        block_and_detach(&unit, &ctx)
                    });
                if released {
                    ctx.state.set(DeviceCtxState::Released);
                } else {
                    report.quarantined += 1;
                }
            }
            if ctx.state.get() == DeviceCtxState::Released {
                // The shared state also lets a cloned engine discard its retained
                // metadata after the same proof, without calling the backend twice.
                // Drop backing references only after the checked outcome. — KESTREL
                self.devices
                    .device_vmar_mappings
                    .retain(|m| m.ctx_koid != ctx.koid);
                self.devices
                    .device_ctx_faults
                    .retain(|fault| fault.ctx_koid != ctx.koid);
                self.release_port_bindings(ctx.koid);
                self.devices.device_ctxs.remove(index);
                #[cfg(feature = "arch_aarch64")]
                self.devices.display_contexts.retain(|d| d.koid != ctx.koid);
                report.released += 1;
            } else {
                index += 1;
            }
        }
        report
    }

    /// Drop every port binding touching `koid`, whether it is the watching port or the
    /// watched object. Called at teardown for each handle a dying process releases: once
    /// either endpoint is gone the binding can never fire again, so leaving it would leak
    /// and let a future koid reuse alias a stale watch. Returns true if any were removed.
    pub fn release_port_bindings(&mut self, koid: KoId) -> bool {
        let before = self.devices.port_bindings.len();
        self.devices
            .port_bindings
            .retain(|&(port_koid, object_koid)| port_koid != koid && object_koid != koid);
        self.devices.port_bindings.len() != before
    }

    /// Signal all interrupt objects bound to `irq`. Called from the IRQ handler.
    /// Increments the fire count for each matching binding (the `InterruptWait`
    /// drain path) **and** signals any `Port` the interrupt is bound to with
    /// `Signals::IRQ` — so a driver may `port_wait` on {IRQ-fired OR
    /// channel-readable} in one thread, which console-out (Stage C) needs to
    /// serve input and output from a single loop. Channel-readable already feeds
    /// ports (`ChannelWrite` → `signal_ports(READABLE)`); this closes the
    /// symmetric gap for interrupts (J145 §1).
    pub fn signal_interrupt(&mut self, irq: u32) {
        // Indexed walk, not `for binding in &mut self.interrupts`: `signal_ports`
        // re-borrows `self` (port_bindings + ipc), so no borrow of `self.interrupts`
        // may be live across it (GUIDANCE/006 §2.1). signal_ports touches neither
        // `interrupts` nor its length, so the index stays valid.
        let mut i = 0;
        while i < self.interrupts.len() {
            if self.interrupts[i].irq == irq
                && self.interrupts[i].state.get() == IrqState::Bound
                && self.interrupts[i].references.is_referenced()
            {
                if self.interrupts[i].requires_complete && self.interrupts[i].outstanding {
                    i += 1;
                    continue;
                }
                self.interrupts[i].count = self.interrupts[i].count.saturating_add(1);
                self.interrupts[i].outstanding = true;
                let koid = self.interrupts[i].koid;
                #[cfg(target_os = "none")]
                crate::user_thread::wake_child_waiting_on_interrupt(koid);
                self.signal_ports(koid, kumo_abi::Signals::IRQ);
            }
            i += 1;
        }
    }

    /// Expire every one-shot timer whose monotonic deadline has arrived.
    /// Marking the timer first makes the transition exactly-once and drops the
    /// element borrow before `signal_ports` re-enters the engine.
    pub fn signal_timers(&mut self, now_ns: u64) {
        let mut i = 0;
        while i < self.timers.len() {
            if !self.timers[i].fired && self.timers[i].deadline_ns <= now_ns {
                self.timers[i].fired = true;
                let koid = self.timers[i].koid;
                self.signal_ports(koid, kumo_abi::Signals::TIMER);
            }
            i += 1;
        }
    }

    /// Raw capture already happened outside Sora. Fixed per-context slots share the
    /// first unit refusal across engine clones and never allocate a fault record.
    /// Reading/closing an IRQ or consuming records cannot clear quarantine. — KESTREL
    #[cfg(feature = "arch_aarch64")]
    pub(crate) fn signal_display_fault(&mut self, fault: kumo_hal::active::DisplayFault) {
        let mut index = 0;
        while index < self.devices.display_contexts.len() {
            let domain = self.devices.display_contexts[index].clone();
            index += 1;
            let Some(ctx) = self
                .devices
                .device_ctxs
                .iter()
                .find(|c| c.koid == domain.koid)
            else {
                continue;
            };
            if fault.epoch != 0 && domain.epoch.get() != fault.epoch {
                continue;
            }
            if ctx.state.get() == DeviceCtxState::Released || domain.faults.latched.replace(true) {
                continue;
            }
            ctx.state.set(DeviceCtxState::Quarantined);
            for (slot, record) in domain.faults.slots.iter().zip(fault.records()) {
                slot.set(record);
            }
            #[cfg(target_os = "none")]
            crate::user_thread::wake_child_waiting_on_device_ctx_fault(domain.koid);
            self.signal_ports(domain.koid, kumo_abi::Signals::DEVICE_FAULT);
        }
    }

    pub fn signal_device_ctx_fault(
        &mut self,
        ctx_koid: KoId,
        fault_record: u64,
        fault_addr: u64,
    ) -> Result<(), Errno> {
        if self
            .devices
            .device_ctxs
            .iter()
            .all(|ctx| ctx.koid != ctx_koid)
        {
            return Err(Errno::BadHandle);
        }
        self.devices.device_ctx_faults.push(DeviceCtxFault {
            ctx_koid,
            info: DeviceCtxInfo {
                fault_record,
                fault_addr,
            },
        });
        #[cfg(target_os = "none")]
        crate::user_thread::wake_child_waiting_on_device_ctx_fault(ctx_koid);
        self.signal_ports(ctx_koid, kumo_abi::Signals::DEVICE_FAULT);
        Ok(())
    }

    pub fn signal_iommu_stream_fault(
        &mut self,
        iommu_koid: KoId,
        stream_id: u32,
        fault_record: u64,
        fault_addr: u64,
    ) -> Result<KoId, Errno> {
        let mut matched = None;
        for ctx in &self.devices.device_ctxs {
            if ctx.iommu_koid == iommu_koid && ctx.stream_id == stream_id {
                if matched.is_some() {
                    return Err(Errno::InvalidArgs);
                }
                matched = Some(ctx.koid);
            }
        }
        let ctx_koid = matched.ok_or(Errno::BadHandle)?;
        self.signal_device_ctx_fault(ctx_koid, fault_record, fault_addr)?;
        Ok(ctx_koid)
    }

    /// Bind an object to a port. When an event occurs on that object,
    /// the port is signalled with the corresponding signals and the object koid as source.
    pub fn port_bind(&mut self, port_koid: KoId, object_koid: KoId) {
        self.devices.port_bindings.push((port_koid, object_koid));
        // Binding is level-triggered for channel state, just like the timer/fault cases below.
        // A message may have arrived before the receiver created and bound its serve port. If we
        // only remember future transitions, that already-readable inbox is stranded until an
        // unrelated write happens. Sora used to compensate with a blocking "backlog drain";
        // when the HID channel was empty that parked Sora while its RefCell borrow was live and
        // wedged every ordinary console key (IRQ-side sysrq remained alive). Replay only states
        // useful to a channel reader -- WRITABLE is normally always set and would busy-loop.
        if let Ok(signals) = self.ipc.channel_signals_by_koid(object_koid) {
            let mut pending = kumo_abi::Signals::empty();
            if signals.contains(kumo_abi::Signals::READABLE) {
                pending |= kumo_abi::Signals::READABLE;
            }
            if signals.contains(kumo_abi::Signals::PEER_CLOSED) {
                pending |= kumo_abi::Signals::PEER_CLOSED;
            }
            if pending.bits() != 0 {
                self.signal_ports(object_koid, pending);
            }
        }
        if self
            .timers
            .iter()
            .any(|timer| timer.koid == object_koid && timer.fired)
        {
            self.signal_ports(object_koid, kumo_abi::Signals::TIMER);
        }
        if self
            .devices
            .device_ctx_faults
            .iter()
            .any(|fault| fault.ctx_koid == object_koid)
        {
            self.signal_ports(object_koid, kumo_abi::Signals::DEVICE_FAULT);
        }
    }

    /// Remove the binding from `port_koid` to `object_koid`, if present — the inverse
    /// of `port_bind`. A supervisor calls this to drop a watch it no longer needs (e.g.
    /// a restarted instance's dead predecessor), so bindings do not accumulate once
    /// repeated restarts become normal. Unlike `release_port_bindings` (teardown, keyed
    /// on a single koid touching either endpoint), this removes exactly the one named
    /// pair and leaves every other watch on that port intact. Returns true if a binding
    /// was removed.
    pub fn port_unbind(&mut self, port_koid: KoId, object_koid: KoId) -> bool {
        let before = self.devices.port_bindings.len();
        self.devices
            .port_bindings
            .retain(|&(p, o)| !(p == port_koid && o == object_koid));
        self.devices.port_bindings.len() != before
    }

    /// Signal all ports bound to `object_koid`.
    pub fn signal_ports(&mut self, object_koid: KoId, signals: kumo_abi::Signals) {
        for &(port_koid, bound_koid) in &self.devices.port_bindings {
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

    /// Level-triggered READABLE over coalescing ports. A burst of channel writes merges
    /// into ONE port packet ([`Port::queue_signal`] coalesces per source), yet each write
    /// is a distinct channel message. A reader that consumes one message per port wake
    /// then strands the rest of the burst until some *unrelated* signal re-arms the port —
    /// the X13s boot-tail "framebuffer repaints only on keypress" lag (`DEFERRED/004`) and
    /// the same piecemeal draining for a keystroke burst. So: after a `ChannelRead`, if the
    /// endpoint just read still holds a queued message, re-queue its bound port packet, so
    /// the reader's next `PortWait` returns immediately and drains the whole backlog in one
    /// quantum. **Port-signal only** — this never pokes channel waiters (that, paired with
    /// the j408 deferred IRQ handoff, was the j409 keypress lock; both are gone). It fires
    /// only when a real message remains, so it cannot spuriously spin.
    fn rearm_port_if_channel_readable(&mut self, channel_koid: KoId) {
        if matches!(
            self.ipc.channel_signals_by_koid(channel_koid),
            Ok(signals) if signals.contains(kumo_abi::Signals::READABLE)
        ) {
            self.signal_ports(channel_koid, kumo_abi::Signals::READABLE);
        }
    }

    pub fn set_boot_info(&mut self, boot: kumo_abi::BootInfo) {
        // Only the kernel installs its validated handoff, before entering EL0.
        // Snapshot classification now; pins never dereference a physical map or
        // trust a userspace BootInfo copy on an SVC path. — KESTREL
        #[cfg(feature = "arch_aarch64")]
        {
            self.display_unit = kumo_hal::active::display_iommu_from_dtb(boot.platform.dtb);
            self.framebuffer_ram = if boot.flags & BootInfo::FLAG_FRAMEBUFFER_RESERVED_RAM != 0 {
                crate::device_memory::FramebufferRam::from_boot(&boot, unsafe {
                    boot.mem_regions.as_slice()
                })
            } else {
                None
            };
        }
        self.boot_info = Some(boot);
        #[cfg(feature = "arch_aarch64")]
        self.table_pages.set_boot_info(boot);
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
        if !self.vmo_span_allowed(vmo, 0, vmo.len(), true, None) {
            return Err(ObjectError::AccessDenied);
        }
        #[cfg(feature = "arch_aarch64")]
        let span = self.table_pages.prepare_vmo(vmo)?;
        self.vmos
            .try_reserve(1)
            .map_err(|_| ObjectError::TableFull)?;
        let object = self.objects.create(ObjectKind::Vmo);
        let handle = process.handles_mut().insert(object, rights)?;
        #[cfg(feature = "arch_aarch64")]
        self.table_pages.commit_vmo(span);
        self.vmos.push(Rc::new(VmoEntry {
            koid: object.koid(),
            vmo,
            frames: RefCell::new(Vec::new()),
        }));
        Ok(handle)
    }

    #[cfg(feature = "arch_aarch64")]
    pub(crate) fn pin_device_vmo_read_only(
        &self,
        process: &Process,
        handle: Handle,
        offset: u64,
        len: u64,
        rights: Rights,
    ) -> Result<VmoPin, Errno> {
        if rights != Rights::READ {
            return Err(Errno::AccessDenied);
        }
        let handle = process
            .handles()
            .require(handle, ObjectKind::Vmo, Rights::READ)
            .map_err(errno_from_object)?;
        let entry = self
            .vmos
            .iter()
            .find(|v| v.koid == handle.koid)
            .ok_or(Errno::BadHandle)?
            .clone();
        if matches!(entry.vmo.backing(), crate::mm::VmoBacking::Mmio { .. }) {
            return VmoPin::new_inner(entry, offset, len, self.framebuffer_ram, || {
                Err(Errno::NotSupported)
            });
        }
        VmoPin::new(entry, offset, len, || {
            alloc_anonymous_frame(self.boot_info.as_ref().ok_or(Errno::BadState)?)
        })
    }

    /// Enroll only the attested GOP pages in an unpublished table. Do not touch
    /// framebuffer bytes, publish a root or modify either display MMU. — KESTREL
    #[cfg(all(feature = "arch_aarch64", target_os = "none"))]
    pub(crate) fn verify_framebuffer_device_pin(
        &mut self,
        process: &mut Process,
        resource: Handle,
    ) -> Result<(), Errno> {
        use kumo_hal::active::DeviceTableAccess as Access;
        let Some(proof) = self.framebuffer_ram else {
            crate::klog_at!(kumo_abi::sys::log_level::NOTICE,
                "GOP DEVICE PIN    Refuse    loader reserved-RAM attestation absent; no context admitted\n");
            return Err(Errno::NotSupported);
        };
        let span = proof.span();
        let handle = match self.dispatch(
            process,
            KernelCall::ResourceMintMmio {
                resource,
                phys_base: span.start,
                len: span.len,
            },
        ) {
            KernelCallResult::Handle(handle) => handle,
            _ => return Err(Errno::Internal),
        };
        let koid = process
            .handles()
            .get(handle)
            .map_err(errno_from_object)?
            .koid;
        let owner = Rc::downgrade(
            self.vmos
                .iter()
                .find(|v| v.koid == koid)
                .ok_or(Errno::Internal)?,
        );
        if self
            .pin_device_vmo_read_only(process, handle, 0, span.len, Rights::READ | Rights::WRITE)
            .is_ok()
            || self
                .pin_device_vmo_read_only(
                    process,
                    handle,
                    0,
                    span.len - crate::mm::PAGE_SIZE,
                    Rights::READ,
                )
                .is_ok()
        {
            return Err(Errno::Internal);
        }
        let pin = self.pin_device_vmo_read_only(process, handle, 0, span.len, Rights::READ)?;
        let mut mappings = crate::device_memory::OfflineMappings::new(&self.table_pages)?;
        mappings.map(span.start, pin)?;
        if self.dispatch(process, KernelCall::HandleClose { handle })
            != KernelCallResult::Status(Errno::Ok.status())
        {
            return Err(Errno::Internal);
        }
        self.vmos.retain(|v| v.koid != koid);
        let end = span.start + span.len;
        if owner.upgrade().is_none()
            || mappings.walk(span.start, Access::Read)? != Some(span.start)
            || mappings.walk(end - 1, Access::Read)? != Some(end - 1)
            || mappings.walk(end, Access::Read)?.is_some()
            || mappings.walk(span.start, Access::Write) != Err(Errno::AccessDenied)
            || mappings.unmap(span.start, span.len - crate::mm::PAGE_SIZE)
                != Err(Errno::InvalidArgs)
        {
            return Err(Errno::Internal);
        }
        mappings.unmap(span.start, span.len)?;
        if owner.upgrade().is_some() || mappings.walk(span.start, Access::Read)?.is_some() {
            return Err(Errno::Internal);
        }
        drop(mappings);
        let fb = self.boot_info.ok_or(Errno::Internal)?.framebuffer;
        crate::klog_at!(kumo_abi::sys::log_level::NOTICE,
            "GOP DEVICE PIN    Check     {:#x}, visible {:#x}, mapped {:#x}, padding {:#x}; read-only, closed handle, offline unmap OK; no paging\n",
            span.start, fb.len, span.len, span.len - fb.len);
        Ok(())
    }

    /// Hold the known producer and roundtrip an owned, cleaned GOP directory through
    /// both MMUs with paging off. Failure retains every table page and pin. — KESTREL
    #[cfg(all(feature = "arch_aarch64", target_os = "none"))]
    pub(crate) fn verify_display_directory_reset(
        &mut self,
        process: &mut Process,
        resource: Handle,
    ) -> Result<(), Errno> {
        let span = self.framebuffer_ram.ok_or(Errno::NotSupported)?.span();
        let handle = match self.dispatch(
            process,
            KernelCall::ResourceMintMmio {
                resource,
                phys_base: span.start,
                len: span.len,
            },
        ) {
            KernelCallResult::Handle(h) => h,
            _ => return Err(Errno::Internal),
        };
        let koid = process
            .handles()
            .get(handle)
            .map_err(errno_from_object)?
            .koid;
        let owner = Rc::downgrade(
            self.vmos
                .iter()
                .find(|v| v.koid == koid)
                .ok_or(Errno::Internal)?,
        );
        let pin = self.pin_device_vmo_read_only(process, handle, 0, span.len, Rights::READ)?;
        let mut mappings = crate::device_memory::OfflineMappings::new(&self.table_pages)?;
        mappings.map(span.start, pin)?;
        if self.dispatch(process, KernelCall::HandleClose { handle })
            != KernelCallResult::Status(Errno::Ok.status())
        {
            return Err(Errno::Internal);
        }
        self.vmos.retain(|v| v.koid != koid);
        let published = mappings.publish(|page| {
            kumo_hal::active::clean_dcache_to_poc(
                phys_ptr_mut(page) as usize,
                crate::mm::PAGE_SIZE as usize,
            );
        });
        let root = published.root();
        let boot = self.boot_info.ok_or(Errno::Internal)?;
        let report = match kumo_hal::active::display_directory_reset_probe(boot.platform.dtb, root)
        {
            Ok(report) => report,
            Err(error) => {
                crate::klog_at!(kumo_abi::sys::log_level::ERR,
                    "DISPLAY ROOT RESET Refuse   {:?}; owned root {:#x}, tables and GOP pins retained\n", error, root);
                return Err(Errno::BadState);
            }
        };
        let mut offline = published
            .reclaim(report.detached)
            .map_err(|_| Errno::BadState)?;
        offline.unmap(span.start, span.len)?;
        drop(offline);
        if owner.upgrade().is_some() || !self.table_pages.all_free() {
            return Err(Errno::Internal);
        }
        crate::klog_at!(kumo_abi::sys::log_level::NOTICE,
            "DISPLAY ROOT RESET Check    owned {:#x}, both roots {:#x}/{:#x}->0/0, status {:#x}/{:#x}, hold {:#x}, BIU idle {:#x}; VP2 {:#x} resumed; closed-handle GOP pins/tables reclaimed; no paging\n",
            root, report.published[0], report.published[1], report.reset[0].status, report.reset[1].status,
            report.stop.hold, report.stop.bus.idle, report.resumed.ports[2]);
        Ok(())
    }

    /// Prove the offline table provider on actual kernel RAM before Sora runs. The data
    /// page is the existing boot-info VMO; no MMU root, user table handle or device command
    /// is published. This is backing/alias/reuse evidence, not DMA confinement. — KESTREL
    #[cfg(all(feature = "arch_aarch64", target_os = "none"))]
    pub(crate) fn verify_private_device_tables(
        &mut self,
        process: &mut Process,
        resource: Handle,
        data_vmo: Handle,
    ) -> Result<(), Errno> {
        use kumo_hal::active::{DeviceTableAccess as Access, OfflineDeviceTable as Table};
        let entry = process
            .handles()
            .require(data_vmo, ObjectKind::Vmo, Rights::READ)
            .map_err(errno_from_object)?;
        let data = self.vmo_by_koid(entry.koid).ok_or(Errno::BadHandle)?;
        let crate::mm::VmoBacking::PhysicalRam { phys_base } = data.backing() else {
            return Err(Errno::BadState);
        };
        let pool = self.table_pages.clone();
        let mut table = Table::new(pool.lease()).map_err(|_| Errno::NoMemory)?;
        table
            .map_read_only(0x003f_f000, &[phys_base, phys_base])
            .map_err(|_| Errno::Internal)?;
        let expected = unsafe { core::ptr::read_volatile(phys_ptr_mut(phys_base).cast::<u32>()) };
        for iova in [0x003f_f000, 0x0040_0000] {
            let phys = table
                .walk(iova, Access::Read)
                .map_err(|_| Errno::Internal)?;
            if phys != Some(phys_base)
                || table.walk(iova, Access::Write).is_ok()
                || unsafe { core::ptr::read_volatile(phys_ptr_mut(phys.unwrap()).cast::<u32>()) }
                    != expected
            {
                return Err(Errno::Internal);
            }
        }
        let frames = pool.frames();
        if frames.len() != 3 {
            return Err(Errno::Internal);
        }
        let handles = process.handles().live_count();
        let vmos = self.vmos.len();
        for &phys in &frames {
            for constructor in [
                Vmo::from_physical_range,
                Vmo::from_mmio_range,
                Vmo::from_dma_range,
            ] {
                let vmo = constructor(phys, crate::mm::PAGE_SIZE).map_err(|_| Errno::Internal)?;
                if self.root_vmo_create(process, vmo, Rights::READ | Rights::WRITE)
                    != Err(ObjectError::AccessDenied)
                {
                    return Err(Errno::Internal);
                }
            }
            if self.dispatch(
                process,
                KernelCall::ResourceMintMmio {
                    resource,
                    phys_base: phys,
                    len: crate::mm::PAGE_SIZE,
                },
            ) != KernelCallResult::Status(Errno::AccessDenied.status())
            {
                return Err(Errno::Internal);
            }
        }
        // Reject an adjacent coarse alias as well as exact table-page aliases.
        let block = frames[0] & !((1 << 21) - 1);
        let neighbor = (0..512)
            .map(|i| block + i * crate::mm::PAGE_SIZE)
            .find(|phys| !frames.contains(phys))
            .ok_or(Errno::Internal)?;
        if self.dispatch(
            process,
            KernelCall::ResourceMintMmio {
                resource,
                phys_base: neighbor,
                len: crate::mm::PAGE_SIZE,
            },
        ) != KernelCallResult::Status(Errno::AccessDenied.status())
        {
            return Err(Errno::Internal);
        }
        // This boot-only proof runs on the identity tree. Exercise the same allocation
        // primitive as VmoCreateContiguous while tables are live in private RAM. — KESTREL
        let boot = self.boot_info.ok_or(Errno::Internal)?;
        let excluded = pool.dma_exclusions();
        let dma = unsafe { crate::mm::alloc_contiguous_frames_excluding(&boot, 1, &excluded) }
            .ok_or(Errno::NoMemory)?;
        let dma_vmo =
            Vmo::from_dma_range(dma, crate::mm::PAGE_SIZE).map_err(|_| Errno::Internal)?;
        if pool.prepare_vmo(dma_vmo).is_err()
            || table
                .walk(0x0040_0000, Access::Read)
                .map_err(|_| Errno::Internal)?
                != Some(phys_base)
        {
            return Err(Errno::Internal);
        }
        table
            .unmap(0x003f_f000, 2 * crate::mm::PAGE_SIZE)
            .map_err(|_| Errno::Internal)?;
        if table
            .walk(0x003f_f000, Access::Read)
            .map_err(|_| Errno::Internal)?
            .is_some()
        {
            return Err(Errno::Internal);
        }
        drop(table);
        if !pool.all_free() {
            return Err(Errno::Internal);
        }
        // Free pool pages remain private, and a new owner must reuse the same frames.
        // This also proves that release did not return them to ordinary allocation. — KESTREL
        for &phys in &frames {
            if self.dispatch(
                process,
                KernelCall::ResourceMintMmio {
                    resource,
                    phys_base: phys,
                    len: crate::mm::PAGE_SIZE,
                },
            ) != KernelCallResult::Status(Errno::AccessDenied.status())
            {
                return Err(Errno::Internal);
            }
        }
        let source = process
            .handles_mut()
            .duplicate(data_vmo, Rights::READ)
            .map_err(errno_from_object)?;
        let data_koid = process
            .handles()
            .get(data_vmo)
            .map_err(errno_from_object)?
            .koid;
        let owner = self
            .vmos
            .iter()
            .find(|v| v.koid == data_koid)
            .ok_or(Errno::Internal)?
            .clone();
        let refs = Rc::strong_count(&owner);
        let mut again = crate::device_memory::OfflineMappings::new(&pool)?;
        for iova in [0x003f_f000, 0x0040_0000] {
            let pin = self.pin_device_vmo_read_only(
                process,
                source,
                0,
                crate::mm::PAGE_SIZE,
                Rights::READ,
            )?;
            again.map(iova, pin)?;
        }
        if self.dispatch(process, KernelCall::HandleClose { handle: source })
            != KernelCallResult::Status(Errno::Ok.status())
            || self
                .pin_device_vmo_read_only(process, source, 0, crate::mm::PAGE_SIZE, Rights::READ)
                .is_ok()
            || Rc::strong_count(&owner) != refs + 2
        {
            return Err(Errno::Internal);
        }
        let record = again.mapping(0x0040_0000).ok_or(Errno::Internal)?;
        if record.koid() != data_koid
            || record.offset() != 0
            || record.len() != crate::mm::PAGE_SIZE
            || record.pages() != [phys_base]
            || again.walk(0x0040_0000, Access::Read)? != Some(phys_base)
            || again.walk(0x0040_0000, Access::Write) != Err(Errno::AccessDenied)
            || again.unmap(0x003f_f000, 2 * crate::mm::PAGE_SIZE) != Err(Errno::InvalidArgs)
            || Rc::strong_count(&owner) != refs + 2
        {
            return Err(Errno::Internal);
        }
        again.unmap(0x003f_f000, crate::mm::PAGE_SIZE)?;
        if again.walk(0x003f_f000, Access::Read)?.is_some()
            || again.walk(0x0040_0000, Access::Read)? != Some(phys_base)
            || Rc::strong_count(&owner) != refs + 1
            || pool.frames() != frames
            || process.handles().live_count() != handles
            || self.vmos.len() != vmos
        {
            return Err(Errno::Internal);
        }
        drop(again);
        if !pool.all_free() || Rc::strong_count(&owner) != refs {
            return Err(Errno::Internal);
        }
        crate::klog_at!(kumo_abi::sys::log_level::NOTICE,
            "DEVICE PINS       Check     VMO identity, closed handle, exact unmap, offline release OK\n");
        self.verify_device_context_references()?;
        self.verify_device_context_retirement(process, resource, data_vmo)?;
        self.verify_device_retention_restart()?;
        Ok(())
    }

    /// Actual RAM, two disposable engine generations, no device publication. Put
    /// canaries in future usable pages so both allocators must skip before zeroing.
    /// The retained root is a synthetic unsupported domain, not a live MMU. — KESTREL
    #[cfg(all(feature = "arch_aarch64", target_os = "none"))]
    fn verify_device_retention_restart(&mut self) -> Result<(), Errno> {
        use crate::mm::PAGE_SIZE as P;
        let boot = self.boot_info.ok_or(Errno::Internal)?;
        let plan = unsafe { crate::mm::KernelMemoryPlan::from_boot_info(&boot) };
        let mut selected = None;
        for _ in 0..1024 {
            let root = unsafe { crate::mm::alloc_zeroed_frame(&boot) }.ok_or(Errno::NoMemory)?;
            if root / (1 << 21) != (root + 3 * P) / (1 << 21) {
                continue;
            }
            let mut frames = plan.frame_allocator();
            let mut mask = 0;
            while let Some(frame) = frames.next_frame() {
                if frame.start > root + 3 * P {
                    break;
                }
                for n in 1..=3 {
                    if frame.start == root + n * P {
                        mask |= 1 << n;
                    }
                }
            }
            if mask == 14 {
                selected = Some(root);
                break;
            }
        }
        let root = selected.ok_or(Errno::NoMemory)?;
        let mut old = Self::with_device_owner(self.device_owner.clone());
        let job = Job::root(&mut old.objects);
        let mut process = Process::new(
            &mut old.objects,
            &job,
            Vmar::new(0, 1 << 32).map_err(|_| Errno::Internal)?,
        );
        let unit = old.objects.create(ObjectKind::IoMmu);
        old.devices.iommus.push(IoMmuBinding {
            koid: unit.koid(),
            iommu_kind: u32::MAX,
            phys_base: 0x1000,
            len: P,
        });
        let object = old.objects.create(ObjectKind::DeviceCtx);
        let (_, references) = process
            .handles_mut()
            .insert_device_context(object, Rights::MAP)
            .map_err(errno_from_object)?;
        old.devices.device_ctxs.push(DeviceCtxBinding {
            koid: object.koid(),
            iommu_koid: unit.koid(),
            stream_id: 0,
            pgd_phys: root,
            references,
            state: Rc::new(Cell::new(DeviceCtxState::Bound)),
        });
        let mut pins = Vec::new();
        for n in [1, 3] {
            let vmo = Vmo::from_physical_range(root + n * P, P).map_err(|_| Errno::Internal)?;
            let handle = old
                .root_vmo_create(&mut process, vmo, Rights::READ)
                .map_err(errno_from_object)?;
            let koid = process
                .handles()
                .get(handle)
                .map_err(errno_from_object)?
                .koid;
            let backing = old
                .vmos
                .iter()
                .find(|v| v.koid == koid)
                .cloned()
                .ok_or(Errno::Internal)?;
            pins.push(Rc::downgrade(&backing));
            old.devices.device_vmar_mappings.push(DeviceVmarMapping {
                ctx_koid: object.koid(),
                iova: n * P,
                len: P,
                vmo_offset: 0,
                backing,
            });
        }
        for n in [0, 1, 3] {
            unsafe {
                core::ptr::write_volatile(phys_ptr_mut(root + n * P).cast::<u64>(), 0x6200_0000 + n)
            };
        }
        drop(old);
        drop(process);
        let mut fresh = Self::with_device_owner(self.device_owner.clone());
        let job = Job::root(&mut fresh.objects);
        let mut process = Process::new(
            &mut fresh.objects,
            &job,
            Vmar::new(0, 1 << 32).map_err(|_| Errno::Internal)?,
        );
        let resource = fresh
            .root_resource_create(&mut process, 0, u64::MAX, 0, 0)
            .map_err(errno_from_object)?;
        if fresh.dispatch(
            &mut process,
            KernelCall::IoMmuFromResource {
                resource,
                kind: u32::MAX,
                phys_base: 0x1000,
                len: P,
            },
        ) != KernelCallResult::Status(Errno::BadState.status())
        {
            return Err(Errno::Internal);
        }
        let mut private = self.table_pages.frames();
        private.extend([root, root + P, root + 3 * P]);
        for phys in private {
            for constructor in [
                Vmo::from_physical_range,
                Vmo::from_mmio_range,
                Vmo::from_dma_range,
            ] {
                if fresh.root_vmo_create(
                    &mut process,
                    constructor(phys, P).map_err(|_| Errno::Internal)?,
                    Rights::READ,
                ) != Err(ObjectError::AccessDenied)
                {
                    return Err(Errno::Internal);
                }
            }
        }
        let gap = unsafe { crate::mm::alloc_zeroed_frame(&boot) }.ok_or(Errno::NoMemory)?;
        if gap != root + 2 * P {
            return Err(Errno::Internal);
        }
        let dma = unsafe {
            crate::mm::alloc_contiguous_frames_excluding(
                &boot,
                1,
                &self.table_pages.dma_exclusions(),
            )
        }
        .ok_or(Errno::NoMemory)?;
        if dma / (1 << 21) == root / (1 << 21) {
            return Err(Errno::Internal);
        }
        for n in [0, 1, 3] {
            if unsafe { core::ptr::read_volatile(phys_ptr_mut(root + n * P).cast::<u64>()) }
                != 0x6200_0000 + n
            {
                return Err(Errno::Internal);
            }
        }
        if pins.iter().any(|pin| pin.upgrade().is_none()) {
            return Err(Errno::Internal);
        }
        crate::klog_at!(kumo_abi::sys::log_level::NOTICE,
            "DEVICE RETAIN     Check     engine replacement, roots/backing held, alias denial, pre-zero gap OK\n");
        Ok(())
    }

    /// Test the refusal path with an unsupported synthetic unit, no allocated device
    /// root and no hardware publication. Retain one real VMO reference in quarantine.
    /// This is an engine-local lifetime proof, not verified DMA blocking. — KESTREL
    #[cfg(all(feature = "arch_aarch64", target_os = "none"))]
    fn verify_device_context_retirement(
        &mut self,
        process: &mut Process,
        resource: Handle,
        data_vmo: Handle,
    ) -> Result<(), Errno> {
        let data_koid = process
            .handles()
            .require(data_vmo, ObjectKind::Vmo, Rights::READ)
            .map_err(errno_from_object)?
            .koid;
        let backing = self
            .vmos
            .iter()
            .find(|v| v.koid == data_koid)
            .cloned()
            .ok_or(Errno::Internal)?;
        let refs = Rc::strong_count(&backing);
        let unit = self.objects.create(ObjectKind::IoMmu);
        self.devices.iommus.push(IoMmuBinding {
            koid: unit.koid(),
            iommu_kind: u32::MAX,
            phys_base: 0,
            len: crate::mm::PAGE_SIZE,
        });
        let object = self.objects.create(ObjectKind::DeviceCtx);
        let (handle, references) = process
            .handles_mut()
            .insert_device_context(object, Rights::MAP | Rights::WAIT | Rights::DUPLICATE)
            .map_err(errno_from_object)?;
        let state = Rc::new(Cell::new(DeviceCtxState::Bound));
        self.devices.device_ctxs.push(DeviceCtxBinding {
            koid: object.koid(),
            iommu_koid: unit.koid(),
            stream_id: 0,
            pgd_phys: 0,
            references,
            state: state.clone(),
        });
        // This record owns real backing but represents no programmed PTE. — KESTREL
        self.devices.device_vmar_mappings.push(DeviceVmarMapping {
            ctx_koid: object.koid(),
            iova: 0x1000,
            len: crate::mm::PAGE_SIZE,
            vmo_offset: 0,
            backing: backing.clone(),
        });
        let duplicate = process
            .handles_mut()
            .duplicate(handle, Rights::WAIT)
            .map_err(errno_from_object)?;
        if self.dispatch(process, KernelCall::HandleClose { handle })
            != KernelCallResult::Status(Errno::Ok.status())
            || state.get() != DeviceCtxState::Bound
        {
            return Err(Errno::Internal);
        }
        if self.dispatch(process, KernelCall::HandleClose { handle: duplicate })
            != KernelCallResult::Status(Errno::Ok.status())
            || state.get() != DeviceCtxState::Quarantined
            || self.device_ctx_has_capabilities(object.koid())?
            || Rc::strong_count(&backing) != refs + 1
            || self.retire_device_contexts() != DeviceCtxRetirement::default()
        {
            return Err(Errno::Internal);
        }
        if self.dispatch(
            process,
            KernelCall::IoMmuFromResource {
                resource,
                kind: u32::MAX,
                phys_base: 0,
                len: crate::mm::PAGE_SIZE,
            },
        ) != KernelCallResult::Status(Errno::BadState.status())
        {
            return Err(Errno::Internal);
        }
        let record = self
            .devices
            .device_vmar_mappings
            .iter()
            .find(|m| m.ctx_koid == object.koid())
            .ok_or(Errno::Internal)?;
        if record.backing.koid != data_koid || record.vmo_offset != 0 {
            return Err(Errno::Internal);
        }
        crate::klog_at!(kumo_abi::sys::log_level::NOTICE,
            "DEVICE RETIRE     Check     checked refusal, final close, backing retained, unit reserved OK\n");
        Ok(())
    }

    /// Exercise capability transport only, without constructing a backend binding,
    /// allocating a device root or granting a driver any new authority. — KESTREL
    #[cfg(all(feature = "arch_aarch64", target_os = "none"))]
    fn verify_device_context_references(&mut self) -> Result<(), Errno> {
        let job = Job::root(&mut self.objects);
        let vmar = Vmar::new(0, 1 << 32).map_err(|_| Errno::Internal)?;
        let mut source = Process::new(&mut self.objects, &job, vmar);
        let mut target = Process::new(&mut self.objects, &job, vmar);
        let object = self.objects.create(ObjectKind::DeviceCtx);
        let (handle, references) = source
            .handles_mut()
            .insert_device_context(object, Rights::MAP | Rights::DUPLICATE | Rights::TRANSFER)
            .map_err(errno_from_object)?;
        let duplicate = source
            .handles_mut()
            .duplicate(handle, Rights::MAP | Rights::TRANSFER)
            .map_err(errno_from_object)?;
        let mut ipc = IpcRegistry::new();
        let (left, right) = ipc
            .channel_create(&mut self.objects, &mut source)
            .map_err(errno_from_ipc)?;
        let grant = source
            .handles()
            .stage_grant_to(target.handles_mut(), right, true)
            .map_err(errno_from_object)?;
        source
            .handles_mut()
            .commit_grant(grant)
            .map_err(errno_from_object)?;
        ipc.channel_write(
            &mut source,
            left,
            Message::new(0, b"reference proof", &[duplicate]).map_err(|_| Errno::Internal)?,
        )
        .map_err(errno_from_ipc)?;
        drop(source.handles_mut().drain());
        drop(source);
        if !references.is_referenced() {
            return Err(Errno::Internal);
        }
        let message = ipc
            .channel_read(&mut target, grant.target())
            .map_err(errno_from_ipc)?;
        let received = *message.handles().first().ok_or(Errno::Internal)?;
        let entry = target
            .handles()
            .require(received, ObjectKind::DeviceCtx, Rights::MAP)
            .map_err(errno_from_object)?;
        if entry.koid != object.koid() || !references.is_referenced() {
            return Err(Errno::Internal);
        }
        let drained = target.handles_mut().drain();
        if !references.is_referenced() {
            return Err(Errno::Internal);
        }
        drop(drained);
        if references.is_referenced() {
            return Err(Errno::Internal);
        }
        crate::klog_at!(kumo_abi::sys::log_level::NOTICE,
            "DEVICE REFS       Check     duplicate, queued transfer, process drain, final drop OK\n");
        Ok(())
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
        self.vmos
            .iter()
            .find(|entry| entry.koid == koid)
            .map(Rc::as_ref)
    }

    fn device_dma_phys_page(&mut self, vmo_koid: KoId, vmo_offset: u64) -> Result<u64, Errno> {
        let Some(vmo_entry) = self.vmo_entry_by_koid(vmo_koid) else {
            return Err(Errno::BadHandle);
        };
        if !self.physical_access_allowed(vmo_entry, vmo_offset, crate::mm::PAGE_SIZE, false) {
            return Err(Errno::AccessDenied);
        }
        match vmo_entry.vmo.backing() {
            crate::mm::VmoBacking::PhysicalRam { phys_base }
            | crate::mm::VmoBacking::Mmio { phys_base }
            | crate::mm::VmoBacking::Dma { phys_base } => {
                phys_base.checked_add(vmo_offset).ok_or(Errno::InvalidArgs)
            }
            crate::mm::VmoBacking::Anonymous => {
                let Some(boot) = self.boot_info else {
                    return Err(Errno::Internal);
                };
                let page_index = (vmo_offset / crate::mm::PAGE_SIZE) as usize;
                let Some(vmo_entry) = self.vmo_entry_by_koid(vmo_koid) else {
                    return Err(Errno::BadHandle);
                };
                ensure_anonymous_frame(vmo_entry, page_index, &boot)
            }
        }
    }

    fn device_vmar_backend_map(
        &mut self,
        ctx: &DeviceCtxBinding,
        iommu: IoMmuBinding,
        vmo_koid: KoId,
        vmo_offset: u64,
        len: u64,
        iova: u64,
        rights: Rights,
    ) -> Result<(), Errno> {
        let pages = len / crate::mm::PAGE_SIZE;
        let mut page = 0;
        while page < pages {
            let off = page * crate::mm::PAGE_SIZE;
            let phys = match self.device_dma_phys_page(vmo_koid, vmo_offset + off) {
                Ok(phys) => phys,
                Err(errno) => {
                    self.device_vmar_backend_unmap_prefix(ctx, iommu, iova, page);
                    return Err(errno);
                }
            };
            if !kumo_hal::active::iommu_map_device_page(
                iommu.iommu_kind,
                iommu.phys_base,
                ctx.stream_id,
                ctx.pgd_phys,
                iova + off,
                phys,
                rights.bits(),
            ) {
                self.device_vmar_backend_unmap_prefix(ctx, iommu, iova, page);
                return Err(Errno::NotSupported);
            }
            page += 1;
        }
        Ok(())
    }

    fn device_vmar_backend_unmap_prefix(
        &self,
        ctx: &DeviceCtxBinding,
        iommu: IoMmuBinding,
        iova: u64,
        pages: u64,
    ) {
        if pages == 0 {
            return;
        }
        let len = pages * crate::mm::PAGE_SIZE;
        let _ = kumo_hal::active::iommu_unmap_device_range(
            iommu.iommu_kind,
            iommu.phys_base,
            ctx.stream_id,
            ctx.pgd_phys,
            iova,
            len,
        );
    }

    fn device_vmar_backend_unmap_range(
        &self,
        ctx: &DeviceCtxBinding,
        iommu: IoMmuBinding,
        iova: u64,
        len: u64,
    ) -> Result<(), Errno> {
        if kumo_hal::active::iommu_unmap_device_range(
            iommu.iommu_kind,
            iommu.phys_base,
            ctx.stream_id,
            ctx.pgd_phys,
            iova,
            len,
        ) {
            Ok(())
        } else {
            Err(Errno::NotSupported)
        }
    }

    /// Write a single mapping into an already-live page table tree.
    /// Called from [`VmarMap`] when the target process's TTBR0 is populated.
    /// Mirrors [`AddressSpaceCreate`] but operates on one mapping at a time,
    /// using 4 KiB pages (not 2 MiB blocks) to avoid clobbering adjacent VAs
    /// that share a 2 MiB region (stack, data, other mappings).
    ///
    /// The caller must switch TTBR0 to the kernel identity map before calling
    /// (so page-table frame writes land by physical address), and restore the
    /// user tree afterward (the `AddressSpaceCreate` pattern at lines ~993-999).
    fn apply_to_live_tree(
        &mut self,
        ttbr0: u64,
        mapping: Mapping,
        vmo_koid: KoId,
    ) -> Result<(), Errno> {
        let boot = self.boot_info;
        let Some(ref boot_ref) = boot else {
            return Err(Errno::Internal);
        };
        let executable = mapping.flags.contains(PageFlags::EXECUTE);
        // W^X: writable wins → non-executable (matches user_page_desc).
        let writable = mapping.flags.contains(PageFlags::WRITE) && !executable;
        let device = mapping.flags.contains(PageFlags::DEVICE);
        let uncached = mapping.flags.contains(PageFlags::UNCACHED);
        if device && uncached {
            return Err(Errno::InvalidArgs);
        }

        let Some(vmo_entry) = self.vmo_entry_by_koid(vmo_koid) else {
            return Err(Errno::BadHandle);
        };

        // Choose the page descriptor from both the backing kind and mapping policy.
        if !self.physical_access_allowed(vmo_entry, mapping.vmo_offset, mapping.len, true) {
            return Err(Errno::AccessDenied);
        }
        // Physical RAM must remain Normal-WB to match the kernel alias (notably the
        // initrd used by drv-blk). Resource-minted MMIO defaults to Device-nGnRnE;
        // only framebuffer callers opt into Normal-NC.
        let desc: u64;
        let needs_frame_alloc: bool;
        match vmo_entry.vmo.backing() {
            crate::mm::VmoBacking::PhysicalRam { .. } => {
                if device || uncached {
                    return Err(Errno::InvalidArgs);
                }
                desc = kumo_hal::active::user_page_desc(executable, writable);
                needs_frame_alloc = false;
            }
            crate::mm::VmoBacking::Mmio { .. } => {
                if executable {
                    return Err(Errno::InvalidArgs);
                }
                desc = if uncached {
                    kumo_hal::active::user_nc_page_desc(writable)
                } else {
                    kumo_hal::active::user_device_page_desc(writable)
                };
                needs_frame_alloc = false;
            }
            crate::mm::VmoBacking::Dma { .. } => {
                // DMA RAM is Normal memory, never Device, and never executable. Uncached
                // (Normal-NC) is the coherency-free default a driver asks for; a cacheable
                // request stays Normal-WB for the future coherent-DMA path.
                if device || executable {
                    return Err(Errno::InvalidArgs);
                }
                desc = if uncached {
                    kumo_hal::active::user_nc_page_desc(writable)
                } else {
                    kumo_hal::active::user_page_desc(false, writable)
                };
                needs_frame_alloc = false;
            }
            crate::mm::VmoBacking::Anonymous => {
                if device || uncached {
                    return Err(Errno::InvalidArgs);
                }
                desc = kumo_hal::active::user_page_desc(executable, writable);
                needs_frame_alloc = true;
            }
        }

        if mapping.len % crate::mm::PAGE_SIZE != 0 {
            return Err(Errno::InvalidArgs);
        }
        let pages = mapping.len / crate::mm::PAGE_SIZE;
        let phys_base = match vmo_entry.vmo.backing() {
            crate::mm::VmoBacking::PhysicalRam { phys_base }
            | crate::mm::VmoBacking::Mmio { phys_base }
            | crate::mm::VmoBacking::Dma { phys_base } => phys_base,
            crate::mm::VmoBacking::Anonymous => 0,
        };

        let mut alloc = || unsafe { crate::mm::alloc_zeroed_frame(boot_ref) };
        let mut tables: usize = 0;
        let mut page: u64 = 0;
        while page < pages {
            let off = page * crate::mm::PAGE_SIZE;
            let va = mapping.virt + off;
            let pa = if needs_frame_alloc {
                let vmo_page = ((mapping.vmo_offset + off) / crate::mm::PAGE_SIZE) as usize;
                let Some(vmo_entry) = self.vmo_entry_by_koid(vmo_koid) else {
                    return Err(Errno::BadHandle);
                };
                match ensure_anonymous_frame(vmo_entry, vmo_page, boot_ref) {
                    Ok(frame) => frame,
                    Err(errno) => return Err(errno),
                }
            } else {
                phys_base + mapping.vmo_offset + off
            };

            // SAFETY: caller switched TTBR0 to kernel identity map; `root` (ttbr0)
            // is the user tree root, and all intermediate table frames come from
            // `alloc` which returns identity-mapped RAM (J153 invariant).
            unsafe {
                kumo_hal::active::map_user_page(ttbr0, va, pa, desc, &mut alloc, &mut tables)
            }
            .map_err(|_| Errno::NoMemory)?;

            // Cache-maintain cacheable-RAM pages so EL0 sees what was written. The child's
            // segment bytes were written through a *data* mapping (Sora's copy / the anon
            // frame's zeroing); on a core with no I/D coherency (X13s) EL0 would otherwise
            // read stale RAM, and for code FETCH stale instructions — running garbage where
            // a register holds a value no instruction wrote (the J… double-TOWER). EL0 can't
            // self-clean (UCI=0 traps `dc cvac` at EL0), so the kernel does it here, the same
            // way `build_user_space` flushes the first image. Device/MMIO/uncached pages are
            // not cacheable RAM and are skipped. `pa` is identity-mapped (cacheable) here.
            if !device && !uncached {
                if executable {
                    kumo_hal::active::sync_icache_to_pou(
                        pa as usize,
                        crate::mm::PAGE_SIZE as usize,
                    );
                } else {
                    kumo_hal::active::clean_dcache_to_poc(
                        pa as usize,
                        crate::mm::PAGE_SIZE as usize,
                    );
                }
            }

            page += 1;
        }
        Ok(())
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
        // MANAGE is the creator's authority over the child's lifetime: `ProcessKill` (J556)
        // requires it, and a duplicate made without it cannot end the process.
        let handle = caller.handles_mut().insert(
            child.object(),
            Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::WAIT | Rights::MANAGE,
        )?;
        self.processes.push(child);
        Ok(handle)
    }

    /// The process `ProcessKill` would end: `handle` must name a live process in `caller`'s
    /// table with `MANAGE`. A process may not kill itself; it exits instead. Pure, so the
    /// policy is host-tested apart from the scheduler half of the kill.
    pub fn process_kill_target(&self, caller: &Process, handle: Handle) -> Result<KoId, Errno> {
        let entry = caller
            .handles()
            .require(handle, ObjectKind::Process, Rights::MANAGE)
            .map_err(errno_from_object)?;
        if entry.koid == caller.koid() {
            return Err(Errno::InvalidArgs);
        }
        if self.process_by_koid(entry.koid).is_none() {
            return Err(Errno::BadHandle);
        }
        Ok(entry.koid)
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
        let result = self.dispatch_inner(process, call);
        // The inner call has dropped its removed entries. This also catches grants
        // discarded by IPC before returning to userspace. — KESTREL
        self.retire_device_contexts();
        self.retire_interrupts();
        result
    }

    fn dispatch_inner(&mut self, process: &mut Process, call: KernelCall<'_>) -> KernelCallResult {
        match call {
            KernelCall::HandleClose { handle } => {
                let status = match process.handles_mut().remove(handle) {
                    Ok(entry) => {
                        if entry.kind == ObjectKind::Channel {
                            if let Ok(Some(peer_koid)) = self.ipc.close_by_koid(entry.koid) {
                                self.signal_ports(peer_koid, kumo_abi::Signals::PEER_CLOSED);
                                // J547: a child parked in `ChannelRead` on the surviving end
                                // waits on its endpoint, not a port. Wake it as a write would, so
                                // its retried read sees `PeerClosed` instead of sleeping forever.
                                // — TIMBERDOODLE
                                #[cfg(target_os = "none")]
                                crate::user_thread::wake_child_waiting_on_channel(peer_koid);
                            }
                        }
                        if entry.kind == ObjectKind::Timer {
                            self.release_timer(entry.koid);
                            self.release_port_bindings(entry.koid);
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
            KernelCall::ChannelRead { channel } => {
                // Resolve the endpoint koid before the read borrows `process`, so the
                // re-arm below can re-signal its bound port (see
                // `rearm_port_if_channel_readable`).
                let read_koid = process.handles().get(channel).ok().map(|entry| entry.koid);
                match self.ipc.channel_read(process, channel) {
                    Ok(message) => {
                        if let Some(koid) = read_koid {
                            self.rearm_port_if_channel_readable(koid);
                        }
                        KernelCallResult::Message(message)
                    }
                    Err(error) => KernelCallResult::Status(errno_from_ipc(error).status()),
                }
            }
            KernelCall::PortCreate => match self.ipc.port_create(&mut self.objects, process) {
                Ok(handle) => KernelCallResult::Handle(handle),
                Err(error) => KernelCallResult::Status(errno_from_ipc(error).status()),
            },
            KernelCall::PortWait { port } => match self.ipc.port_wait(process, port) {
                Ok(packet) => KernelCallResult::PortPacket(packet),
                Err(error) => KernelCallResult::Status(errno_from_ipc(error).status()),
            },
            KernelCall::TimerCreate { deadline_ns } => {
                if deadline_ns == 0 {
                    return KernelCallResult::Status(Errno::InvalidArgs.status());
                }
                let object = self.objects.create(ObjectKind::Timer);
                match process
                    .handles_mut()
                    .insert(object, Rights::WAIT | Rights::TRANSFER)
                {
                    Ok(handle) => {
                        self.timers.push(TimerBinding {
                            koid: object.koid(),
                            deadline_ns,
                            fired: false,
                        });
                        KernelCallResult::Handle(handle)
                    }
                    Err(error) => KernelCallResult::Status(errno_from_object(error).status()),
                }
            }
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
                            } else if !self
                                .physical_access_allowed(vmo_entry, offset, len as u64, false)
                            {
                                Errno::AccessDenied.status()
                            } else {
                                let dest_slice =
                                    unsafe { core::slice::from_raw_parts_mut(dest, len) };
                                match vmo_entry.vmo.backing() {
                                    crate::mm::VmoBacking::PhysicalRam { phys_base }
                                    | crate::mm::VmoBacking::Mmio { phys_base }
                                    | crate::mm::VmoBacking::Dma { phys_base } => {
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
                                                vmo_entry.frames.borrow().get(page_index)
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
                        } else if !physical_vmo_write_allowed(&vmo_entry.vmo, offset, len as u64) {
                            Errno::AccessDenied.status()
                        } else if !self
                            .physical_access_allowed(vmo_entry, offset, len as u64, false)
                        {
                            Errno::AccessDenied.status()
                        } else {
                            let backing = vmo_entry.vmo.backing();
                            let src_slice = unsafe { core::slice::from_raw_parts(src, len) };
                            match backing {
                                crate::mm::VmoBacking::PhysicalRam { phys_base }
                                | crate::mm::VmoBacking::Mmio { phys_base }
                                | crate::mm::VmoBacking::Dma { phys_base } => {
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
                                    let Some(vmo_entry) = self.vmo_entry_by_koid(handle_entry.koid)
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
                // DESIGN/020 D0-1: a writable mapping needs a writable handle, as
                // DeviceVmarMap already requires for DMA. Before J605 READ alone let any VMO
                // handle, even one narrowed to READ, be mapped writable. — TIMBERDOODLE
                let needed = if flags.contains(PageFlags::WRITE) {
                    Rights::READ | Rights::WRITE
                } else {
                    Rights::READ
                };
                let (vmo_koid, vmo) =
                    match process
                        .handles()
                        .require(vmo_handle, ObjectKind::Vmo, needed)
                    {
                        Ok(entry) => (entry.koid, self.vmo_by_koid(entry.koid)),
                        Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                    };
                let Some(vmo) = vmo else {
                    return KernelCallResult::Status(Errno::BadHandle.status());
                };
                if !self
                    .vmo_entry_by_koid(vmo_koid)
                    .is_some_and(|entry| self.physical_access_allowed(entry, vmo_offset, len, true))
                {
                    return KernelCallResult::Status(Errno::AccessDenied.status());
                }
                if flags.contains(PageFlags::WRITE)
                    && !physical_vmo_write_allowed(&vmo, vmo_offset, len)
                {
                    return KernelCallResult::Status(Errno::AccessDenied.status());
                }
                if process_handle == kumo_abi::INVALID_HANDLE {
                    // Self-map: map into the calling process's own VMAR.
                    //
                    // `virt == 0` asks the kernel to place the mapping. Userland cannot choose a
                    // base safely — a child's VMAR is 512 MiB from 0 carrying its image, stacks and
                    // device maps, Sora's is 2 GiB from another base — and the kernel is the only
                    // party that knows every existing mapping. Address 0 is a safe sentinel because
                    // the first page is never handed out: a null dereference must keep faulting.
                    let virt = if virt == 0 {
                        match process.find_free_span(len, crate::mm::PAGE_SIZE) {
                            Some(chosen) => chosen,
                            None => return KernelCallResult::Status(Errno::NoMemory.status()),
                        }
                    } else {
                        virt
                    };
                    let status = match process.root_vmar().map(vmo, vmo_offset, virt, len, flags) {
                        Ok(mapping) => {
                            if !process.user_range_is_free(mapping.virt, mapping.len) {
                                return KernelCallResult::Status(Errno::InvalidArgs.status());
                            }
                            process.add_mapping(mapping, vmo_koid);
                            // If this process is already running, eagerly write the
                            // mapping into the live page tables so the next access
                            // doesn't fault (FORECAST/001 — VmarMap was record-only).
                            if let Some(ttbr0) = process.ttbr0 {
                                #[cfg(target_os = "none")]
                                {
                                    let saved = kumo_hal::active::read_user_aspace_root();
                                    unsafe {
                                        kumo_hal::active::set_user_aspace_root(
                                            crate::user_thread::kernel_ttbr0(),
                                        )
                                    };
                                    let result = self.apply_to_live_tree(ttbr0, mapping, vmo_koid);
                                    unsafe { kumo_hal::active::set_user_aspace_root(saved) };
                                    if let Err(e) = result {
                                        return KernelCallResult::Status(e.status());
                                    }
                                }
                                #[cfg(not(target_os = "none"))]
                                {
                                    // Host tests: exercise the logic but skip
                                    // hardware table writes (stub HAL fns).
                                    let _ = self.apply_to_live_tree(ttbr0, mapping, vmo_koid);
                                }
                            }
                            Errno::Ok.status()
                        }
                        Err(_) => Errno::InvalidArgs.status(),
                    };
                    // The caller needs the address when it asked the kernel to choose, so report it
                    // alongside the status rather than making userland guess or re-query.
                    return KernelCallResult::StatusAndValue {
                        status,
                        value: virt,
                    };
                }

                // Target-child: map into a different process's VMAR.
                // Scope the target borrow tightly so we can re-borrow `self`
                // for the live-tree write (target borrows `self.processes`).
                let proc_koid = match process.handles().require(
                    process_handle,
                    ObjectKind::Process,
                    Rights::WRITE,
                ) {
                    Ok(entry) => entry.koid,
                    Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                };
                let (status, live_write) = {
                    let Some(target) = self.process_by_koid_mut(proc_koid) else {
                        return KernelCallResult::Status(Errno::BadHandle.status());
                    };
                    match target.root_vmar().map(vmo, vmo_offset, virt, len, flags) {
                        Ok(mapping) => {
                            if !target.user_range_is_free(mapping.virt, mapping.len) {
                                return KernelCallResult::Status(Errno::InvalidArgs.status());
                            }
                            target.add_mapping(mapping, vmo_koid);
                            let ttbr0 = target.ttbr0;
                            (
                                Errno::Ok.status(),
                                ttbr0.map(|root| (root, mapping, vmo_koid)),
                            )
                        }
                        Err(_) => (Errno::InvalidArgs.status(), None),
                    }
                }; // target borrow ends here
                if let Some((ttbr0, mapping, vmo_koid)) = live_write {
                    #[cfg(target_os = "none")]
                    {
                        let saved = kumo_hal::active::read_user_aspace_root();
                        unsafe {
                            kumo_hal::active::set_user_aspace_root(
                                crate::user_thread::kernel_ttbr0(),
                            )
                        };
                        let result = self.apply_to_live_tree(ttbr0, mapping, vmo_koid);
                        unsafe { kumo_hal::active::set_user_aspace_root(saved) };
                        if let Err(e) = result {
                            return KernelCallResult::Status(e.status());
                        }
                    }
                    #[cfg(not(target_os = "none"))]
                    {
                        let _ = self.apply_to_live_tree(ttbr0, mapping, vmo_koid);
                    }
                }
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
                #[cfg(target_os = "none")]
                let proc_ttbr0 = {
                    // Two-pass: first extract the process koid (mutable borrow), then
                    // look up the process's ttbr0 (immutable borrow).
                    let proc_koid = {
                        let Some(thread) = self.thread_by_koid_mut(thread_koid) else {
                            return KernelCallResult::Status(Errno::BadHandle.status());
                        };
                        thread.process()
                    };
                    self.process_by_koid(proc_koid).and_then(|p| p.ttbr0)
                };
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
                    thread.user_state = Some(user_state);
                    let state = thread
                        .user_state
                        .as_ref()
                        .expect("user state just installed")
                        as *const kumo_hal::active::UserState;
                    *thread.context_mut() = kumo_hal::active::user_entry_context(state, kernel_sp);
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
                let Some(stack_base) = stack_virt.checked_sub(stack_size) else {
                    return KernelCallResult::Status(Errno::InvalidArgs.status());
                };
                if stack_size == 0
                    || !self
                        .process_by_koid(proc_koid)
                        .is_some_and(|target| target.user_range_is_free(stack_base, stack_size))
                {
                    return KernelCallResult::Status(Errno::InvalidArgs.status());
                }
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
                    let uncached = mapping.flags.contains(PageFlags::UNCACHED);
                    if device && uncached {
                        return KernelCallResult::Status(Errno::InvalidArgs.status());
                    }
                    let Some(vmo_entry) = self.vmo_entry_by_koid(vmo_koid) else {
                        return KernelCallResult::Status(Errno::BadHandle.status());
                    };
                    if !self.physical_access_allowed(
                        vmo_entry,
                        mapping.vmo_offset,
                        mapping.len,
                        true,
                    ) {
                        return KernelCallResult::Status(Errno::AccessDenied.status());
                    }
                    match vmo_entry.vmo.backing() {
                        crate::mm::VmoBacking::PhysicalRam { phys_base } => {
                            if device || uncached {
                                return KernelCallResult::Status(Errno::InvalidArgs.status());
                            }
                            user_mappings.push(kumo_hal::active::UserMapping {
                                phys_base: phys_base + mapping.vmo_offset,
                                virt_addr: mapping.virt,
                                len: mapping.len,
                                writable,
                                device: false,
                                uncached: false,
                                executable,
                            });
                        }
                        crate::mm::VmoBacking::Mmio { phys_base } => {
                            if executable {
                                return KernelCallResult::Status(Errno::InvalidArgs.status());
                            }
                            // Device/framebuffer mappings ride a 2 MiB-aligned block slot.
                            let virt_addr = {
                                const BLOCK_MASK: u64 = (1 << 21) - 1;
                                mapping.virt & !BLOCK_MASK
                            };
                            user_mappings.push(kumo_hal::active::UserMapping {
                                phys_base: phys_base + mapping.vmo_offset,
                                virt_addr,
                                len: mapping.len,
                                writable,
                                device: !uncached,
                                uncached,
                                executable: false,
                            });
                        }
                        crate::mm::VmoBacking::Dma { phys_base } => {
                            // Contiguous DMA RAM: Normal memory (never Device), page-granular at
                            // the driver's chosen VA. Uncached → Normal-NC for coherency-free DMA.
                            if executable {
                                return KernelCallResult::Status(Errno::InvalidArgs.status());
                            }
                            user_mappings.push(kumo_hal::active::UserMapping {
                                phys_base: phys_base + mapping.vmo_offset,
                                virt_addr: mapping.virt,
                                len: mapping.len,
                                writable,
                                device: false,
                                uncached,
                                executable: false,
                            });
                        }
                        crate::mm::VmoBacking::Anonymous => {
                            if device || uncached {
                                return KernelCallResult::Status(Errno::InvalidArgs.status());
                            }
                            if mapping.len % crate::mm::PAGE_SIZE != 0 {
                                return KernelCallResult::Status(Errno::InvalidArgs.status());
                            }
                            let pages = mapping.len / crate::mm::PAGE_SIZE;
                            let mut page = 0;
                            while page < pages {
                                let off = page * crate::mm::PAGE_SIZE;
                                let vmo_page =
                                    ((mapping.vmo_offset + off) / crate::mm::PAGE_SIZE) as usize;
                                let Some(vmo_entry) = self.vmo_entry_by_koid(vmo_koid) else {
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
                                    uncached: false,
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
                            target.set_user_stack(stack_base, stack_size);
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
                    let run_flags = ProcessRunFlags(flags);
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

                    if run_flags.contains(ProcessRunFlags::TRANSFER_ARG)
                        && run_flags.contains(ProcessRunFlags::TRANSFER_ARG2)
                        && arg != 0
                        && arg == arg2
                    {
                        return KernelCallResult::Status(Errno::InvalidArgs.status());
                    }

                    // Stage both grants in the child first. A failed second grant or
                    // failed admission rolls them back without consuming either source.
                    let target_ptr = self
                        .process_by_koid(proc_koid)
                        .map(|child| child as *const Process as *mut Process)
                        .expect("ProcessRun target disappeared");
                    let (child_arg, child_arg2, grants) = unsafe {
                        let child_handles = (*target_ptr).handles_mut();
                        let (child_arg, first) = match stage_process_arg(
                            process.handles(),
                            child_handles,
                            arg,
                            run_flags.contains(ProcessRunFlags::TRANSFER_ARG),
                        ) {
                            Ok(staged) => staged,
                            Err(error) => {
                                return KernelCallResult::Status(errno_from_object(error).status())
                            }
                        };
                        let (child_arg2, second) = match stage_process_arg(
                            process.handles(),
                            child_handles,
                            arg2,
                            run_flags.contains(ProcessRunFlags::TRANSFER_ARG2),
                        ) {
                            Ok(staged) => staged,
                            Err(error) => {
                                let _ = rollback_process_grants(child_handles, &[first]);
                                return KernelCallResult::Status(errno_from_object(error).status());
                            }
                        };
                        (child_arg, child_arg2, [first, second])
                    };

                    let status = if run_flags.contains(ProcessRunFlags::ASYNC) {
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
                    if status == Errno::Ok.status() {
                        if commit_process_grants(process.handles_mut(), &grants).is_err() {
                            return KernelCallResult::Status(Errno::Internal.status());
                        }
                    } else {
                        let child_handles = unsafe { (*target_ptr).handles_mut() };
                        if rollback_process_grants(child_handles, &grants).is_err() {
                            return KernelCallResult::Status(Errno::Internal.status());
                        }
                    }
                    KernelCallResult::Status(status)
                }
                #[cfg(not(target_os = "none"))]
                {
                    let _ = (process_handle, entry, sp, arg, arg2, flags);
                    KernelCallResult::Status(Errno::NotSupported.status())
                }
            }
            KernelCall::ProcessWait => KernelCallResult::Status(crate::user_thread::process_wait()),
            KernelCall::InterruptCreate { resource, irq } => {
                // IRQ authority flows through a Resource grant, range-checked like MMIO
                // (no ambient authority — PLAN §5.1). A driver may only bind an IRQ its
                // narrowed device Resource covers.
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
                if !resource_contains_irq(res, irq) {
                    return KernelCallResult::Status(Errno::AccessDenied.status());
                }
                let key = interrupt_authority_key(irq);
                if self
                    .interrupts
                    .iter()
                    .any(|binding| binding.irq == key && binding.state.get() != IrqState::Released)
                {
                    return KernelCallResult::Status(Errno::BadState.status());
                }
                // Reserve both allocations before configuring a physical source. No
                // second owner may reconfigure/unmask an outstanding delivery. — KESTREL
                if let Err(error) = process.handles_mut().reserve_insert() {
                    return KernelCallResult::Status(errno_from_object(error).status());
                }
                if self.interrupts.try_reserve(1).is_err() {
                    return KernelCallResult::Status(Errno::NoMemory.status());
                }
                let object = self.objects.create(ObjectKind::Interrupt);
                let (handle, references) = match process
                    .handles_mut()
                    .insert_interrupt(object, Rights::READ | Rights::WAIT | Rights::DUPLICATE)
                {
                    Ok(owner) => owner,
                    Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                };
                let state = Rc::new(Cell::new(IrqState::Bound));
                let irq = match configure_interrupt_source(irq) {
                    Ok(irq) => irq,
                    Err(error) => {
                        let _ = process.handles_mut().close(handle);
                        return KernelCallResult::Status(error.status());
                    }
                };
                self.interrupts.push(IrqBinding {
                    references,
                    state,
                    irq,
                    koid: object.koid(),
                    count: 0,
                    requires_complete: decode_tlmm_gpio_irq(irq).is_some(),
                    outstanding: false,
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
                    if binding.koid == entry.koid && binding.state.get() == IrqState::Bound {
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
            KernelCall::InterruptComplete { interrupt } => {
                let entry =
                    match process
                        .handles()
                        .require(interrupt, ObjectKind::Interrupt, Rights::WAIT)
                    {
                        Ok(e) => e,
                        Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                    };
                for binding in &mut self.interrupts {
                    if binding.koid == entry.koid && binding.state.get() == IrqState::Bound {
                        return KernelCallResult::Status(
                            match binding.complete(complete_interrupt_source) {
                                Ok(()) => Errno::Ok.status(),
                                Err(error) => error.status(),
                            },
                        );
                    }
                }
                KernelCallResult::Status(Errno::BadHandle.status())
            }
            KernelCall::ResourceMintMmio {
                resource,
                phys_base,
                len,
            } => {
                // DESIGN/020 D0-1: READ mints; the VMO is writable only if the Resource is. A
                // holder narrowed to READ can map its registers but never write them.
                // — TIMBERDOODLE
                let res_entry =
                    match process
                        .handles()
                        .require(resource, ObjectKind::Resource, Rights::READ)
                    {
                        Ok(e) => e,
                        Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                    };
                let Some(res) = self.resource_by_koid(res_entry.koid) else {
                    return KernelCallResult::Status(Errno::BadHandle.status());
                };
                if len == 0 {
                    return KernelCallResult::Status(Errno::InvalidArgs.status());
                }
                if !resource_contains(res, phys_base, len) {
                    return KernelCallResult::Status(Errno::AccessDenied.status());
                }
                // DESIGN/020 §5: a powered-down device's registers cannot be minted, so no driver
                // can map, and then read, an aperture whose read may hang the bus. Ranges outside
                // every power unit return at the HAL's first comparison.
                if kumo_hal::active::device_aperture_blocked(self.dtb(), phys_base, len) {
                    return KernelCallResult::Status(Errno::BadState.status());
                }
                // Create a Physical VMO from the MMIO range.
                let write = if kumo_hal::active::device_mmio_read_only(phys_base, len) {
                    Rights::empty()
                } else {
                    res_entry.rights & Rights::WRITE
                };
                let mut transfer = Rights::empty();
                #[cfg(feature = "arch_aarch64")]
                if res_entry.rights.contains(Rights::TRANSFER)
                    && self
                        .framebuffer_ram
                        .is_some_and(|p| p.span() == kumo_abi::Range::new(phys_base, len))
                {
                    // The exact loader-attested GOP span is reserved RAM data, so its
                    // actual mapped identity may cross IPC. Other MMIO stays nontransferable. — KESTREL
                    transfer = Rights::TRANSFER;
                }
                match crate::mm::Vmo::from_mmio_range(phys_base, len) {
                    Ok(vmo) => match self.root_vmo_create(
                        process,
                        vmo,
                        Rights::READ | Rights::DUPLICATE | write | transfer,
                    ) {
                        Ok(handle) => KernelCallResult::Handle(handle),
                        Err(e) => KernelCallResult::Status(errno_from_object(e).status()),
                    },
                    Err(_) => KernelCallResult::Status(Errno::InvalidArgs.status()),
                }
            }
            KernelCall::FramebufferClaim {
                resource,
                phys_base,
                len,
            } => {
                let res_entry =
                    match process
                        .handles()
                        .require(resource, ObjectKind::Resource, Rights::WRITE)
                    {
                        Ok(entry) => entry,
                        Err(error) => {
                            return KernelCallResult::Status(errno_from_object(error).status())
                        }
                    };
                let Some(res) = self.resource_by_koid(res_entry.koid) else {
                    return KernelCallResult::Status(Errno::BadHandle.status());
                };
                if len == 0 {
                    KernelCallResult::Status(Errno::InvalidArgs.status())
                } else if !resource_contains(res, phys_base, len) {
                    KernelCallResult::Status(Errno::AccessDenied.status())
                } else {
                    KernelCallResult::Status(Errno::Ok.status())
                }
            }
            KernelCall::ResourceCreateChild {
                parent,
                phys_base,
                len,
                irq_base,
                irq_count,
            } => match self
                .resource_create_child(process, parent, phys_base, len, irq_base, irq_count)
            {
                Ok(handle) => KernelCallResult::Handle(handle),
                Err(errno) => KernelCallResult::Status(errno.status()),
            },
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
            KernelCall::PortUnbind { port, object } => {
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
                // Idempotent: removing an already-absent watch is a no-op success, so a
                // supervisor need not track whether it already dropped a dead instance.
                self.port_unbind(port_koid, obj_koid);
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
            KernelCall::VmoCreateContiguous { size } => {
                let Some(boot) = self.boot_info else {
                    return KernelCallResult::Status(Errno::Internal.status());
                };
                // A DMA VMO is whole frames; page-round the request and reject 0 / overflow.
                let Some(len) = crate::mm::align_up(size).filter(|&len| len != 0) else {
                    return KernelCallResult::Status(Errno::InvalidArgs.status());
                };
                let count = len / crate::mm::PAGE_SIZE;
                #[cfg(feature = "arch_aarch64")]
                let excluded = self.table_pages.dma_exclusions();
                #[cfg(not(feature = "arch_aarch64"))]
                let excluded = Vec::new();
                let phys = match alloc_contiguous(&boot, count, &excluded) {
                    Ok(phys) => phys,
                    Err(e) => return KernelCallResult::Status(e.status()),
                };
                let vmo = match crate::mm::Vmo::from_dma_range(phys, len) {
                    Ok(vmo) => vmo,
                    Err(_) => return KernelCallResult::Status(Errno::Internal.status()),
                };
                match self.root_vmo_create(
                    process,
                    vmo,
                    Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER,
                ) {
                    Ok(handle) => KernelCallResult::HandleAndValue {
                        handle,
                        value: phys,
                    },
                    Err(e) => KernelCallResult::Status(errno_from_object(e).status()),
                }
            }
            KernelCall::IoMmuFromResource {
                resource,
                kind: _kind,
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
                if len == 0 || !resource_contains(res, phys_base, len) {
                    return KernelCallResult::Status(Errno::AccessDenied.status());
                }

                if self.iommu_unit_quarantined(phys_base, len) {
                    return KernelCallResult::Status(Errno::BadState.status());
                }

                let mut supported = false;
                #[cfg(feature = "arch_aarch64")]
                if _kind == kumo_abi::sys::IoMmuKind::RockchipV2 as u32 {
                    if self.devices.iommus.iter().any(|u| {
                        u.iommu_kind == _kind && ranges_overlap(phys_base, len, u.phys_base, u.len)
                    }) {
                        return KernelCallResult::Status(Errno::BadState.status());
                    }
                    supported = self.display_iommu_supported(phys_base, len);
                }
                if _kind != kumo_abi::sys::IoMmuKind::RockchipV2 as u32
                    && !kumo_hal::active::device_mmio_read_only(phys_base, len)
                {
                    supported = kumo_hal::active::iommu_init(_kind, phys_base, len);
                }
                if !supported {
                    return KernelCallResult::Status(Errno::NotSupported.status());
                }

                if let Err(e) = process.handles_mut().reserve_insert() {
                    return KernelCallResult::Status(errno_from_object(e).status());
                }
                if self.devices.iommus.try_reserve(1).is_err() {
                    return KernelCallResult::Status(Errno::NoMemory.status());
                }
                #[cfg(feature = "arch_aarch64")]
                if _kind == kumo_abi::sys::IoMmuKind::RockchipV2 as u32 {
                    let Ok(mut owner) = self.device_owner.0.display_reservation.try_borrow_mut()
                    else {
                        return KernelCallResult::Status(Errno::BadState.status());
                    };
                    if owner.upgrade().is_some() {
                        return KernelCallResult::Status(Errno::BadState.status());
                    }
                    // Physical ownership is shared by engine clones and retained payloads,
                    // not recreated from a fresh object koid after restart. — KESTREL
                    let reservation = Rc::new(());
                    *owner = Rc::downgrade(&reservation);
                    self.devices.display_reservation = Some(reservation);
                }

                let object = self.objects.create(ObjectKind::IoMmu);
                self.devices.iommus.push(IoMmuBinding {
                    koid: object.koid(),
                    iommu_kind: _kind as u32,
                    phys_base,
                    len,
                });

                let handle = match process.handles_mut().insert(
                    object,
                    Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER,
                ) {
                    Ok(h) => h,
                    Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                };
                KernelCallResult::Handle(handle)
            }
            KernelCall::DeviceCtxActivate { ctx } => {
                let result = self.activate_display_context(process, ctx);
                KernelCallResult::Status(result.err().map_or(Errno::Ok.status(), Errno::status))
            }
            KernelCall::DeviceCtxCreate {
                iommu,
                stream_or_rid: _stream_or_rid,
            } => {
                let _iommu_entry =
                    match process
                        .handles()
                        .require(iommu, ObjectKind::IoMmu, Rights::WRITE)
                    {
                        Ok(e) => e,
                        Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                    };

                let Some(iommu_binding) = self
                    .devices
                    .iommus
                    .iter()
                    .find(|b| b.koid == _iommu_entry.koid)
                    .copied()
                else {
                    return KernelCallResult::Status(Errno::BadHandle.status());
                };

                let Ok(stream_id) = u32::try_from(_stream_or_rid) else {
                    return KernelCallResult::Status(Errno::InvalidArgs.status());
                };
                if iommu_binding.iommu_kind == kumo_abi::sys::IoMmuKind::RockchipV2 as u32
                    && stream_id != 0
                {
                    return KernelCallResult::Status(Errno::InvalidArgs.status());
                }
                if self.iommu_unit_quarantined(iommu_binding.phys_base, iommu_binding.len) {
                    return KernelCallResult::Status(Errno::BadState.status());
                }
                if self
                    .devices
                    .device_ctxs
                    .iter()
                    .any(|ctx| ctx.iommu_koid == _iommu_entry.koid && ctx.stream_id == stream_id)
                {
                    return KernelCallResult::Status(Errno::InvalidArgs.status());
                }

                // Preflight metadata before touching the backend. No handle-table
                // mutation intervenes before installing its sole initial grant. — KESTREL
                if let Err(error) = process.handles_mut().reserve_insert() {
                    return KernelCallResult::Status(errno_from_object(error).status());
                }
                if self.devices.device_ctxs.try_reserve(1).is_err() {
                    return KernelCallResult::Status(Errno::NoMemory.status());
                }

                #[cfg(feature = "arch_aarch64")]
                let mut display = None;
                let pgd_phys;
                if iommu_binding.iommu_kind == kumo_abi::sys::IoMmuKind::RockchipV2 as u32 {
                    #[cfg(feature = "arch_aarch64")]
                    {
                        if self.devices.display_contexts.try_reserve(1).is_err() {
                            return KernelCallResult::Status(Errno::NoMemory.status());
                        }
                        display =
                            match crate::device_memory::OfflineMappings::new(&self.table_pages) {
                                Ok(m) => Some(m),
                                Err(e) => return KernelCallResult::Status(e.status()),
                            };
                        pgd_phys = 0;
                    }
                    #[cfg(not(feature = "arch_aarch64"))]
                    {
                        return KernelCallResult::Status(Errno::NotSupported.status());
                    }
                } else {
                    let Some(boot) = self.boot_info.as_ref() else {
                        return KernelCallResult::Status(Errno::BadState.status());
                    };
                    let Some(root) = (unsafe { crate::mm::alloc_zeroed_frame(boot) }) else {
                        return KernelCallResult::Status(Errno::NoMemory.status());
                    };
                    pgd_phys = root;
                }

                if iommu_binding.iommu_kind != kumo_abi::sys::IoMmuKind::RockchipV2 as u32
                    && !kumo_hal::active::iommu_create_device_context(
                        iommu_binding.iommu_kind,
                        iommu_binding.phys_base,
                        stream_id,
                        pgd_phys,
                    )
                {
                    return KernelCallResult::Status(Errno::NotSupported.status());
                }

                let object = self.objects.create(ObjectKind::DeviceCtx);

                let (handle, references) = match process.handles_mut().insert_device_context(
                    object,
                    Rights::MAP | Rights::WAIT | Rights::DUPLICATE | Rights::TRANSFER,
                ) {
                    Ok(h) => h,
                    Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                };
                self.devices.device_ctxs.push(DeviceCtxBinding {
                    koid: object.koid(),
                    iommu_koid: _iommu_entry.koid,
                    stream_id,
                    pgd_phys,
                    references,
                    state: Rc::new(Cell::new(DeviceCtxState::Bound)),
                });
                #[cfg(feature = "arch_aarch64")]
                if let Some(mappings) = display {
                    self.devices.display_contexts.push(DisplayContext {
                        koid: object.koid(),
                        faults: Rc::new(DisplayFaults::default()),
                        epoch: Rc::new(Cell::new(0)),
                        published: Rc::new(RefCell::new(None)),
                        mappings: Rc::new(RefCell::new(Some(mappings))),
                    });
                }
                KernelCallResult::Handle(handle)
            }
            KernelCall::DeviceVmarMap {
                ctx,
                vmo,
                vmo_offset,
                len,
                iova_hint,
                rights,
            } => {
                let ctx_entry =
                    match process
                        .handles()
                        .require(ctx, ObjectKind::DeviceCtx, Rights::MAP)
                    {
                        Ok(e) => e,
                        Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                    };
                let Some(ctx_binding) = self
                    .devices
                    .device_ctxs
                    .iter()
                    .find(|b| b.koid == ctx_entry.koid)
                    .cloned()
                else {
                    return KernelCallResult::Status(Errno::BadHandle.status());
                };
                let Some(iommu_binding) = self
                    .devices
                    .iommus
                    .iter()
                    .find(|b| b.koid == ctx_binding.iommu_koid)
                    .copied()
                else {
                    return KernelCallResult::Status(Errno::BadHandle.status());
                };
                if ctx_binding.state.get() != DeviceCtxState::Bound
                    || self.iommu_unit_quarantined(iommu_binding.phys_base, iommu_binding.len)
                {
                    return KernelCallResult::Status(Errno::BadState.status());
                }
                if !device_dma_rights_valid(rights)
                    || len == 0
                    || iova_hint == 0
                    || !crate::mm::is_page_aligned(vmo_offset)
                    || !crate::mm::is_page_aligned(iova_hint)
                    || !crate::mm::is_page_aligned(len)
                    || iova_hint.checked_add(len).is_none()
                {
                    return KernelCallResult::Status(Errno::InvalidArgs.status());
                }

                #[cfg(feature = "arch_aarch64")]
                if iommu_binding.iommu_kind == kumo_abi::sys::IoMmuKind::RockchipV2 as u32 {
                    let result = (|| {
                        if rights != Rights::READ {
                            return Err(Errno::AccessDenied);
                        }
                        let pin =
                            self.pin_device_vmo_read_only(process, vmo, vmo_offset, len, rights)?;
                        let domain = self
                            .display_context(ctx_entry.koid)
                            .ok_or(Errno::BadState)?;
                        let mut maps = domain
                            .mappings
                            .try_borrow_mut()
                            .map_err(|_| Errno::BadState)?;
                        maps.as_mut().ok_or(Errno::BadState)?.map(iova_hint, pin)
                    })();
                    return KernelCallResult::Status(
                        result.err().map_or(Errno::Ok.status(), Errno::status),
                    );
                }

                let vmo_entry = match process.handles().require(
                    vmo,
                    ObjectKind::Vmo,
                    vmo_rights_for_device_dma(rights),
                ) {
                    Ok(e) => e,
                    Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                };
                let Some(backing) = self.vmos.iter().find(|v| v.koid == vmo_entry.koid).cloned()
                else {
                    return KernelCallResult::Status(Errno::BadHandle.status());
                };
                let vmo_koid = backing.koid;
                if vmo_offset
                    .checked_add(len)
                    .is_none_or(|end| end > backing.vmo.len())
                {
                    return KernelCallResult::Status(Errno::InvalidArgs.status());
                }
                if self.devices.device_vmar_mappings.iter().any(|mapping| {
                    mapping.ctx_koid == ctx_entry.koid
                        && ranges_overlap(mapping.iova, mapping.len, iova_hint, len)
                }) {
                    return KernelCallResult::Status(Errno::InvalidArgs.status());
                }

                if self.devices.device_vmar_mappings.try_reserve(1).is_err() {
                    return KernelCallResult::Status(Errno::NoMemory.status());
                }

                if let Err(errno) = self.device_vmar_backend_map(
                    &ctx_binding,
                    iommu_binding,
                    vmo_koid,
                    vmo_offset,
                    len,
                    iova_hint,
                    rights,
                ) {
                    return KernelCallResult::Status(errno.status());
                }

                self.devices.device_vmar_mappings.push(DeviceVmarMapping {
                    ctx_koid: ctx_entry.koid,
                    iova: iova_hint,
                    len,
                    vmo_offset,
                    backing,
                });
                KernelCallResult::Status(Errno::Ok.status())
            }
            KernelCall::DeviceVmarUnmap { ctx, iova, len } => {
                let ctx_entry =
                    match process
                        .handles()
                        .require(ctx, ObjectKind::DeviceCtx, Rights::MAP)
                    {
                        Ok(e) => e,
                        Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                    };
                let Some(ctx_binding) = self
                    .devices
                    .device_ctxs
                    .iter()
                    .find(|b| b.koid == ctx_entry.koid)
                    .cloned()
                else {
                    return KernelCallResult::Status(Errno::BadHandle.status());
                };
                let Some(iommu_binding) = self
                    .devices
                    .iommus
                    .iter()
                    .find(|b| b.koid == ctx_binding.iommu_koid)
                    .copied()
                else {
                    return KernelCallResult::Status(Errno::BadHandle.status());
                };
                if ctx_binding.state.get() != DeviceCtxState::Bound
                    || self.iommu_unit_quarantined(iommu_binding.phys_base, iommu_binding.len)
                {
                    return KernelCallResult::Status(Errno::BadState.status());
                }
                if len == 0
                    || iova == 0
                    || !crate::mm::is_page_aligned(iova)
                    || !crate::mm::is_page_aligned(len)
                    || iova.checked_add(len).is_none()
                {
                    return KernelCallResult::Status(Errno::InvalidArgs.status());
                }

                #[cfg(feature = "arch_aarch64")]
                if iommu_binding.iommu_kind == kumo_abi::sys::IoMmuKind::RockchipV2 as u32 {
                    let result = (|| {
                        let domain = self
                            .display_context(ctx_entry.koid)
                            .ok_or(Errno::BadState)?;
                        let mut maps = domain
                            .mappings
                            .try_borrow_mut()
                            .map_err(|_| Errno::BadState)?;
                        maps.as_mut().ok_or(Errno::BadState)?.unmap(iova, len)
                    })();
                    return KernelCallResult::Status(
                        result.err().map_or(Errno::Ok.status(), Errno::status),
                    );
                }

                let Some(index) = self
                    .devices
                    .device_vmar_mappings
                    .iter()
                    .position(|mapping| {
                        mapping.ctx_koid == ctx_entry.koid
                            && mapping.iova == iova
                            && mapping.len == len
                    })
                else {
                    return KernelCallResult::Status(Errno::InvalidArgs.status());
                };

                if let Err(errno) =
                    self.device_vmar_backend_unmap_range(&ctx_binding, iommu_binding, iova, len)
                {
                    return KernelCallResult::Status(errno.status());
                }

                self.devices.device_vmar_mappings.remove(index);
                KernelCallResult::Status(Errno::Ok.status())
            }
            KernelCall::DeviceCtxWaitFault { ctx } => {
                let ctx_entry =
                    match process
                        .handles()
                        .require(ctx, ObjectKind::DeviceCtx, Rights::WAIT)
                    {
                        Ok(e) => e,
                        Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                    };
                if self
                    .devices
                    .device_ctxs
                    .iter()
                    .all(|b| b.koid != ctx_entry.koid)
                {
                    return KernelCallResult::Status(Errno::BadHandle.status());
                }
                #[cfg(feature = "arch_aarch64")]
                if let Some(domain) = self.display_context(ctx_entry.koid) {
                    return KernelCallResult::Status(
                        if domain.faults.slots.iter().any(|s| s.get().is_some()) {
                            Errno::Ok.status()
                        } else {
                            Errno::ShouldWait.status()
                        },
                    );
                }
                if self
                    .devices
                    .device_ctx_faults
                    .iter()
                    .any(|fault| fault.ctx_koid == ctx_entry.koid)
                {
                    KernelCallResult::Status(Errno::Ok.status())
                } else {
                    KernelCallResult::Status(Errno::ShouldWait.status())
                }
            }
            KernelCall::DeviceCtxInfo {
                ctx,
                user_ptr,
                user_len,
            } => {
                let ctx_entry =
                    match process
                        .handles()
                        .require(ctx, ObjectKind::DeviceCtx, Rights::WAIT)
                    {
                        Ok(e) => e,
                        Err(e) => return KernelCallResult::Status(errno_from_object(e).status()),
                    };
                if self
                    .devices
                    .device_ctxs
                    .iter()
                    .all(|b| b.koid != ctx_entry.koid)
                {
                    return KernelCallResult::Status(Errno::BadHandle.status());
                }
                let info_len = mem::size_of::<DeviceCtxInfo>() as u64;
                if user_ptr == 0 || user_len < info_len || user_ptr.checked_add(info_len).is_none()
                {
                    return KernelCallResult::Status(Errno::InvalidArgs.status());
                }
                #[cfg(feature = "arch_aarch64")]
                if let Some(domain) = self.display_context(ctx_entry.koid) {
                    let info = if let Some(info) = domain.faults.slots.iter().find_map(Cell::take) {
                        info
                    } else if domain.epoch.get() != 0 {
                        use kumo_abi::sys::device_ctx_state as s;
                        DeviceCtxInfo {
                            fault_record: s::RECORD
                                | if self.display_context_live(&domain) {
                                    s::LIVE
                                } else {
                                    s::QUARANTINED
                                },
                            fault_addr: domain.epoch.get(),
                        }
                    } else {
                        return KernelCallResult::Status(Errno::ShouldWait.status());
                    };
                    unsafe {
                        core::ptr::write_unaligned(user_ptr as *mut DeviceCtxInfo, info);
                    }
                    return KernelCallResult::Status(Errno::Ok.status());
                }
                let Some(index) = self
                    .devices
                    .device_ctx_faults
                    .iter()
                    .position(|fault| fault.ctx_koid == ctx_entry.koid)
                else {
                    return KernelCallResult::Status(Errno::ShouldWait.status());
                };
                let fault = self.devices.device_ctx_faults.remove(index);
                unsafe {
                    core::ptr::write_unaligned(user_ptr as *mut DeviceCtxInfo, fault.info);
                }
                KernelCallResult::Status(Errno::Ok.status())
            }
            KernelCall::DevicePowerUp {
                resource,
                aperture_base,
            } => match self.device_power_authority(process, resource, Rights::WRITE, aperture_base)
            {
                Ok(unit) => KernelCallResult::Status(self.device_power_up(unit).status()),
                Err(errno) => KernelCallResult::Status(errno.status()),
            },
            KernelCall::DevicePowerStatus {
                resource,
                aperture_base,
                user_ptr,
                user_len,
            } => {
                if let Err(errno) =
                    process
                        .handles()
                        .require(resource, ObjectKind::Resource, Rights::READ)
                {
                    return KernelCallResult::Status(errno_from_object(errno).status());
                }
                let record_len = mem::size_of::<DevicePowerStatus>() as u64;
                if user_ptr == 0
                    || user_len < record_len
                    || user_ptr.checked_add(record_len).is_none()
                {
                    return KernelCallResult::Status(Errno::InvalidArgs.status());
                }
                let unit = match self.device_power_authority(
                    process,
                    resource,
                    Rights::READ,
                    aperture_base,
                ) {
                    Ok(unit) => unit,
                    Err(errno) => return KernelCallResult::Status(errno.status()),
                };
                let Some(readout) =
                    kumo_hal::active::device_power_status(self.dtb(), unit.aperture_base)
                else {
                    return KernelCallResult::Status(Errno::NotSupported.status());
                };
                unsafe {
                    core::ptr::write_unaligned(
                        user_ptr as *mut DevicePowerStatus,
                        device_power_status_record(&readout),
                    );
                }
                KernelCallResult::Status(Errno::Ok.status())
            }
            KernelCall::DeviceRegisterWrite {
                resource,
                aperture_base,
                index,
                value,
            } => KernelCallResult::Status(
                self.device_register_write(process, resource, aperture_base, index, value)
                    .status(),
            ),
            KernelCall::DeviceResetSet {
                resource,
                aperture_base,
                line,
                asserted,
            } => {
                let status =
                    self.device_reset_set(process, resource, aperture_base, line, asserted);
                KernelCallResult::Status(status.status())
            }
            KernelCall::Unsupported(_) => KernelCallResult::Status(Errno::NotSupported.status()),
        }
    }

    /// The address of the board DTB the kernel accepted at boot, or 0.
    fn dtb(&self) -> u64 {
        self.boot_info.as_ref().map_or(0, |boot| boot.platform.dtb)
    }

    #[cfg(feature = "arch_aarch64")]
    fn display_context_live(&self, d: &DisplayContext) -> bool {
        if !self
            .devices
            .device_ctxs
            .iter()
            .any(|c| c.koid == d.koid && c.state.get() == DeviceCtxState::Bound)
        {
            return false;
        }
        let Ok(published) = d.published.try_borrow() else {
            return false;
        };
        published.as_ref().is_some_and(|p| {
            kumo_hal::active::display_live_binding() == Some((p.root(), d.epoch.get()))
        })
    }
    fn activate_display_context(&mut self, process: &Process, ctx: Handle) -> Result<(), Errno> {
        let entry = process
            .handles()
            .require(ctx, ObjectKind::DeviceCtx, Rights::MAP)
            .map_err(errno_from_object)?;
        let binding = self
            .devices
            .device_ctxs
            .iter()
            .find(|c| c.koid == entry.koid)
            .cloned()
            .ok_or(Errno::BadHandle)?;
        if binding.state.get() != DeviceCtxState::Bound {
            return Err(Errno::BadState);
        }
        #[cfg(feature = "arch_aarch64")]
        {
            let domain = self
                .display_context(entry.koid)
                .ok_or(Errno::NotSupported)?;
            let mut maps = domain
                .mappings
                .try_borrow_mut()
                .map_err(|_| Errno::BadState)?;
            let mut published = domain
                .published
                .try_borrow_mut()
                .map_err(|_| Errno::BadState)?;
            if published.is_some() {
                return Err(Errno::BadState);
            }
            if maps.as_ref().is_none_or(|m| m.pins().next().is_none()) {
                return Err(Errno::InvalidArgs);
            }
            let mut controller = self
                .device_owner
                .0
                .display_controller
                .try_borrow_mut()
                .map_err(|_| Errno::BadState)?;
            if controller.is_none() {
                *controller = kumo_hal::active::display_controller_new(self.dtb());
            }
            let c = controller.as_mut().ok_or(Errno::NotSupported)?;
            let epoch = c.next_epoch().map_err(|_| Errno::BadState)?;
            *published = Some(maps.take().ok_or(Errno::BadState)?.publish(|page| {
                kumo_hal::active::clean_dcache_to_poc(
                    phys_ptr_mut(page) as usize,
                    crate::mm::PAGE_SIZE as usize,
                );
            }));
            domain.epoch.set(epoch);
            let p = published.as_ref().ok_or(Errno::BadState)?;
            match kumo_hal::active::display_controller_activate(c, p.root_ref(), epoch) {
                Ok(report) => {
                    if report.previous.is_some() {
                        binding.state.set(DeviceCtxState::Quarantined);
                        return Err(Errno::BadState);
                    }
                    #[cfg(target_os = "none")]
                    crate::klog_at!(kumo_abi::sys::log_level::NOTICE,"DISPLAY ACTIVATE Check     generation {} owned root {:#x}, both roots {:#x}/{:#x}, paging/masks {:#x}/{:#x} {:#x}/{:#x}; bypass sealed, VP2 resumed\n",epoch,p.root(),report.banks[0].root,report.banks[1].root,report.banks[0].status,report.banks[1].status,report.banks[0].mask,report.banks[1].mask);
                    Ok(())
                }
                Err(error) => {
                    binding.state.set(DeviceCtxState::Quarantined);
                    #[cfg(target_os = "none")]
                    crate::klog_at!(kumo_abi::sys::log_level::ERR,"DISPLAY ACTIVATE Refuse    {:?}; generation {}, root {:#x} tables and pins retained\n",error,epoch,p.root());
                    let _ = error;
                    Err(Errno::BadState)
                }
            }
        }
        #[cfg(not(feature = "arch_aarch64"))]
        {
            Err(Errno::NotSupported)
        }
    }

    fn device_register_write(
        &self,
        process: &Process,
        resource: Handle,
        base: u64,
        index: u64,
        value: u64,
    ) -> Errno {
        let entry = match process
            .handles()
            .require(resource, ObjectKind::Resource, Rights::WRITE)
        {
            Ok(entry) => entry,
            Err(error) => return errno_from_object(error),
        };
        if index > u64::from(u32::MAX) || value > u64::from(u32::MAX) {
            return Errno::InvalidArgs;
        }
        let Some(res) = self.resource_by_koid(entry.koid) else {
            return Errno::BadHandle;
        };
        if !resource_contains(res, base, 1) {
            return Errno::AccessDenied;
        }
        let Some(len) = kumo_hal::active::device_register_window(base) else {
            return Errno::NotSupported;
        };
        if !resource_contains(res, base, len) {
            return Errno::AccessDenied;
        }
        if !kumo_hal::active::device_register_valid(base, index as u32, value as u32) {
            return Errno::InvalidArgs;
        }
        #[cfg(feature = "arch_aarch64")]
        if self.device_owner.0.display_controller.borrow().is_some()
            && !self.devices.display_contexts.iter().any(|d| {
                self.display_context_live(d)
                    && process
                        .handles()
                        .holds(d.koid, ObjectKind::DeviceCtx, Rights::MAP)
            })
        {
            return Errno::BadState;
        }
        match kumo_hal::active::device_register_write(self.dtb(), base, index as u32, value as u32)
        {
            Some(true) => Errno::Ok,
            Some(false) => Errno::BadState,
            None => Errno::NotSupported,
        }
    }

    fn device_reset_set(
        &self,
        process: &Process,
        resource: Handle,
        base: u64,
        line: u64,
        asserted: u64,
    ) -> Errno {
        let entry = match process
            .handles()
            .require(resource, ObjectKind::Resource, Rights::WRITE)
        {
            Ok(entry) => entry,
            Err(error) => return errno_from_object(error),
        };
        if asserted > 1 || line > u64::from(u32::MAX) {
            return Errno::InvalidArgs;
        }
        let Some(res) = self.resource_by_koid(entry.koid) else {
            return Errno::BadHandle;
        };
        if !resource_contains(res, base, 1) {
            return Errno::AccessDenied;
        }
        let Some((len, count)) = kumo_hal::active::device_reset_unit(self.dtb(), base) else {
            return Errno::NotSupported;
        };
        if !resource_contains(res, base, len) {
            return Errno::AccessDenied;
        }
        if line >= u64::from(count) {
            return Errno::InvalidArgs;
        }
        if kumo_hal::active::device_reset_set(self.dtb(), base, line as u32, asserted != 0) {
            Errno::Ok
        } else {
            Errno::BadState
        }
    }

    /// DESIGN/020 §1: the caller must hold a Resource with `needed` whose window contains the
    /// aperture of a live power unit. The device is named by its aperture base; there is no other
    /// identifier, and nothing outside the unit's own bits can be reached through it.
    fn device_power_authority(
        &self,
        process: &Process,
        resource: Handle,
        needed: Rights,
        aperture_base: u64,
    ) -> Result<DevicePowerUnitInfo, Errno> {
        let entry = process
            .handles()
            .require(resource, ObjectKind::Resource, needed)
            .map_err(errno_from_object)?;
        let res = self.resource_by_koid(entry.koid).ok_or(Errno::BadHandle)?;
        let unit = kumo_hal::active::device_power_unit(self.dtb(), aperture_base)
            .ok_or(Errno::NotSupported)?;
        if !resource_contains(res, unit.aperture_base, unit.aperture_len) {
            return Err(Errno::AccessDenied);
        }
        Ok(unit)
    }

    /// Run the HAL power-up for `unit` and say what happened in the kernel log: one line on
    /// success, an `ERR` line naming the step on failure (DESIGN/014).
    fn device_power_up(&self, unit: DevicePowerUnitInfo) -> Errno {
        match kumo_hal::active::device_power_up(self.dtb(), unit.aperture_base) {
            Ok(DevicePowerUpOutcome::AlreadyUp) => {
                crate::klog!(
                    "{} POWER-UP Check     {:#x} already up   OK\n",
                    unit.name,
                    unit.aperture_base
                );
                Errno::Ok
            }
            Ok(DevicePowerUpOutcome::Raised { repair_ns, idle_ns }) => {
                crate::klog!(
                    "{} POWER-UP Check     {:#x} clock set, repair {} us, bus {} us   OK\n",
                    unit.name,
                    unit.aperture_base,
                    repair_ns / 1_000,
                    idle_ns / 1_000
                );
                Errno::Ok
            }
            Err(DevicePowerError::NoUnit) => Errno::NotSupported,
            Err(DevicePowerError::PllMismatch { hz }) => {
                crate::klog_at!(
                    crate::kmsg::ERR,
                    "{} POWER-UP Check     refused: parent PLL at {} Hz, not the expected rate; \
                     nothing written   FAIL\n",
                    unit.name,
                    hz
                );
                Errno::NotSupported
            }
            Err(DevicePowerError::Timeout { step }) => {
                crate::klog_at!(
                    crate::kmsg::ERR,
                    "{} POWER-UP Check     {} timed out; power-down request restored \
                     (is the rail on?)   FAIL\n",
                    unit.name,
                    step.name()
                );
                Errno::TimedOut
            }
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

/// Apply the backend reservation to stale/privileged physical aliases too, not only MMIO
/// mints. No physical dereference or page-table update precedes this check. — KESTREL
fn physical_vmo_write_allowed(vmo: &Vmo, offset: u64, len: u64) -> bool {
    match vmo.backing() {
        crate::mm::VmoBacking::PhysicalRam { phys_base }
        | crate::mm::VmoBacking::Mmio { phys_base }
        | crate::mm::VmoBacking::Dma { phys_base } => phys_base
            .checked_add(offset)
            .is_some_and(|base| !kumo_hal::active::device_mmio_read_only(base, len)),
        crate::mm::VmoBacking::Anonymous => true,
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

    mod context_retirement {
        use super::*;
        use crate::mm::PAGE_SIZE;
        use crate::object::KernelObject;
        use alloc::rc::Weak;

        fn mapped_context() -> (SyscallEngine, Process, Handle, Weak<VmoEntry>) {
            let mut engine = SyscallEngine::new();
            let mut process = test_process(&mut engine);
            let ctx = create_test_device_ctx(&mut engine, &mut process);
            let vmo = create_physical_vmo(&mut engine, &mut process, 0x9000_0000, 3 * PAGE_SIZE);
            let backing = Rc::downgrade(&engine.vmos[0]);
            assert_eq!(
                engine.dispatch(
                    &mut process,
                    KernelCall::DeviceVmarMap {
                        ctx,
                        vmo,
                        vmo_offset: PAGE_SIZE,
                        len: PAGE_SIZE,
                        iova_hint: 0x4000_0000,
                        rights: Rights::READ,
                    }
                ),
                KernelCallResult::Status(Errno::Ok.status())
            );
            assert_eq!(engine.devices.device_vmar_mappings[0].vmo_offset, PAGE_SIZE);
            assert_eq!(
                engine.devices.device_vmar_mappings[0].backing.koid,
                process.handles().get(vmo).unwrap().koid
            );
            engine.dispatch(&mut process, KernelCall::HandleClose { handle: vmo });
            // Remove the ordinary registry owner: only the mapping may retain backing.
            // This proves quarantine does not rely on boot-long VMO retention. — KESTREL
            engine.vmos.clear();
            let koid = process.handles().get(ctx).unwrap().koid;
            engine
                .signal_device_ctx_fault(koid, 0xa5, 0x4000_0000)
                .unwrap();
            engine.devices.port_bindings.push((KoId(999), koid));
            (engine, process, ctx, backing)
        }

        #[test]
        fn last_close_quarantines_without_releasing_root_backing_faults_or_watches() {
            let (mut engine, mut process, ctx, backing) = mapped_context();
            let duplicate = process.handles_mut().duplicate(ctx, Rights::WAIT).unwrap();
            let root = engine.devices.device_ctxs[0].pgd_phys;
            engine.dispatch(&mut process, KernelCall::HandleClose { handle: ctx });
            assert_eq!(
                engine.devices.device_ctxs[0].state.get(),
                DeviceCtxState::Bound
            );
            assert!(backing.upgrade().is_some());
            assert_eq!(
                engine.dispatch(&mut process, KernelCall::HandleClose { handle: duplicate }),
                KernelCallResult::Status(Errno::Ok.status())
            );
            assert_eq!(
                engine.devices.device_ctxs[0].state.get(),
                DeviceCtxState::Quarantined
            );
            assert_eq!(engine.devices.device_ctxs[0].pgd_phys, root);
            assert_eq!(engine.devices.device_vmar_mappings.len(), 1);
            assert_eq!(engine.devices.device_ctx_faults.len(), 1);
            assert_eq!(engine.devices.port_bindings.len(), 1);
            assert!(backing.upgrade().is_some());
            assert_eq!(
                engine.retire_device_contexts_with(|_, _| panic!("quarantine retried")),
                DeviceCtxRetirement::default()
            );
        }

        #[test]
        fn only_a_checked_success_can_release_mapping_ownership_and_metadata() {
            let (mut engine, mut process, ctx, backing) = mapped_context();
            process.handles_mut().close(ctx).unwrap();
            let mut checks = 0;
            let report = engine.retire_device_contexts_with(|unit, ctx| {
                checks += 1;
                assert_eq!(unit.phys_base, 0x1500_0000);
                assert_eq!(ctx.pgd_phys, 0x8000_0000);
                assert_eq!(ctx.state.get(), DeviceCtxState::Quarantined);
                assert!(backing.upgrade().is_some());
                true
            });
            assert_eq!(checks, 1);
            assert_eq!(
                report,
                DeviceCtxRetirement {
                    released: 1,
                    quarantined: 0
                }
            );
            assert!(backing.upgrade().is_none());
            assert!(engine.devices.device_ctxs.is_empty());
            assert!(engine.devices.device_vmar_mappings.is_empty());
            assert!(engine.devices.device_ctx_faults.is_empty());
            assert!(engine.devices.port_bindings.is_empty());
            assert!(!engine.iommu_unit_quarantined(0x1500_0000, 0x1000));
        }

        #[test]
        fn queued_transfers_and_drained_entries_delay_the_checked_teardown() {
            let (mut engine, mut source, ctx, backing) = mapped_context();
            let mut target = test_process(&mut engine);
            let (left, right) = engine.channel_create(&mut source).unwrap();
            let grant = source
                .handles()
                .stage_grant_to(target.handles_mut(), right, true)
                .unwrap();
            source.handles_mut().commit_grant(grant).unwrap();
            assert_eq!(
                engine.dispatch(
                    &mut source,
                    KernelCall::ChannelWrite {
                        channel: left,
                        message: Message::new(1, b"ctx", &[ctx]).unwrap(),
                    }
                ),
                KernelCallResult::Status(Errno::Ok.status())
            );
            drop(source.handles_mut().drain());
            drop(source);
            assert_eq!(
                engine.retire_device_contexts_with(|_, _| panic!("queue owner ignored")),
                DeviceCtxRetirement::default()
            );
            let received = engine.dispatch(
                &mut target,
                KernelCall::ChannelRead {
                    channel: grant.target(),
                },
            );
            assert!(matches!(received, KernelCallResult::Message(_)));
            let drained = target.handles_mut().drain();
            assert_eq!(
                engine.retire_device_contexts_with(|_, _| panic!("drained owner ignored")),
                DeviceCtxRetirement::default()
            );
            drop(drained);
            assert_eq!(
                engine.retire_device_contexts(),
                DeviceCtxRetirement {
                    released: 0,
                    quarantined: 1
                }
            );
            assert!(backing.upgrade().is_some());
        }

        #[test]
        fn quarantine_reserves_overlapping_unit_aliases_and_preexisting_iommu_handles() {
            let (mut engine, mut process, ctx, _) = mapped_context();
            let unit = engine.devices.iommus[0];
            let alias = process
                .handles_mut()
                .insert(
                    KernelObject::new(unit.koid, ObjectKind::IoMmu),
                    Rights::WRITE,
                )
                .unwrap();
            engine.dispatch(&mut process, KernelCall::HandleClose { handle: ctx });
            assert_eq!(
                engine.dispatch(
                    &mut process,
                    KernelCall::DeviceCtxCreate {
                        iommu: alias,
                        stream_or_rid: 7
                    }
                ),
                KernelCallResult::Status(Errno::BadState.status())
            );
            let resource = engine
                .root_resource_create(&mut process, 0, u64::MAX, 0, 0)
                .unwrap();
            for (base, len) in [
                (unit.phys_base, unit.len),
                (unit.phys_base + 0x1000, 0x1000),
                (unit.phys_base - 0x1000, 0x2000),
            ] {
                assert_eq!(
                    engine.dispatch(
                        &mut process,
                        KernelCall::IoMmuFromResource {
                            resource,
                            kind: kumo_abi::sys::IoMmuKind::Virtio as u32,
                            phys_base: base,
                            len,
                        }
                    ),
                    KernelCallResult::Status(Errno::BadState.status())
                );
            }
            assert!(!engine.iommu_unit_quarantined(unit.phys_base + unit.len, 0x1000));
        }

        #[test]
        fn another_bound_context_cannot_mutate_a_quarantined_unit() {
            let (mut engine, mut process, ctx, _) = mapped_context();
            let other = create_test_device_ctx_with_kind_and_stream(
                &mut engine,
                &mut process,
                kumo_abi::sys::IoMmuKind::SmmuV3 as u32,
                2,
            );
            let vmo = create_physical_vmo(&mut engine, &mut process, 0x9100_0000, PAGE_SIZE);
            engine.dispatch(&mut process, KernelCall::HandleClose { handle: ctx });
            assert_eq!(
                engine.dispatch(
                    &mut process,
                    KernelCall::DeviceVmarMap {
                        ctx: other,
                        vmo,
                        vmo_offset: 0,
                        len: PAGE_SIZE,
                        iova_hint: 0x5000_0000,
                        rights: Rights::READ,
                    }
                ),
                KernelCallResult::Status(Errno::BadState.status())
            );
            assert_eq!(
                engine.dispatch(
                    &mut process,
                    KernelCall::DeviceVmarUnmap {
                        ctx: other,
                        iova: 0x5000_0000,
                        len: PAGE_SIZE,
                    }
                ),
                KernelCallResult::Status(Errno::BadState.status())
            );
            assert_eq!(engine.devices.device_vmar_mappings.len(), 1);
        }

        #[test]
        fn missing_unit_metadata_retains_the_context_without_guessing_a_hardware_address() {
            let (mut engine, mut process, ctx, backing) = mapped_context();
            engine.devices.iommus.clear();
            process.handles_mut().close(ctx).unwrap();
            assert_eq!(
                engine.retire_device_contexts_with(|_, _| panic!("missing unit reached backend")),
                DeviceCtxRetirement {
                    released: 0,
                    quarantined: 1
                }
            );
            assert!(backing.upgrade().is_some());
            assert!(engine.iommu_unit_quarantined(0, 0x1000));
        }

        #[test]
        fn cloned_engines_share_the_checked_outcome_and_never_detach_twice() {
            let (mut engine, mut process, ctx, backing) = mapped_context();
            let mut clone = engine.clone();
            process.handles_mut().close(ctx).unwrap();
            assert_eq!(engine.retire_device_contexts_with(|_, _| true).released, 1);
            assert!(backing.upgrade().is_some());
            assert_eq!(
                clone
                    .retire_device_contexts_with(|_, _| panic!("second detach"))
                    .released,
                1
            );
            assert!(backing.upgrade().is_none());
        }

        #[test]
        fn engine_loss_retains_bound_contexts_before_their_last_capability_drops() {
            let (engine, process, _, backing) = mapped_context();
            let owner = engine.device_owner.clone();
            let state = engine.devices.device_ctxs[0].state.clone();
            drop(engine);
            assert_eq!(state.get(), DeviceCtxState::Quarantined);
            assert!(backing.upgrade().is_some());
            let replacement = SyscallEngine::with_device_owner(owner.clone());
            assert!(replacement.iommu_unit_quarantined(0x1500_1000, 0x1000));
            assert!(!owner.ram_allowed(0x8000_0000, PAGE_SIZE, None));
            assert!(!owner.ram_allowed(0x9000_1000, PAGE_SIZE, None));
            let retained = owner.0.retained.borrow();
            let payload = retained.as_ref().unwrap();
            assert_eq!(payload.device_ctxs[0].pgd_phys, 0x8000_0000);
            assert_eq!(payload.device_vmar_mappings.len(), 1);
            assert_eq!(payload.device_ctx_faults.len(), 1);
            assert_eq!(payload.port_bindings.len(), 1);
            drop(retained);
            drop(process);
            assert!(backing.upgrade().is_some());
        }

        #[test]
        fn replacement_denies_new_root_data_and_coarse_neighbor_aliases() {
            let (engine, process, _, backing) = mapped_context();
            let owner = engine.device_owner.clone();
            drop(engine);
            drop(process);
            let mut replacement = SyscallEngine::with_device_owner(owner);
            let mut caller = test_process(&mut replacement);
            for constructor in [
                Vmo::from_physical_range,
                Vmo::from_mmio_range,
                Vmo::from_dma_range,
            ] {
                for phys in [0x8000_0000, 0x9000_1000] {
                    assert_eq!(
                        replacement.root_vmo_create(
                            &mut caller,
                            constructor(phys, PAGE_SIZE).unwrap(),
                            Rights::READ
                        ),
                        Err(ObjectError::AccessDenied)
                    );
                }
            }
            assert_eq!(
                replacement.root_vmo_create(
                    &mut caller,
                    Vmo::from_mmio_range(0x8000_1000, PAGE_SIZE).unwrap(),
                    Rights::READ
                ),
                Err(ObjectError::AccessDenied)
            );
            assert!(replacement
                .root_vmo_create(
                    &mut caller,
                    Vmo::from_physical_range(0x8000_1000, PAGE_SIZE).unwrap(),
                    Rights::READ
                )
                .is_ok());
            let root = replacement
                .root_resource_create(&mut caller, 0, u64::MAX, 0, 0)
                .unwrap();
            assert_eq!(
                replacement.dispatch(
                    &mut caller,
                    KernelCall::IoMmuFromResource {
                        resource: root,
                        kind: kumo_abi::sys::IoMmuKind::Virtio as u32,
                        phys_base: 0x1500_1000,
                        len: PAGE_SIZE,
                    }
                ),
                KernelCallResult::Status(Errno::BadState.status())
            );
            assert!(backing.upgrade().is_some());
        }

        #[test]
        fn stale_physical_aliases_are_rechecked_before_read_write_or_mapping() {
            let (engine, process, _, _) = mapped_context();
            let mut replacement = SyscallEngine::with_device_owner(engine.device_owner.clone());
            let mut caller = test_process(&mut replacement);
            // This alias predates loss of the old engine; its unrelated VMO identity
            // must not gain authority merely because koids restart at one. — KESTREL
            let stale = replacement
                .root_vmo_create(
                    &mut caller,
                    Vmo::from_physical_range(0x9000_1000, PAGE_SIZE).unwrap(),
                    Rights::READ | Rights::WRITE,
                )
                .unwrap();
            drop(engine);
            drop(process);
            let mut bytes = [0u8; 16];
            assert_eq!(
                replacement.dispatch(
                    &mut caller,
                    KernelCall::VmoRead {
                        vmo: stale,
                        offset: 0,
                        dest: bytes.as_mut_ptr(),
                        len: bytes.len(),
                    }
                ),
                KernelCallResult::Status(Errno::AccessDenied.status())
            );
            assert_eq!(
                replacement.dispatch(
                    &mut caller,
                    KernelCall::VmoWrite {
                        vmo: stale,
                        offset: 0,
                        src: bytes.as_ptr(),
                        len: bytes.len(),
                    }
                ),
                KernelCallResult::Status(Errno::AccessDenied.status())
            );
            assert_eq!(
                replacement.dispatch(
                    &mut caller,
                    KernelCall::VmarMap {
                        process_handle: kumo_abi::INVALID_HANDLE,
                        vmo_handle: stale,
                        vmo_offset: 0,
                        virt: 0x2000_0000,
                        len: PAGE_SIZE,
                        flags: PageFlags::USER | PageFlags::WRITE,
                    }
                ),
                KernelCallResult::Status(Errno::AccessDenied.status())
            );
            assert!(caller.mappings().is_empty());
        }

        #[test]
        fn preexisting_iommu_handles_in_a_replacement_cannot_reinitialize_a_retained_unit() {
            let (engine, process, _, _) = mapped_context();
            let mut replacement = SyscallEngine::with_device_owner(engine.device_owner.clone());
            let mut caller = test_process(&mut replacement);
            let (iommu, _) = create_test_iommu_with_kind(
                &mut replacement,
                &mut caller,
                kumo_abi::sys::IoMmuKind::SmmuV3 as u32,
            );
            drop(engine);
            drop(process);
            assert_eq!(
                replacement.dispatch(
                    &mut caller,
                    KernelCall::DeviceCtxCreate {
                        iommu,
                        stream_or_rid: 5
                    }
                ),
                KernelCallResult::Status(Errno::BadState.status())
            );
            assert!(replacement.devices.device_ctxs.is_empty());
        }

        #[test]
        fn original_vmo_identity_survives_but_metadata_lookalikes_cannot_bypass_retention() {
            let (engine, process, _, backing) = mapped_context();
            let owner = engine.device_owner.clone();
            let original = backing.upgrade().unwrap();
            let lookalike = VmoEntry {
                koid: original.koid,
                vmo: original.vmo,
                frames: RefCell::new(Vec::new()),
            };
            drop(engine);
            drop(process);
            assert!(owner.ram_allowed(0x9000_1000, PAGE_SIZE, Some(&original)));
            assert!(!owner.ram_allowed(0x9000_1000, PAGE_SIZE, Some(&lookalike)));
            assert!(!owner.ram_allowed(0x8000_0000, PAGE_SIZE, Some(&original)));
        }

        #[test]
        fn anonymous_backing_frames_and_several_lost_generations_remain_reserved() {
            let owner = KernelDeviceOwner::new();
            for frame in [0x9100_0000, 0x9200_0000] {
                let mut engine = SyscallEngine::with_device_owner(owner.clone());
                let mut process = test_process(&mut engine);
                let ctx = create_test_device_ctx(&mut engine, &mut process);
                let koid = process.handles().get(ctx).unwrap().koid;
                let backing = Rc::new(VmoEntry {
                    koid: KoId(99),
                    vmo: Vmo::new(2 * PAGE_SIZE).unwrap(),
                    frames: RefCell::new(vec![None, Some(frame)]),
                });
                engine.devices.device_vmar_mappings.push(DeviceVmarMapping {
                    ctx_koid: koid,
                    iova: 0x4000,
                    len: PAGE_SIZE,
                    vmo_offset: PAGE_SIZE,
                    backing,
                });
                drop(engine);
                drop(process);
            }
            for frame in [0x9100_0000, 0x9200_0000] {
                assert!(!owner.ram_allowed(frame, PAGE_SIZE, None));
            }
            assert!(owner.ram_allowed(0x9300_0000, PAGE_SIZE, None));
            assert!(!owner.ram_allowed(0x9100_0000 - 1, 2, None));
        }

        #[test]
        fn a_contended_retention_store_leaks_safely_and_freezes_all_new_admission() {
            let (engine, process, _, backing) = mapped_context();
            let owner = engine.device_owner.clone();
            let borrow = owner.0.retained.borrow_mut();
            drop(engine);
            drop(process);
            drop(borrow);
            assert!(backing.upgrade().is_some());
            assert!(!owner.ram_allowed(0xdead_0000, PAGE_SIZE, None));
            assert!(!owner.unit_allowed(0xdead_0000, PAGE_SIZE));
            let mut replacement = SyscallEngine::with_device_owner(owner);
            let mut caller = test_process(&mut replacement);
            assert_eq!(
                replacement.root_vmo_create(
                    &mut caller,
                    Vmo::from_physical_range(0xdead_0000, PAGE_SIZE).unwrap(),
                    Rights::READ
                ),
                Err(ObjectError::AccessDenied)
            );
        }

        #[test]
        fn confirmed_released_contexts_do_not_enter_kernel_retention() {
            let (mut engine, mut process, ctx, backing) = mapped_context();
            let owner = engine.device_owner.clone();
            process.handles_mut().close(ctx).unwrap();
            assert_eq!(engine.retire_device_contexts_with(|_, _| true).released, 1);
            drop(engine);
            assert!(owner.0.retained.borrow().is_none());
            assert!(backing.upgrade().is_none());
            assert!(owner.unit_allowed(0x1500_0000, 0x1000));
        }
    }

    #[test]
    fn context_binding_observes_duplicate_close_and_process_drain_without_owning_a_capability() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let handle = create_test_device_ctx(&mut engine, &mut process);
        let koid = process.handles().get(handle).unwrap().koid;
        let duplicate = match engine.dispatch(
            &mut process,
            KernelCall::HandleDuplicate {
                handle,
                rights: Rights::MAP,
            },
        ) {
            KernelCallResult::Handle(handle) => handle,
            result => panic!("{result:?}"),
        };
        assert_eq!(
            engine.dispatch(&mut process, KernelCall::HandleClose { handle }),
            KernelCallResult::Status(Errno::Ok.status())
        );
        assert_eq!(engine.device_ctx_has_capabilities(koid), Ok(true));
        let entries = process.handles_mut().drain();
        assert_eq!(
            process.handles().get(duplicate),
            Err(ObjectError::BadHandle)
        );
        assert_eq!(engine.device_ctx_has_capabilities(koid), Ok(true));
        drop(entries);
        assert_eq!(engine.device_ctx_has_capabilities(koid), Ok(false));
        assert_eq!(
            engine.device_ctx_has_capabilities(KoId(u64::MAX)),
            Err(Errno::BadHandle)
        );
        // Observation alone cannot free the binding or reuse its stream. The explicit
        // retirement pass must obtain a checked backend result. — KESTREL
        assert_eq!(engine.devices.device_ctxs.len(), 1);
    }

    #[cfg(feature = "arch_aarch64")]
    mod device_pins {
        use super::*;
        use crate::device_memory::{OfflineMappings, PageMemory, TablePool};
        use crate::mm::PAGE_SIZE;
        use alloc::rc::Weak;
        use kumo_hal::active::DeviceTableAccess as Access;
        use std::collections::BTreeMap;

        #[derive(Default)]
        struct Model {
            words: BTreeMap<u64, Vec<u32>>,
            limit: Option<usize>,
            guard: Option<Weak<VmoEntry>>,
        }
        #[derive(Clone, Default)]
        struct Memory(Rc<RefCell<Model>>);
        impl PageMemory for Memory {
            fn allocate(&mut self, excluded: &[kumo_abi::Range]) -> Option<u64> {
                let mut m = self.0.borrow_mut();
                if m.limit.is_some_and(|limit| m.words.len() >= limit) {
                    return None;
                }
                let mut phys = (m.words.len() as u64 + 1) * PAGE_SIZE;
                while excluded
                    .iter()
                    .any(|r| ranges_overlap(r.start, r.len, phys, PAGE_SIZE))
                {
                    phys += PAGE_SIZE;
                }
                m.words.insert(phys, vec![0; 1024]);
                Some(phys)
            }
            fn read(&self, page: u64, index: usize) -> u32 {
                self.0.borrow().words[&page][index]
            }
            fn write(&mut self, page: u64, index: usize, word: u32) {
                let mut m = self.0.borrow_mut();
                if let Some(owner) = &m.guard {
                    assert!(
                        owner.upgrade().is_some(),
                        "descriptor removed after backing released"
                    );
                }
                m.words.get_mut(&page).unwrap()[index] = word;
            }
        }

        fn gop_fixture() -> (SyscallEngine, Process, Handle, BootInfo) {
            use kumo_abi::{
                Framebuffer, FramebufferFormat, MemRegion, MemRegionKind, Range, RawSlice,
            };
            static REGIONS: [MemRegion; 1] = [MemRegion {
                range: Range::new(0x9000_0000, 3 * PAGE_SIZE),
                kind: MemRegionKind::Reserved,
                _reserved: 0,
            }];
            let mut boot = BootInfo::empty(kumo_abi::ABI_VERSION);
            boot.flags =
                BootInfo::FLAG_FRAMEBUFFER_PRESENT | BootInfo::FLAG_FRAMEBUFFER_RESERVED_RAM;
            boot.framebuffer = Framebuffer {
                phys: 0x9000_0000,
                len: PAGE_SIZE + 4,
                width: 1,
                height: 1025,
                stride: 1,
                format: FramebufferFormat::Bgr,
            };
            boot.mem_regions = RawSlice::from_slice(&REGIONS);
            let mut engine = SyscallEngine::new();
            engine.set_boot_info(boot);
            let mut process = test_process(&mut engine);
            let vmo = Vmo::from_mmio_range(boot.framebuffer.phys, boot.framebuffer.len).unwrap();
            let handle = engine
                .root_vmo_create(
                    &mut process,
                    vmo,
                    Rights::READ | Rights::WRITE | Rights::DUPLICATE,
                )
                .unwrap();
            (engine, process, handle, boot)
        }

        #[test]
        fn only_exact_attested_gop_mints_transferable_data_with_source_authority() {
            let (mut e, mut p, _, boot) = gop_fixture();
            let root = e
                .root_resource_create(&mut p, 0x90000000, 3 * PAGE_SIZE, 0, 0)
                .unwrap();
            let ro = p
                .handles_mut()
                .duplicate(root, Rights::READ | Rights::DUPLICATE | Rights::TRANSFER)
                .unwrap();
            let confined = p
                .handles_mut()
                .duplicate(root, Rights::READ | Rights::DUPLICATE)
                .unwrap();
            for (res, base, len, yes, writable) in [
                (root, boot.framebuffer.phys, 2 * PAGE_SIZE, true, true),
                (ro, boot.framebuffer.phys, 2 * PAGE_SIZE, true, false),
                (confined, boot.framebuffer.phys, 2 * PAGE_SIZE, false, false),
                (root, boot.framebuffer.phys, PAGE_SIZE, false, true),
                (
                    root,
                    boot.framebuffer.phys + PAGE_SIZE,
                    2 * PAGE_SIZE,
                    false,
                    true,
                ),
            ] {
                let KernelCallResult::Handle(h) = e.dispatch(
                    &mut p,
                    KernelCall::ResourceMintMmio {
                        resource: res,
                        phys_base: base,
                        len,
                    },
                ) else {
                    panic!("mint");
                };
                let entry = p.handles().get(h).unwrap();
                assert_eq!(entry.rights.contains(Rights::TRANSFER), yes);
                assert_eq!(entry.rights.contains(Rights::WRITE), writable);
            }
            e.framebuffer_ram = None;
            let KernelCallResult::Handle(h) = e.dispatch(
                &mut p,
                KernelCall::ResourceMintMmio {
                    resource: root,
                    phys_base: boot.framebuffer.phys,
                    len: 2 * PAGE_SIZE,
                },
            ) else {
                panic!("mint");
            };
            assert!(!p
                .handles()
                .get(h)
                .unwrap()
                .rights
                .contains(Rights::TRANSFER));
        }

        #[test]
        fn gop_pins_require_read_authority_and_the_exact_attested_vmo() {
            let (mut engine, mut process, handle, _) = gop_fixture();
            let pin = engine
                .pin_device_vmo_read_only(&process, handle, 0, 2 * PAGE_SIZE, Rights::READ)
                .unwrap();
            assert_eq!(pin.pages(), [0x9000_0000, 0x9000_1000]);
            for rights in [
                Rights::empty(),
                Rights::WRITE,
                Rights::READ | Rights::WRITE,
                Rights::MAP,
            ] {
                assert!(matches!(
                    engine.pin_device_vmo_read_only(&process, handle, 0, 2 * PAGE_SIZE, rights),
                    Err(Errno::AccessDenied)
                ));
            }
            let no_read = process
                .handles_mut()
                .duplicate(handle, Rights::WRITE)
                .unwrap();
            assert!(matches!(
                engine.pin_device_vmo_read_only(&process, no_read, 0, 2 * PAGE_SIZE, Rights::READ),
                Err(Errno::AccessDenied)
            ));
            for (offset, len) in [(0, PAGE_SIZE), (PAGE_SIZE, PAGE_SIZE)] {
                assert!(matches!(
                    engine.pin_device_vmo_read_only(&process, handle, offset, len, Rights::READ),
                    Err(Errno::NotSupported)
                ));
            }
            for (base, size, offset) in [
                (0x9000_0000, 3 * PAGE_SIZE, 0),
                (0x9000_1000, 2 * PAGE_SIZE, 0),
                (0x8fff_f000, 3 * PAGE_SIZE, PAGE_SIZE),
                (0xa000_0000, 2 * PAGE_SIZE, 0),
            ] {
                let vmo = Vmo::from_mmio_range(base, size).unwrap();
                let h = engine
                    .root_vmo_create(&mut process, vmo, Rights::READ)
                    .unwrap();
                assert!(matches!(
                    engine.pin_device_vmo_read_only(
                        &process,
                        h,
                        offset,
                        2 * PAGE_SIZE,
                        Rights::READ
                    ),
                    Err(Errno::NotSupported)
                ));
            }
            let entry = engine
                .vmos
                .iter()
                .find(|v| v.koid == pin.koid())
                .unwrap()
                .clone();
            assert!(matches!(
                VmoPin::new(entry, 0, 2 * PAGE_SIZE, || unreachable!()),
                Err(Errno::NotSupported)
            ));
        }

        #[test]
        fn framebuffer_claim_and_mutable_boot_copies_do_not_authorize_gop_pins() {
            let (mut engine, mut process, handle, mut boot) = gop_fixture();
            let valid = boot;
            boot.flags &= !BootInfo::FLAG_FRAMEBUFFER_RESERVED_RAM;
            engine.set_boot_info(boot);
            let resource = engine
                .root_resource_create(&mut process, 0x9000_0000, 3 * PAGE_SIZE, 0, 0)
                .unwrap();
            assert_eq!(
                engine.dispatch(
                    &mut process,
                    KernelCall::FramebufferClaim {
                        resource,
                        phys_base: 0x9000_0000,
                        len: 2 * PAGE_SIZE,
                    }
                ),
                KernelCallResult::Status(Errno::Ok.status())
            );
            boot.flags |= BootInfo::FLAG_FRAMEBUFFER_RESERVED_RAM;
            assert!(matches!(
                engine.pin_device_vmo_read_only(&process, handle, 0, 2 * PAGE_SIZE, Rights::READ),
                Err(Errno::NotSupported)
            ));
            engine.set_boot_info(valid);
            boot.framebuffer.phys += PAGE_SIZE;
            let false_gop = engine
                .root_vmo_create(
                    &mut process,
                    Vmo::from_mmio_range(boot.framebuffer.phys, 2 * PAGE_SIZE).unwrap(),
                    Rights::READ,
                )
                .unwrap();
            assert!(matches!(
                engine.pin_device_vmo_read_only(
                    &process,
                    false_gop,
                    0,
                    2 * PAGE_SIZE,
                    Rights::READ
                ),
                Err(Errno::NotSupported)
            ));
            assert!(engine
                .pin_device_vmo_read_only(&process, handle, 0, 2 * PAGE_SIZE, Rights::READ)
                .is_ok());
            boot.flags = 0;
            engine.set_boot_info(boot);
            assert!(matches!(
                engine.pin_device_vmo_read_only(&process, handle, 0, 2 * PAGE_SIZE, Rights::READ),
                Err(Errno::NotSupported)
            ));
        }

        #[test]
        fn gop_padding_and_identity_remain_pinned_until_exact_offline_unmap() {
            let (mut engine, mut process, handle, _) = gop_fixture();
            let pin = engine
                .pin_device_vmo_read_only(&process, handle, 0, 2 * PAGE_SIZE, Rights::READ)
                .unwrap();
            let weak = Rc::downgrade(&pin.entry);
            let memory = Memory::default();
            memory.0.borrow_mut().guard = Some(weak.clone());
            let pool = crate::device_memory::TablePool::new(memory.clone());
            let mut mappings = crate::device_memory::OfflineMappings::new(&pool).unwrap();
            mappings.map(0x003f_f000, pin).unwrap();
            process.handles_mut().close(handle).unwrap();
            drop(process);
            drop(engine);
            assert!(weak.upgrade().is_some());
            assert_eq!(
                mappings.walk(0x0040_0fff, Access::Read),
                Ok(Some(0x9000_1fff))
            );
            assert_eq!(mappings.walk(0x0040_1000, Access::Read), Ok(None));
            assert_eq!(
                mappings.walk(0x003f_f000, Access::Write),
                Err(Errno::AccessDenied)
            );
            assert_eq!(
                mappings.unmap(0x003f_f000, PAGE_SIZE),
                Err(Errno::InvalidArgs)
            );
            assert!(weak.upgrade().is_some());
            mappings.unmap(0x003f_f000, 2 * PAGE_SIZE).unwrap();
            assert!(weak.upgrade().is_none());
            memory.0.borrow_mut().guard = None;
            drop(mappings);
            assert!(pool.all_free());
        }

        fn fixture() -> (SyscallEngine, Process, Handle) {
            let mut engine = SyscallEngine::new();
            let mut process = test_process(&mut engine);
            let handle = create_physical_vmo(&mut engine, &mut process, 0x1000_0000, 3 * PAGE_SIZE);
            (engine, process, handle)
        }

        #[test]
        fn pin_requires_current_read_authority_and_an_exact_supported_ram_range() {
            let (mut engine, mut process, handle) = fixture();
            let no_read = process
                .handles_mut()
                .duplicate(handle, Rights::WRITE)
                .unwrap();
            let weak = Rc::downgrade(&engine.vmos[0]);
            for rights in [
                Rights::empty(),
                Rights::WRITE,
                Rights::READ | Rights::WRITE,
                Rights::MAP,
            ] {
                assert!(matches!(
                    engine.pin_device_vmo_read_only(&process, handle, 0, PAGE_SIZE, rights),
                    Err(Errno::AccessDenied)
                ));
            }
            assert!(matches!(
                engine.pin_device_vmo_read_only(&process, no_read, 0, PAGE_SIZE, Rights::READ),
                Err(Errno::AccessDenied)
            ));
            for (offset, len) in [
                (0, 0),
                (1, PAGE_SIZE),
                (0, 1),
                (3 * PAGE_SIZE, PAGE_SIZE),
                (u64::MAX - 4095, PAGE_SIZE),
            ] {
                assert!(matches!(
                    engine.pin_device_vmo_read_only(&process, handle, offset, len, Rights::READ),
                    Err(Errno::InvalidArgs)
                ));
            }
            for vmo in [
                Vmo::from_mmio_range(0x2000_0000, PAGE_SIZE).unwrap(),
                Vmo::from_physical_range(1 << 32, PAGE_SIZE).unwrap(),
                Vmo::from_dma_range((1 << 32) - PAGE_SIZE, 2 * PAGE_SIZE).unwrap(),
            ] {
                let h = engine
                    .root_vmo_create(&mut process, vmo, Rights::READ)
                    .unwrap();
                assert!(matches!(
                    engine.pin_device_vmo_read_only(&process, h, 0, vmo.len(), Rights::READ),
                    Err(Errno::NotSupported)
                ));
            }
            let overflow = Rc::new(VmoEntry {
                koid: KoId(98),
                vmo: Vmo::from_physical_range(u64::MAX - 4095, 2 * PAGE_SIZE).unwrap(),
                frames: RefCell::new(Vec::new()),
            });
            assert!(matches!(
                VmoPin::new(overflow, 0, 2 * PAGE_SIZE, || unreachable!()),
                Err(Errno::NotSupported)
            ));
            assert_eq!(weak.strong_count(), 1);
            let object = engine.objects.create(ObjectKind::Job);
            let job = process.handles_mut().insert(object, Rights::READ).unwrap();
            assert!(matches!(
                engine.pin_device_vmo_read_only(&process, job, 0, PAGE_SIZE, Rights::READ),
                Err(Errno::WrongType)
            ));
            process.handles_mut().close(handle).unwrap();
            assert!(matches!(
                engine.pin_device_vmo_read_only(&process, handle, 0, PAGE_SIZE, Rights::READ),
                Err(Errno::BadHandle)
            ));
        }

        #[test]
        fn published_drop_retains_actual_pins_tables_and_allocator_after_engine_loss() {
            let (engine, mut process, handle) = fixture();
            let weak = Rc::downgrade(&engine.vmos[0]);
            let memory = Memory::default();
            let pool = TablePool::new(memory.clone());
            let mut maps = OfflineMappings::new(&pool).unwrap();
            maps.map(
                0x1000,
                engine
                    .pin_device_vmo_read_only(&process, handle, 0, PAGE_SIZE, Rights::READ)
                    .unwrap(),
            )
            .unwrap();
            process.handles_mut().close(handle).unwrap();
            drop(process);
            drop(engine);
            let before = memory.0.borrow().words.clone();
            let mut cleaned = Vec::new();
            let published = maps.publish(|p| cleaned.push(p));
            assert_eq!(cleaned.last(), Some(&published.root()));
            assert_eq!(cleaned.len(), 2);
            assert_eq!(weak.strong_count(), 1);
            drop(published);
            assert_eq!(weak.strong_count(), 1);
            assert_eq!(memory.0.borrow().words, before);
            assert!(!pool.all_free());
            assert!(pool
                .prepare_vmo(Vmo::from_physical_range(cleaned[0], PAGE_SIZE).unwrap())
                .is_err());
            drop(pool);
            assert_eq!(weak.strong_count(), 1);
            assert_eq!(memory.0.borrow().words, before);
        }

        #[test]
        fn publication_clean_failure_retains_pins_as_well_as_private_tables() {
            let (engine, process, handle) = fixture();
            let weak = Rc::downgrade(&engine.vmos[0]);
            let pool = TablePool::new(Memory::default());
            let mut maps = OfflineMappings::new(&pool).unwrap();
            maps.map(
                0x1000,
                engine
                    .pin_device_vmo_read_only(&process, handle, 0, PAGE_SIZE, Rights::READ)
                    .unwrap(),
            )
            .unwrap();
            drop(process);
            drop(engine);
            assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || maps.publish(|_| panic!("clean failed"))
            ))
            .is_err());
            assert_eq!(weak.strong_count(), 1);
            assert!(!pool.all_free());
        }

        #[test]
        fn two_exact_mappings_retain_identity_after_all_source_handles_and_registry_drop() {
            let (engine, mut process, handle) = fixture();
            let koid = process.handles().get(handle).unwrap().koid;
            let weak = Rc::downgrade(&engine.vmos[0]);
            let duplicate = process
                .handles_mut()
                .duplicate(handle, Rights::READ)
                .unwrap();
            let memory = Memory::default();
            let pool = TablePool::new(memory);
            let mut maps = OfflineMappings::new(&pool).unwrap();
            for (iova, offset) in [(0x003f_f000, PAGE_SIZE), (0x0040_0000, 2 * PAGE_SIZE)] {
                maps.map(
                    iova,
                    engine
                        .pin_device_vmo_read_only(
                            &process,
                            duplicate,
                            offset,
                            PAGE_SIZE,
                            Rights::READ,
                        )
                        .unwrap(),
                )
                .unwrap();
                let record = maps.mapping(iova).unwrap();
                assert_eq!(
                    (record.koid(), record.offset(), record.len()),
                    (koid, offset, PAGE_SIZE)
                );
                assert_eq!(record.pages(), [0x1000_0000 + offset]);
            }
            process.handles_mut().close(handle).unwrap();
            process.handles_mut().close(duplicate).unwrap();
            drop(process);
            drop(engine);
            assert_eq!(weak.strong_count(), 2);
            assert_eq!(maps.walk(0x0040_0123, Access::Read), Ok(Some(0x1000_2123)));
            assert_eq!(
                maps.walk(0x0040_0123, Access::Write),
                Err(Errno::AccessDenied)
            );
            assert_eq!(
                maps.unmap(0x003f_f000, 2 * PAGE_SIZE),
                Err(Errno::InvalidArgs)
            );
            assert_eq!(weak.strong_count(), 2);
            maps.unmap(0x003f_f000, PAGE_SIZE).unwrap();
            assert_eq!(maps.walk(0x003f_f000, Access::Read), Ok(None));
            assert_eq!(weak.strong_count(), 1);
            maps.unmap(0x0040_0000, PAGE_SIZE).unwrap();
            assert!(weak.upgrade().is_none());
            assert_eq!(maps.walk(0x0040_0000, Access::Read), Ok(None));
        }

        #[test]
        fn failed_map_rolls_back_prefix_and_pin_without_harming_existing_record() {
            let (engine, process, handle) = fixture();
            let weak = Rc::downgrade(&engine.vmos[0]);
            let memory = Memory::default();
            let pool = TablePool::new(memory.clone());
            let mut maps = OfflineMappings::new(&pool).unwrap();
            maps.map(
                0x1000,
                engine
                    .pin_device_vmo_read_only(&process, handle, 0, PAGE_SIZE, Rights::READ)
                    .unwrap(),
            )
            .unwrap();
            let before = memory.0.borrow().words.clone();
            memory.0.borrow_mut().limit = Some(2);
            assert_eq!(
                maps.map(
                    0x003f_f000,
                    engine
                        .pin_device_vmo_read_only(
                            &process,
                            handle,
                            PAGE_SIZE,
                            2 * PAGE_SIZE,
                            Rights::READ
                        )
                        .unwrap()
                ),
                Err(Errno::NoMemory)
            );
            assert_eq!(weak.strong_count(), 2);
            assert!(maps.mapping(0x003f_f000).is_none());
            assert_eq!(maps.walk(0x003f_f000, Access::Read), Ok(None));
            assert_eq!(memory.0.borrow().words, before);
            assert_eq!(
                maps.map(
                    0x1000,
                    engine
                        .pin_device_vmo_read_only(&process, handle, 0, PAGE_SIZE, Rights::READ)
                        .unwrap()
                ),
                Err(Errno::InvalidArgs)
            );
            assert_eq!(weak.strong_count(), 2);
            memory.0.borrow_mut().limit = None;
            maps.map(
                0x003f_f000,
                engine
                    .pin_device_vmo_read_only(
                        &process,
                        handle,
                        PAGE_SIZE,
                        2 * PAGE_SIZE,
                        Rights::READ,
                    )
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(weak.strong_count(), 3);
            maps.unmap(0x003f_f000, 2 * PAGE_SIZE).unwrap();
            assert_eq!(maps.walk(0x1000, Access::Read), Ok(Some(0x1000_0000)));
        }

        #[test]
        fn offline_drop_removes_table_descriptors_while_backing_is_still_owned() {
            let (engine, process, handle) = fixture();
            let weak = Rc::downgrade(&engine.vmos[0]);
            let memory = Memory::default();
            let pool = TablePool::new(memory.clone());
            let mut maps = OfflineMappings::new(&pool).unwrap();
            maps.map(
                0x003f_f000,
                engine
                    .pin_device_vmo_read_only(&process, handle, 0, 2 * PAGE_SIZE, Rights::READ)
                    .unwrap(),
            )
            .unwrap();
            drop(engine);
            drop(process);
            memory.0.borrow_mut().guard = Some(weak.clone());
            drop(maps);
            assert!(weak.upgrade().is_none());
            assert!(memory
                .0
                .borrow()
                .words
                .values()
                .all(|page| page.iter().all(|&word| word == 0)));
        }

        #[test]
        fn corrupt_unmap_keeps_the_complete_record_and_both_backing_references() {
            let (engine, process, handle) = fixture();
            let weak = Rc::downgrade(&engine.vmos[0]);
            let memory = Memory::default();
            let pool = TablePool::new(memory.clone());
            let mut maps = OfflineMappings::new(&pool).unwrap();
            maps.map(
                0x003f_f000,
                engine
                    .pin_device_vmo_read_only(
                        &process,
                        handle,
                        PAGE_SIZE,
                        2 * PAGE_SIZE,
                        Rights::READ,
                    )
                    .unwrap(),
            )
            .unwrap();
            // Directory and leaves occupy model pages 1, 2, 3. Damage the suffix;
            // the valid prefix must survive the failed all-range preflight. — KESTREL
            memory
                .0
                .borrow_mut()
                .words
                .get_mut(&(3 * PAGE_SIZE))
                .unwrap()[0] = 8;
            let before = memory.0.borrow().words.clone();
            assert_eq!(maps.unmap(0x003f_f000, 2 * PAGE_SIZE), Err(Errno::BadState));
            assert_eq!(memory.0.borrow().words, before);
            assert_eq!(maps.walk(0x003f_f000, Access::Read), Ok(Some(0x1000_1000)));
            assert_eq!(
                maps.mapping(0x003f_f000).unwrap().pages(),
                [0x1000_1000, 0x1000_2000]
            );
            assert_eq!(weak.strong_count(), 2);
        }

        #[test]
        fn anonymous_pins_share_materialized_pages_and_failed_pin_keeps_only_vmo_cache() {
            let entry = Rc::new(VmoEntry {
                koid: KoId(99),
                vmo: Vmo::new(3 * PAGE_SIZE).unwrap(),
                frames: RefCell::new(Vec::new()),
            });
            let weak = Rc::downgrade(&entry);
            let mut calls = 0;
            let failed = VmoPin::new(entry.clone(), PAGE_SIZE, 2 * PAGE_SIZE, || {
                calls += 1;
                if calls == 1 {
                    Ok(0x2000_0000)
                } else {
                    Err(Errno::NoMemory)
                }
            });
            assert!(matches!(failed, Err(Errno::NoMemory)));
            assert_eq!(weak.strong_count(), 1);
            assert_eq!(*entry.frames.borrow(), [None, Some(0x2000_0000), None]);
            let first =
                VmoPin::new(entry.clone(), PAGE_SIZE, 2 * PAGE_SIZE, || Ok(0x3000_0000)).unwrap();
            let second = VmoPin::new(entry.clone(), PAGE_SIZE, 2 * PAGE_SIZE, || {
                panic!("shared page allocated twice")
            })
            .unwrap();
            assert_eq!(first.pages(), [0x2000_0000, 0x3000_0000]);
            assert_eq!(first.pages(), second.pages());
            drop(entry);
            drop(first);
            assert_eq!(weak.strong_count(), 1);
            drop(second);
            assert!(weak.upgrade().is_none());
        }

        #[test]
        fn dma_pin_accepts_the_top_32_bit_page_and_engine_clone_shares_vmo_pages() {
            let (mut engine, mut process, _) = fixture();
            let dma = engine
                .root_vmo_create(
                    &mut process,
                    Vmo::from_dma_range((1 << 32) - PAGE_SIZE, PAGE_SIZE).unwrap(),
                    Rights::READ,
                )
                .unwrap();
            let pin = engine
                .pin_device_vmo_read_only(&process, dma, 0, PAGE_SIZE, Rights::READ)
                .unwrap();
            assert_eq!(pin.pages(), [(1 << 32) - PAGE_SIZE]);
            let clone = engine.clone();
            assert!(Rc::ptr_eq(&engine.vmos[0], &clone.vmos[0]));
            let anon = Rc::new(VmoEntry {
                koid: KoId(100),
                vmo: Vmo::new(PAGE_SIZE).unwrap(),
                frames: RefCell::new(vec![Some(1 << 32)]),
            });
            assert!(matches!(
                VmoPin::new(anon.clone(), 0, PAGE_SIZE, || unreachable!()),
                Err(Errno::NotSupported)
            ));
            assert_eq!(Rc::strong_count(&anon), 1);
        }
    }

    #[cfg(feature = "arch_aarch64")]
    #[test]
    fn register_write_checks_authority_whole_window_widths_and_selectors() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let root = engine
            .root_resource_create(&mut process, 0xfdd9_0000, 0x7000, 0, 0)
            .unwrap();
        let narrow = engine
            .root_resource_create(&mut process, 0xfdd9_0000, 0x1000, 0, 0)
            .unwrap();
        let reader = process.handles_mut().duplicate(root, Rights::READ).unwrap();
        let (channel, _) = create_channel(&mut engine, &mut process);
        let call = |resource, base, index, value| KernelCall::DeviceRegisterWrite {
            resource,
            aperture_base: base,
            index,
            value,
        };
        for (request, expected) in [
            (call(Handle(u32::MAX), 0xfdd9_0000, 0, 2), Errno::BadHandle),
            (call(channel, 0xfdd9_0000, 0, 2), Errno::WrongType),
            (call(reader, 0xfdd9_0000, 0, 2), Errno::AccessDenied),
            (call(root, 0xfde8_0000, 0, 2), Errno::AccessDenied),
            (call(root, u64::MAX, 0, 2), Errno::AccessDenied),
            (call(narrow, 0xfdd9_0000, 0, 2), Errno::AccessDenied),
            (call(root, 0xfdd9_0004, 0, 2), Errno::NotSupported),
            (call(root, 0xfdd9_0000, 1 << 32, 2), Errno::InvalidArgs),
            (call(root, 0xfdd9_0000, 0, 1 << 32), Errno::InvalidArgs),
            (call(root, 0xfdd9_0000, 0x24, 0), Errno::InvalidArgs),
            (call(root, 0xfdd9_0000, 0, 1 << 31), Errno::InvalidArgs),
            (call(root, 0xfdd9_0000, 14, 0x48005), Errno::InvalidArgs),
            (call(root, 0xfdd9_0000, 15, 0x20002), Errno::InvalidArgs),
            (call(root, 0xfdd9_0000, 0, 2), Errno::NotSupported),
        ] {
            assert_eq!(
                engine.dispatch(&mut process, request),
                KernelCallResult::Status(expected.status())
            );
        }
    }

    #[cfg(feature = "arch_aarch64")]
    #[test]
    fn full_and_partial_protected_mints_are_read_only_even_from_write_authority() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let root = engine
            .root_resource_create(&mut process, 0xfdd8_f000, 0xa000, 0, 0)
            .unwrap();
        for (base, len) in [
            (0xfdd9_0000, 0x8000),
            (0xfdd8_f000, 0x2000),
            (0xfdd9_7000, 0x1000),
        ] {
            let KernelCallResult::Handle(vmo) = engine.dispatch(
                &mut process,
                KernelCall::ResourceMintMmio {
                    resource: root,
                    phys_base: base,
                    len,
                },
            ) else {
                panic!("read mint refused");
            };
            assert_eq!(
                process.handles().get(vmo).unwrap().rights,
                Rights::READ | Rights::DUPLICATE
            );
            let rw = PageFlags::READ | PageFlags::WRITE | PageFlags::DEVICE;
            assert_eq!(
                map_page(
                    &mut engine,
                    &mut process,
                    kumo_abi::INVALID_HANDLE,
                    vmo,
                    0,
                    rw
                ),
                KernelCallResult::Status(Errno::AccessDenied.status())
            );
            let value = 0u32;
            assert_eq!(
                engine.dispatch(
                    &mut process,
                    KernelCall::VmoWrite {
                        vmo,
                        offset: 0,
                        src: &value as *const u32 as *const u8,
                        len: 4
                    }
                ),
                KernelCallResult::Status(Errno::AccessDenied.status())
            );
            assert!(mapped(map_page(
                &mut engine,
                &mut process,
                kumo_abi::INVALID_HANDLE,
                vmo,
                0,
                PageFlags::READ | PageFlags::DEVICE
            )));
        }
        for base in [0xfdd8_f000, 0xfdd9_8000] {
            let KernelCallResult::Handle(vmo) = engine.dispatch(
                &mut process,
                KernelCall::ResourceMintMmio {
                    resource: root,
                    phys_base: base,
                    len: 0x1000,
                },
            ) else {
                panic!("neighbor refused");
            };
            assert!(process
                .handles()
                .get(vmo)
                .unwrap()
                .rights
                .contains(Rights::WRITE));
        }
    }

    #[cfg(feature = "arch_aarch64")]
    #[test]
    fn stale_physical_dma_and_mmio_write_aliases_cannot_bypass_reservation() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let parent = crate::task::Job::root(engine.objects_mut());
        let child = engine
            .process_create(
                &mut process,
                &parent,
                Vmar::new(0x1000, 0x2000_0000).unwrap(),
            )
            .unwrap();
        for constructor in [
            Vmo::from_physical_range,
            Vmo::from_mmio_range,
            Vmo::from_dma_range,
        ] {
            // The protected page is reached through a nonzero VMO offset. — KESTREL
            let vmo = constructor(0xfdd8_f000, 0xa000).unwrap();
            let handle = engine
                .root_vmo_create(&mut process, vmo, Rights::READ | Rights::WRITE)
                .unwrap();
            for offset in [0x1000, 0x2000, 0x8000] {
                for target in [kumo_abi::INVALID_HANDLE, child] {
                    let call = KernelCall::VmarMap {
                        process_handle: target,
                        vmo_handle: handle,
                        vmo_offset: offset,
                        virt: 0x1000,
                        len: 0x1000,
                        flags: PageFlags::READ | PageFlags::WRITE | PageFlags::DEVICE,
                    };
                    assert_eq!(
                        engine.dispatch(&mut process, call),
                        KernelCallResult::Status(Errno::AccessDenied.status())
                    );
                }
                let value = 0u32;
                let call = KernelCall::VmoWrite {
                    vmo: handle,
                    offset,
                    src: &value as *const u32 as *const u8,
                    len: 4,
                };
                assert_eq!(
                    engine.dispatch(&mut process, call),
                    KernelCallResult::Status(Errno::AccessDenied.status())
                );
            }
            assert!(physical_vmo_write_allowed(&vmo, 0, 0x1000));
            assert!(physical_vmo_write_allowed(&vmo, 0x9000, 0x1000));
            assert!(!physical_vmo_write_allowed(&vmo, 0xfff, 2));
        }
    }

    #[test]
    fn device_reset_checks_type_rights_range_and_raw_argument_widths_before_hardware() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let root = engine
            .root_resource_create(&mut process, 0xfed6_0000, 0x2000, 0, 0)
            .unwrap();
        let reader = process.handles_mut().duplicate(root, Rights::READ).unwrap();
        let (channel, _) = create_channel(&mut engine, &mut process);
        let call = |resource, base, line, asserted| KernelCall::DeviceResetSet {
            resource,
            aperture_base: base,
            line,
            asserted,
        };
        for (request, expected) in [
            (call(Handle(u32::MAX), 0xfed6_0000, 0, 0), Errno::BadHandle),
            (call(channel, 0xfed6_0000, 0, 0), Errno::WrongType),
            (call(reader, 0xfed6_0000, 0, 0), Errno::AccessDenied),
            (call(root, 0xfd7c_0000, 0, 0), Errno::AccessDenied),
            (call(root, u64::MAX, 0, 0), Errno::AccessDenied),
            (call(root, 0xfed6_0000, 1 << 32, 0), Errno::InvalidArgs),
            (call(root, 0xfed6_0000, 0, 2), Errno::InvalidArgs),
            (call(root, 0xfed6_0000, 0, u64::MAX), Errno::InvalidArgs),
            (call(root, 0xfed6_0000, 0, 1), Errno::NotSupported),
        ] {
            assert!(
                matches!(engine.dispatch(&mut process, request), KernelCallResult::Status(s) if s == expected.status())
            );
        }
    }

    fn test_process(engine: &mut SyscallEngine) -> Process {
        let job = crate::task::Job::root(engine.objects_mut());
        let vmar = crate::mm::Vmar::new(0xffff_0000_0000_0000, crate::mm::PAGE_SIZE * 16).unwrap();
        Process::new(engine.objects_mut(), &job, vmar)
    }

    #[test]
    fn process_kill_needs_manage_on_a_live_process_and_never_the_caller() {
        let mut engine = SyscallEngine::new();
        let mut parent = test_process(&mut engine);
        let job = crate::task::Job::root(engine.objects_mut());
        let vmar = crate::mm::Vmar::new(0x1000_0000, crate::mm::PAGE_SIZE * 16).unwrap();
        let child = engine.process_create(&mut parent, &job, vmar).unwrap();
        let child_koid = parent.handles().get(child).unwrap().koid;
        // The creator manages its child (J556).
        assert_eq!(engine.process_kill_target(&parent, child), Ok(child_koid));
        // A duplicate made without MANAGE can watch the child but not end it.
        let watcher = parent
            .handles_mut()
            .duplicate(child, Rights::READ | Rights::WAIT)
            .unwrap();
        assert_eq!(
            engine.process_kill_target(&parent, watcher),
            Err(Errno::AccessDenied)
        );
        // Only a process handle names a process.
        let (channel, _) = create_channel(&mut engine, &mut parent);
        assert_eq!(
            engine.process_kill_target(&parent, channel),
            Err(Errno::WrongType)
        );
        // A process ends itself by exiting, not by killing.
        let own = parent.object();
        let me = parent.handles_mut().insert(own, Rights::MANAGE).unwrap();
        assert_eq!(
            engine.process_kill_target(&parent, me),
            Err(Errno::InvalidArgs)
        );
    }

    const GPU_APERTURE: u64 = 0xfb00_0000;

    fn device_power_up(
        engine: &mut SyscallEngine,
        process: &mut Process,
        resource: Handle,
    ) -> Status {
        match engine.dispatch(
            process,
            KernelCall::DevicePowerUp {
                resource,
                aperture_base: GPU_APERTURE,
            },
        ) {
            KernelCallResult::Status(status) => status,
            other => panic!("expected a status, got {other:?}"),
        }
    }

    fn device_power_status(
        engine: &mut SyscallEngine,
        process: &mut Process,
        resource: Handle,
        record: Option<&mut DevicePowerStatus>,
        user_len: u64,
    ) -> Status {
        let user_ptr = record.map_or(0, |record| record as *mut DevicePowerStatus as u64);
        match engine.dispatch(
            process,
            KernelCall::DevicePowerStatus {
                resource,
                aperture_base: GPU_APERTURE,
                user_ptr,
                user_len,
            },
        ) {
            KernelCallResult::Status(status) => status,
            other => panic!("expected a status, got {other:?}"),
        }
    }

    #[test]
    fn device_power_calls_check_rights_first_and_find_no_unit_off_rk3588() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let root = engine
            .root_resource_create(&mut process, GPU_APERTURE, 0x20_0000, 0, u32::MAX)
            .unwrap();
        let (channel, _) = create_channel(&mut engine, &mut process);
        assert_eq!(
            device_power_up(&mut engine, &mut process, channel),
            Errno::WrongType.status()
        );
        // Raising power needs WRITE; reading status needs READ.
        let reader = process.handles_mut().duplicate(root, Rights::READ).unwrap();
        let writer = process
            .handles_mut()
            .duplicate(root, Rights::WRITE)
            .unwrap();
        assert_eq!(
            device_power_up(&mut engine, &mut process, reader),
            Errno::AccessDenied.status()
        );
        let mut record = DevicePowerStatus::default();
        assert_eq!(
            device_power_status(&mut engine, &mut process, writer, Some(&mut record), 64),
            Errno::AccessDenied.status()
        );
        // The record pointer is checked before any unit is looked up.
        assert_eq!(
            device_power_status(&mut engine, &mut process, reader, None, 64),
            Errno::InvalidArgs.status()
        );
        assert_eq!(
            device_power_status(&mut engine, &mut process, reader, Some(&mut record), 16),
            Errno::InvalidArgs.status()
        );
        // With the right rights the host HAL holds no power unit: inert, nothing written.
        assert_eq!(
            device_power_up(&mut engine, &mut process, writer),
            Errno::NotSupported.status()
        );
        assert_eq!(
            device_power_status(&mut engine, &mut process, reader, Some(&mut record), 64),
            Errno::NotSupported.status()
        );
        assert_eq!(record, DevicePowerStatus::default());
    }

    #[test]
    fn the_status_record_maps_every_readout_flag() {
        let none = DevicePowerReadout {
            powered: false,
            repair_done: false,
            bus_idle: false,
            bus_idle_requested: false,
            clock_at_boot_rate: false,
            aperture_ready: false,
            pmu: [1, 2, 3, 4, 5, 6],
            cru: [7, 8, 9, 10, 11, 12, 13, 14],
        };
        let record = device_power_status_record(&none);
        assert_eq!(record.version, DevicePowerStatus::VERSION);
        assert_eq!(record.flags, 0);
        assert_eq!(record.pmu, none.pmu);
        assert_eq!(record.cru, none.cru);
        let cases = [
            (
                DevicePowerReadout {
                    powered: true,
                    ..none
                },
                DevicePowerStatus::POWERED,
            ),
            (
                DevicePowerReadout {
                    repair_done: true,
                    ..none
                },
                DevicePowerStatus::REPAIR_DONE,
            ),
            (
                DevicePowerReadout {
                    bus_idle: true,
                    ..none
                },
                DevicePowerStatus::BUS_IDLE,
            ),
            (
                DevicePowerReadout {
                    bus_idle_requested: true,
                    ..none
                },
                DevicePowerStatus::BUS_IDLE_REQUESTED,
            ),
            (
                DevicePowerReadout {
                    clock_at_boot_rate: true,
                    ..none
                },
                DevicePowerStatus::CLOCK_AT_BOOT_RATE,
            ),
            (
                DevicePowerReadout {
                    aperture_ready: true,
                    ..none
                },
                DevicePowerStatus::APERTURE_MINTABLE,
            ),
        ];
        for (readout, flag) in cases {
            assert_eq!(device_power_status_record(&readout).flags, flag);
        }
    }

    fn map_page(
        engine: &mut SyscallEngine,
        process: &mut Process,
        target: Handle,
        vmo: Handle,
        virt: u64,
        flags: PageFlags,
    ) -> KernelCallResult {
        engine.dispatch(
            process,
            KernelCall::VmarMap {
                process_handle: target,
                vmo_handle: vmo,
                vmo_offset: 0,
                virt,
                len: crate::mm::PAGE_SIZE,
                flags,
            },
        )
    }

    fn mapped(result: KernelCallResult) -> bool {
        matches!(result, KernelCallResult::StatusAndValue { status, .. } if status == Errno::Ok.status())
    }

    #[test]
    fn a_writable_mapping_needs_write_on_the_vmo() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let vmo = create_physical_vmo(&mut engine, &mut process, 0x9100_0000, crate::mm::PAGE_SIZE);
        let read_only = process.handles_mut().duplicate(vmo, Rights::READ).unwrap();
        let rw = PageFlags(PageFlags::READ.0 | PageFlags::WRITE.0);
        let base = 0xffff_0000_0000_0000;

        assert_eq!(
            map_page(
                &mut engine,
                &mut process,
                kumo_abi::INVALID_HANDLE,
                read_only,
                base,
                rw
            ),
            KernelCallResult::Status(Errno::AccessDenied.status())
        );
        assert!(process.mappings().is_empty());
        assert!(mapped(map_page(
            &mut engine,
            &mut process,
            kumo_abi::INVALID_HANDLE,
            read_only,
            base,
            PageFlags::READ,
        )));
        assert!(mapped(map_page(
            &mut engine,
            &mut process,
            kumo_abi::INVALID_HANDLE,
            vmo,
            base + crate::mm::PAGE_SIZE,
            rw,
        )));

        // The same rule covers mapping into a child's VMAR.
        let job = crate::task::Job::root(engine.objects_mut());
        let vmar = crate::mm::Vmar::new(0x1000_0000, crate::mm::PAGE_SIZE * 16).unwrap();
        let child = engine.process_create(&mut process, &job, vmar).unwrap();
        assert_eq!(
            map_page(&mut engine, &mut process, child, read_only, 0x1000_0000, rw),
            KernelCallResult::Status(Errno::AccessDenied.status())
        );
    }

    #[test]
    fn a_read_only_resource_mints_a_read_only_vmo() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let root = engine
            .root_resource_create(&mut process, 0x9000_0000, 0x1000, 0, u32::MAX)
            .unwrap();
        let mint = |engine: &mut SyscallEngine, process: &mut Process, resource| {
            engine.dispatch(
                process,
                KernelCall::ResourceMintMmio {
                    resource,
                    phys_base: 0x9000_0000,
                    len: 0x1000,
                },
            )
        };

        let KernelCallResult::Handle(writable) = mint(&mut engine, &mut process, root) else {
            panic!("a WRITE Resource must mint");
        };
        assert!(process
            .handles()
            .get(writable)
            .unwrap()
            .rights
            .contains(Rights::READ | Rights::WRITE));

        let narrowed = process
            .handles_mut()
            .duplicate(root, Rights::READ | Rights::DUPLICATE | Rights::TRANSFER)
            .unwrap();
        let KernelCallResult::Handle(read_only) = mint(&mut engine, &mut process, narrowed) else {
            panic!("a READ Resource must mint");
        };
        assert_eq!(
            process.handles().get(read_only).unwrap().rights,
            Rights::READ | Rights::DUPLICATE
        );
        let rw = PageFlags(PageFlags::READ.0 | PageFlags::WRITE.0);
        assert_eq!(
            map_page(
                &mut engine,
                &mut process,
                kumo_abi::INVALID_HANDLE,
                read_only,
                0xffff_0000_0000_0000,
                rw,
            ),
            KernelCallResult::Status(Errno::AccessDenied.status())
        );

        let blind = process
            .handles_mut()
            .duplicate(root, Rights::TRANSFER)
            .unwrap();
        assert_eq!(
            mint(&mut engine, &mut process, blind),
            KernelCallResult::Status(Errno::AccessDenied.status())
        );
    }

    #[test]
    fn minting_the_gpu_aperture_is_unchanged_off_rk3588() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let root = engine
            .root_resource_create(&mut process, GPU_APERTURE, 0x20_0000, 0, u32::MAX)
            .unwrap();
        // No power unit is live on the host, so the guard never blocks (DESIGN/020 §5).
        assert!(matches!(
            engine.dispatch(
                &mut process,
                KernelCall::ResourceMintMmio {
                    resource: root,
                    phys_base: GPU_APERTURE,
                    len: 0x1000,
                },
            ),
            KernelCallResult::Handle(_)
        ));
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

    fn create_smmuv3_iommu(engine: &mut SyscallEngine, process: &mut Process) -> Handle {
        let resource = engine
            .root_resource_create(process, 0x1500_0000, 0x20_0000, 0, u32::MAX)
            .unwrap();
        match engine.dispatch(
            process,
            KernelCall::IoMmuFromResource {
                resource,
                kind: kumo_abi::sys::IoMmuKind::SmmuV3 as u32,
                phys_base: 0x1500_0000,
                len: 0x20_0000,
            },
        ) {
            KernelCallResult::Handle(handle) => handle,
            other => panic!("expected IoMmu handle, got {other:?}"),
        }
    }

    fn create_test_iommu_with_kind(
        engine: &mut SyscallEngine,
        process: &mut Process,
        iommu_kind: u32,
    ) -> (Handle, KoId) {
        let iommu = engine.objects.create(ObjectKind::IoMmu);
        let iommu_koid = iommu.koid();
        let handle = process
            .handles_mut()
            .insert(
                iommu,
                Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER,
            )
            .unwrap();
        engine.devices.iommus.push(IoMmuBinding {
            koid: iommu_koid,
            iommu_kind,
            phys_base: 0x1500_0000,
            len: 0x20_0000,
        });
        (handle, iommu_koid)
    }

    fn create_test_device_ctx_with_kind(
        engine: &mut SyscallEngine,
        process: &mut Process,
        iommu_kind: u32,
    ) -> Handle {
        create_test_device_ctx_with_kind_and_stream(engine, process, iommu_kind, 1)
    }

    fn create_test_device_ctx_with_kind_and_stream(
        engine: &mut SyscallEngine,
        process: &mut Process,
        iommu_kind: u32,
        stream_id: u32,
    ) -> Handle {
        let (_iommu, iommu_koid) = create_test_iommu_with_kind(engine, process, iommu_kind);
        let object = engine.objects.create(ObjectKind::DeviceCtx);
        let koid = object.koid();
        let (handle, references) = process
            .handles_mut()
            .insert_device_context(
                object,
                Rights::MAP | Rights::WAIT | Rights::DUPLICATE | Rights::TRANSFER,
            )
            .unwrap();
        engine.devices.device_ctxs.push(DeviceCtxBinding {
            koid,
            iommu_koid,
            stream_id,
            pgd_phys: 0x8000_0000,
            references,
            state: Rc::new(Cell::new(DeviceCtxState::Bound)),
        });
        handle
    }

    fn create_test_device_ctx(engine: &mut SyscallEngine, process: &mut Process) -> Handle {
        create_test_device_ctx_with_kind(engine, process, kumo_abi::sys::IoMmuKind::SmmuV3 as u32)
    }

    fn create_physical_vmo(
        engine: &mut SyscallEngine,
        process: &mut Process,
        phys_base: u64,
        size: u64,
    ) -> Handle {
        engine
            .root_vmo_create(
                process,
                crate::mm::Vmo::from_physical_range(phys_base, size).unwrap(),
                Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER,
            )
            .unwrap()
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
    fn dispatches_handle_koid_for_held_handle() {
        // The koid lookup the bind-port-by-koid pattern depends on (e.g. drv-fb recognising
        // its console channel as the source of a PortWait wake). Pins the engine contract
        // behind J248's async-child `HandleKoid` SVC arm.
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let (left, _right) = create_channel(&mut engine, &mut process);

        let want = process.handles().get(left).unwrap().koid.0 as u32;
        let got = engine.dispatch(&mut process, KernelCall::HandleKoid { handle: left });
        assert_eq!(got, KernelCallResult::Handle(Handle(want)));

        // A handle the process does not hold reports BadHandle rather than a bogus koid.
        let bogus = engine.dispatch(
            &mut process,
            KernelCall::HandleKoid {
                handle: Handle(9999),
            },
        );
        assert_eq!(bogus, KernelCallResult::Status(Errno::BadHandle.status()));
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
    fn resource_child_must_stay_inside_parent_range() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let root = engine
            .root_resource_create(&mut process, 0x1000, 0x1000, 0, u32::MAX)
            .unwrap();

        let child = engine.dispatch(
            &mut process,
            KernelCall::ResourceCreateChild {
                parent: root,
                phys_base: 0x1800,
                len: 0x100,
                irq_base: 0,
                irq_count: u32::MAX,
            },
        );
        let KernelCallResult::Handle(child) = child else {
            panic!("expected child Resource handle, got {child:?}");
        };
        assert!(process
            .handles()
            .require(child, ObjectKind::Resource, Rights::WRITE)
            .is_ok());
        assert_eq!(
            process
                .handles()
                .require(child, ObjectKind::Resource, Rights::MANAGE),
            Err(ObjectError::AccessDenied)
        );

        let outside = engine.dispatch(
            &mut process,
            KernelCall::ResourceCreateChild {
                parent: root,
                phys_base: 0x1f00,
                len: 0x200,
                irq_base: 0,
                irq_count: u32::MAX,
            },
        );
        assert_eq!(
            outside,
            KernelCallResult::Status(Errno::AccessDenied.status())
        );
    }

    #[test]
    fn mmio_minting_is_limited_to_resource_range() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let root = engine
            .root_resource_create(&mut process, 0x9000_0000, 0x1000, 0, u32::MAX)
            .unwrap();
        let child = match engine.dispatch(
            &mut process,
            KernelCall::ResourceCreateChild {
                parent: root,
                phys_base: 0x9000_0000,
                len: 0x1000,
                irq_base: 0,
                irq_count: u32::MAX,
            },
        ) {
            KernelCallResult::Handle(handle) => handle,
            other => panic!("expected child Resource handle, got {other:?}"),
        };

        let inside = engine.dispatch(
            &mut process,
            KernelCall::ResourceMintMmio {
                resource: child,
                phys_base: 0x9000_0000,
                len: 0x1000,
            },
        );
        assert!(matches!(inside, KernelCallResult::Handle(_)));

        let outside = engine.dispatch(
            &mut process,
            KernelCall::ResourceMintMmio {
                resource: child,
                phys_base: 0x9000_1000,
                len: 0x1000,
            },
        );
        assert_eq!(
            outside,
            KernelCallResult::Status(Errno::AccessDenied.status())
        );
    }

    #[test]
    fn device_ctx_create_rejects_stream_ids_outside_backend_width() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let iommu = create_smmuv3_iommu(&mut engine, &mut process);

        let result = engine.dispatch(
            &mut process,
            KernelCall::DeviceCtxCreate {
                iommu,
                stream_or_rid: u64::from(u32::MAX) + 1,
            },
        );

        assert_eq!(
            result,
            KernelCallResult::Status(Errno::InvalidArgs.status())
        );
        assert!(engine.devices.device_ctxs.is_empty());
    }

    #[test]
    fn device_ctx_create_rejects_duplicate_stream_in_same_iommu() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let (iommu, iommu_koid) = create_test_iommu_with_kind(
            &mut engine,
            &mut process,
            kumo_abi::sys::IoMmuKind::SmmuV3 as u32,
        );
        let object = engine.objects.create(ObjectKind::DeviceCtx);
        let (_, references) = process
            .handles_mut()
            .insert_device_context(object, Rights::MAP)
            .unwrap();
        engine.devices.device_ctxs.push(DeviceCtxBinding {
            koid: object.koid(),
            iommu_koid,
            stream_id: 0x820,
            pgd_phys: 0x8000_0000,
            references,
            state: Rc::new(Cell::new(DeviceCtxState::Bound)),
        });

        let duplicate = engine.dispatch(
            &mut process,
            KernelCall::DeviceCtxCreate {
                iommu,
                stream_or_rid: 0x820,
            },
        );

        assert_eq!(
            duplicate,
            KernelCallResult::Status(Errno::InvalidArgs.status())
        );
        assert_eq!(engine.devices.device_ctxs.len(), 1);
    }

    #[test]
    fn device_vmar_map_records_explicit_iova_and_rejects_overlap() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let ctx = create_test_device_ctx(&mut engine, &mut process);
        let vmo = create_physical_vmo(
            &mut engine,
            &mut process,
            0x9000_0000,
            crate::mm::PAGE_SIZE * 2,
        );

        let first = engine.dispatch(
            &mut process,
            KernelCall::DeviceVmarMap {
                ctx,
                vmo,
                vmo_offset: 0,
                len: crate::mm::PAGE_SIZE,
                iova_hint: 0x4000_0000,
                rights: Rights::READ,
            },
        );
        assert_eq!(first, KernelCallResult::Status(Errno::Ok.status()));
        assert_eq!(engine.devices.device_vmar_mappings.len(), 1);
        assert_eq!(engine.devices.device_vmar_mappings[0].iova, 0x4000_0000);
        assert_eq!(
            engine.devices.device_vmar_mappings[0].len,
            crate::mm::PAGE_SIZE
        );

        let overlap = engine.dispatch(
            &mut process,
            KernelCall::DeviceVmarMap {
                ctx,
                vmo,
                vmo_offset: crate::mm::PAGE_SIZE,
                len: crate::mm::PAGE_SIZE,
                iova_hint: 0x4000_0000,
                rights: Rights::READ,
            },
        );
        assert_eq!(
            overlap,
            KernelCallResult::Status(Errno::InvalidArgs.status())
        );
        assert_eq!(engine.devices.device_vmar_mappings.len(), 1);
    }

    #[test]
    fn device_vmar_map_requires_vmo_rights_for_requested_dma() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let ctx = create_test_device_ctx(&mut engine, &mut process);
        let vmo = create_physical_vmo(&mut engine, &mut process, 0x9100_0000, crate::mm::PAGE_SIZE);
        let read_only = process.handles_mut().duplicate(vmo, Rights::READ).unwrap();

        let result = engine.dispatch(
            &mut process,
            KernelCall::DeviceVmarMap {
                ctx,
                vmo: read_only,
                vmo_offset: 0,
                len: crate::mm::PAGE_SIZE,
                iova_hint: 0x5000_0000,
                rights: Rights::WRITE,
            },
        );

        assert_eq!(
            result,
            KernelCallResult::Status(Errno::AccessDenied.status())
        );
        assert!(engine.devices.device_vmar_mappings.is_empty());
    }

    #[test]
    fn device_vmar_map_refuses_backend_rejection_without_recording() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let ctx = create_test_device_ctx_with_kind(
            &mut engine,
            &mut process,
            kumo_abi::sys::IoMmuKind::Virtio as u32,
        );
        let vmo = create_physical_vmo(&mut engine, &mut process, 0x9300_0000, crate::mm::PAGE_SIZE);

        let result = engine.dispatch(
            &mut process,
            KernelCall::DeviceVmarMap {
                ctx,
                vmo,
                vmo_offset: 0,
                len: crate::mm::PAGE_SIZE,
                iova_hint: 0x7000_0000,
                rights: Rights::READ,
            },
        );

        assert_eq!(
            result,
            KernelCallResult::Status(Errno::NotSupported.status())
        );
        assert!(engine.devices.device_vmar_mappings.is_empty());
    }

    #[test]
    fn device_vmar_unmap_requires_exact_record() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let ctx = create_test_device_ctx(&mut engine, &mut process);
        let vmo = create_physical_vmo(
            &mut engine,
            &mut process,
            0x9200_0000,
            crate::mm::PAGE_SIZE * 2,
        );

        let map = engine.dispatch(
            &mut process,
            KernelCall::DeviceVmarMap {
                ctx,
                vmo,
                vmo_offset: 0,
                len: crate::mm::PAGE_SIZE * 2,
                iova_hint: 0x6000_0000,
                rights: Rights::READ | Rights::WRITE,
            },
        );
        assert_eq!(map, KernelCallResult::Status(Errno::Ok.status()));
        assert_eq!(engine.devices.device_vmar_mappings.len(), 1);

        let partial = engine.dispatch(
            &mut process,
            KernelCall::DeviceVmarUnmap {
                ctx,
                iova: 0x6000_0000,
                len: crate::mm::PAGE_SIZE,
            },
        );
        assert_eq!(
            partial,
            KernelCallResult::Status(Errno::InvalidArgs.status())
        );
        assert_eq!(engine.devices.device_vmar_mappings.len(), 1);

        let exact = engine.dispatch(
            &mut process,
            KernelCall::DeviceVmarUnmap {
                ctx,
                iova: 0x6000_0000,
                len: crate::mm::PAGE_SIZE * 2,
            },
        );
        assert_eq!(exact, KernelCallResult::Status(Errno::Ok.status()));
        assert!(engine.devices.device_vmar_mappings.is_empty());

        let repeat = engine.dispatch(
            &mut process,
            KernelCall::DeviceVmarUnmap {
                ctx,
                iova: 0x6000_0000,
                len: crate::mm::PAGE_SIZE * 2,
            },
        );
        assert_eq!(
            repeat,
            KernelCallResult::Status(Errno::InvalidArgs.status())
        );
    }

    #[test]
    fn device_vmar_unmap_refuses_backend_rejection_without_removing_record() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let ctx = create_test_device_ctx(&mut engine, &mut process);
        let vmo = create_physical_vmo(&mut engine, &mut process, 0x9400_0000, crate::mm::PAGE_SIZE);

        let map = engine.dispatch(
            &mut process,
            KernelCall::DeviceVmarMap {
                ctx,
                vmo,
                vmo_offset: 0,
                len: crate::mm::PAGE_SIZE,
                iova_hint: 0x8000_0000,
                rights: Rights::READ,
            },
        );
        assert_eq!(map, KernelCallResult::Status(Errno::Ok.status()));
        assert_eq!(engine.devices.device_vmar_mappings.len(), 1);

        engine.devices.iommus[0].iommu_kind = kumo_abi::sys::IoMmuKind::Virtio as u32;
        let unmap = engine.dispatch(
            &mut process,
            KernelCall::DeviceVmarUnmap {
                ctx,
                iova: 0x8000_0000,
                len: crate::mm::PAGE_SIZE,
            },
        );

        assert_eq!(
            unmap,
            KernelCallResult::Status(Errno::NotSupported.status())
        );
        assert_eq!(engine.devices.device_vmar_mappings.len(), 1);
    }

    #[test]
    fn device_ctx_fault_wait_and_info_drain_fifo_record() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let ctx = create_test_device_ctx(&mut engine, &mut process);
        let ctx_koid = process.handles().get(ctx).unwrap().koid;

        let empty = engine.dispatch(&mut process, KernelCall::DeviceCtxWaitFault { ctx });
        assert_eq!(empty, KernelCallResult::Status(Errno::ShouldWait.status()));

        engine
            .signal_device_ctx_fault(ctx_koid, 0xabc, 0x4000_1000)
            .unwrap();
        let ready = engine.dispatch(&mut process, KernelCall::DeviceCtxWaitFault { ctx });
        assert_eq!(ready, KernelCallResult::Status(Errno::Ok.status()));

        let mut info = DeviceCtxInfo {
            fault_record: 0,
            fault_addr: 0,
        };
        let read = engine.dispatch(
            &mut process,
            KernelCall::DeviceCtxInfo {
                ctx,
                user_ptr: (&mut info as *mut DeviceCtxInfo) as u64,
                user_len: mem::size_of::<DeviceCtxInfo>() as u64,
            },
        );
        assert_eq!(read, KernelCallResult::Status(Errno::Ok.status()));
        assert_eq!(
            info,
            DeviceCtxInfo {
                fault_record: 0xabc,
                fault_addr: 0x4000_1000,
            }
        );

        let drained = engine.dispatch(&mut process, KernelCall::DeviceCtxWaitFault { ctx });
        assert_eq!(
            drained,
            KernelCallResult::Status(Errno::ShouldWait.status())
        );
    }

    #[test]
    fn device_ctx_fault_signals_bound_and_late_bound_ports() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let ctx = create_test_device_ctx(&mut engine, &mut process);
        let ctx_koid = process.handles().get(ctx).unwrap().koid;
        let watched = create_port(&mut engine, &mut process);
        let late_bound = create_port(&mut engine, &mut process);

        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::PortBind {
                    port: watched,
                    object: ctx,
                },
            ),
            KernelCallResult::Status(Errno::Ok.status())
        );

        engine
            .signal_device_ctx_fault(ctx_koid, 0xfeed, 0x6000_0000)
            .unwrap();

        let KernelCallResult::PortPacket(packet) =
            engine.dispatch(&mut process, KernelCall::PortWait { port: watched })
        else {
            panic!("expected device fault packet for bound port");
        };
        assert_eq!(packet.source, ctx_koid);
        assert!(packet.signals.contains(kumo_abi::Signals::DEVICE_FAULT));

        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::PortBind {
                    port: late_bound,
                    object: ctx,
                },
            ),
            KernelCallResult::Status(Errno::Ok.status())
        );

        let KernelCallResult::PortPacket(packet) =
            engine.dispatch(&mut process, KernelCall::PortWait { port: late_bound })
        else {
            panic!("expected retained device fault packet for late-bound port");
        };
        assert_eq!(packet.source, ctx_koid);
        assert!(packet.signals.contains(kumo_abi::Signals::DEVICE_FAULT));
    }

    #[test]
    fn iommu_stream_fault_routes_to_matching_device_ctx() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let first = create_test_device_ctx_with_kind_and_stream(
            &mut engine,
            &mut process,
            kumo_abi::sys::IoMmuKind::SmmuV3 as u32,
            0x820,
        );
        let second = create_test_device_ctx_with_kind_and_stream(
            &mut engine,
            &mut process,
            kumo_abi::sys::IoMmuKind::SmmuV3 as u32,
            0x820,
        );
        let second_koid = process.handles().get(second).unwrap().koid;
        let first_iommu = engine.devices.device_ctxs[0].iommu_koid;
        let second_iommu = engine.devices.device_ctxs[1].iommu_koid;
        let watched = create_port(&mut engine, &mut process);

        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::PortBind {
                    port: watched,
                    object: second,
                },
            ),
            KernelCallResult::Status(Errno::Ok.status())
        );

        let routed = engine
            .signal_iommu_stream_fault(second_iommu, 0x820, 0xfeed, 0x6000_0000)
            .unwrap();
        assert_eq!(routed, second_koid);
        assert_eq!(
            engine.dispatch(&mut process, KernelCall::DeviceCtxWaitFault { ctx: first }),
            KernelCallResult::Status(Errno::ShouldWait.status())
        );
        assert_eq!(
            engine.dispatch(&mut process, KernelCall::DeviceCtxWaitFault { ctx: second }),
            KernelCallResult::Status(Errno::Ok.status())
        );

        let KernelCallResult::PortPacket(packet) =
            engine.dispatch(&mut process, KernelCall::PortWait { port: watched })
        else {
            panic!("expected routed device fault packet");
        };
        assert_eq!(packet.source, second_koid);
        assert!(packet.signals.contains(kumo_abi::Signals::DEVICE_FAULT));

        let mut info = DeviceCtxInfo {
            fault_record: 0,
            fault_addr: 0,
        };
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::DeviceCtxInfo {
                    ctx: second,
                    user_ptr: (&mut info as *mut DeviceCtxInfo) as u64,
                    user_len: mem::size_of::<DeviceCtxInfo>() as u64,
                },
            ),
            KernelCallResult::Status(Errno::Ok.status())
        );
        assert_eq!(
            info,
            DeviceCtxInfo {
                fault_record: 0xfeed,
                fault_addr: 0x6000_0000,
            }
        );
        assert_eq!(
            engine.signal_iommu_stream_fault(first_iommu, 0x999, 0, 0),
            Err(Errno::BadHandle)
        );
        assert_eq!(
            engine.dispatch(&mut process, KernelCall::DeviceCtxWaitFault { ctx: first }),
            KernelCallResult::Status(Errno::ShouldWait.status())
        );
    }

    #[test]
    fn iommu_stream_fault_rejects_ambiguous_legacy_bindings() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let ctx = create_test_device_ctx_with_kind_and_stream(
            &mut engine,
            &mut process,
            kumo_abi::sys::IoMmuKind::SmmuV3 as u32,
            0x42,
        );
        let iommu_koid = engine.devices.device_ctxs[0].iommu_koid;
        let object = engine.objects.create(ObjectKind::DeviceCtx);
        let (_, references) = process
            .handles_mut()
            .insert_device_context(object, Rights::MAP)
            .unwrap();
        engine.devices.device_ctxs.push(DeviceCtxBinding {
            koid: object.koid(),
            iommu_koid,
            stream_id: 0x42,
            pgd_phys: 0x9000_0000,
            references,
            state: Rc::new(Cell::new(DeviceCtxState::Bound)),
        });

        assert_eq!(
            engine.signal_iommu_stream_fault(iommu_koid, 0x42, 0xaaa, 0xbbb),
            Err(Errno::InvalidArgs)
        );
        assert_eq!(
            engine.dispatch(&mut process, KernelCall::DeviceCtxWaitFault { ctx }),
            KernelCallResult::Status(Errno::ShouldWait.status())
        );
    }

    #[test]
    fn device_ctx_info_rejects_short_buffer_without_draining_fault() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let ctx = create_test_device_ctx(&mut engine, &mut process);
        let ctx_koid = process.handles().get(ctx).unwrap().koid;
        engine
            .signal_device_ctx_fault(ctx_koid, 0x1234, 0x5000_0000)
            .unwrap();

        let mut info = DeviceCtxInfo {
            fault_record: 0,
            fault_addr: 0,
        };
        let short = engine.dispatch(
            &mut process,
            KernelCall::DeviceCtxInfo {
                ctx,
                user_ptr: (&mut info as *mut DeviceCtxInfo) as u64,
                user_len: (mem::size_of::<DeviceCtxInfo>() - 1) as u64,
            },
        );
        assert_eq!(short, KernelCallResult::Status(Errno::InvalidArgs.status()));

        let ready = engine.dispatch(&mut process, KernelCall::DeviceCtxWaitFault { ctx });
        assert_eq!(ready, KernelCallResult::Status(Errno::Ok.status()));
    }

    #[test]
    fn device_ctx_fault_wait_requires_wait_right() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let ctx = create_test_device_ctx(&mut engine, &mut process);
        let map_only = process.handles_mut().duplicate(ctx, Rights::MAP).unwrap();

        let result = engine.dispatch(
            &mut process,
            KernelCall::DeviceCtxWaitFault { ctx: map_only },
        );
        assert_eq!(
            result,
            KernelCallResult::Status(Errno::AccessDenied.status())
        );
    }

    #[test]
    fn framebuffer_claim_is_limited_to_resource_range() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let resource = engine
            .root_resource_create(&mut process, 0x9000_0000, 0x20_0000, 0, 0)
            .unwrap();

        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::FramebufferClaim {
                    resource,
                    phys_base: 0x9000_0000,
                    len: 0x10_0000,
                },
            ),
            KernelCallResult::Status(Errno::Ok.status())
        );
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::FramebufferClaim {
                    resource,
                    phys_base: 0x8fff_f000,
                    len: 0x1000,
                },
            ),
            KernelCallResult::Status(Errno::AccessDenied.status())
        );
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::FramebufferClaim {
                    resource,
                    phys_base: 0x9000_0000,
                    len: 0,
                },
            ),
            KernelCallResult::Status(Errno::InvalidArgs.status())
        );
    }

    #[test]
    fn interrupt_create_is_limited_to_resource_irq_window() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let root = engine
            .root_resource_create(&mut process, 0x0900_0000, 0x1000, 0, u32::MAX)
            .unwrap();
        // Carve a PL011-shaped device grant: one MMIO page plus exactly IRQ 33.
        let device = match engine.dispatch(
            &mut process,
            KernelCall::ResourceCreateChild {
                parent: root,
                phys_base: 0x0900_0000,
                len: 0x1000,
                irq_base: 33,
                irq_count: 1,
            },
        ) {
            KernelCallResult::Handle(handle) => handle,
            other => panic!("expected child Resource handle, got {other:?}"),
        };

        // The granted IRQ binds.
        let inside = engine.dispatch(
            &mut process,
            KernelCall::InterruptCreate {
                resource: device,
                irq: 33,
            },
        );
        assert!(matches!(inside, KernelCallResult::Handle(_)));

        // A neighbouring IRQ the grant does not cover is denied — no ambient authority.
        let outside = engine.dispatch(
            &mut process,
            KernelCall::InterruptCreate {
                resource: device,
                irq: 34,
            },
        );
        assert_eq!(
            outside,
            KernelCallResult::Status(Errno::AccessDenied.status())
        );
    }

    #[test]
    fn tlmm_gpio_interrupt_create_is_limited_to_resource_irq_window() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let root = engine
            .root_resource_create(&mut process, 0x0894_0000, 0x4000, 0, u32::MAX)
            .unwrap();
        let device = match engine.dispatch(
            &mut process,
            KernelCall::ResourceCreateChild {
                parent: root,
                phys_base: 0x0894_0000,
                len: 0x4000,
                irq_base: kumo_abi::tlmm_gpio_irq_window_base(104),
                irq_count: 1,
            },
        ) {
            KernelCallResult::Handle(handle) => handle,
            other => panic!("expected child Resource handle, got {other:?}"),
        };

        let inside = engine.dispatch(
            &mut process,
            KernelCall::InterruptCreate {
                resource: device,
                irq: kumo_abi::tlmm_gpio_irq(104, 8),
            },
        );
        let irq_handle = match inside {
            KernelCallResult::Handle(handle) => handle,
            other => panic!("expected TLMM GPIO interrupt handle, got {other:?}"),
        };
        engine.signal_interrupt(kumo_abi::tlmm_gpio_irq_window_base(104));
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::InterruptWait {
                    interrupt: irq_handle
                },
            ),
            KernelCallResult::Handle(Handle(1))
        );
        assert_eq!(
            engine.dispatch(&mut process, KernelCall::HandleClose { handle: irq_handle }),
            KernelCallResult::Status(Errno::Ok.status())
        );

        let outside = engine.dispatch(
            &mut process,
            KernelCall::InterruptCreate {
                resource: device,
                irq: kumo_abi::tlmm_gpio_irq(105, 8),
            },
        );
        assert_eq!(
            outside,
            KernelCallResult::Status(Errno::AccessDenied.status())
        );
    }

    #[test]
    fn tlmm_gpio_interrupt_requires_completion_before_redelivery() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let root = engine
            .root_resource_create(&mut process, 0x0894_0000, 0x4000, 0, u32::MAX)
            .unwrap();
        let device = match engine.dispatch(
            &mut process,
            KernelCall::ResourceCreateChild {
                parent: root,
                phys_base: 0x0894_0000,
                len: 0x4000,
                irq_base: kumo_abi::tlmm_gpio_irq_window_base(104),
                irq_count: 1,
            },
        ) {
            KernelCallResult::Handle(handle) => handle,
            other => panic!("expected child Resource handle, got {other:?}"),
        };
        let irq_handle = match engine.dispatch(
            &mut process,
            KernelCall::InterruptCreate {
                resource: device,
                irq: kumo_abi::tlmm_gpio_irq(104, 8),
            },
        ) {
            KernelCallResult::Handle(handle) => handle,
            other => panic!("expected TLMM GPIO interrupt handle, got {other:?}"),
        };
        let irq_key = kumo_abi::tlmm_gpio_irq_window_base(104);

        engine.signal_interrupt(irq_key);
        engine.signal_interrupt(irq_key);
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::InterruptWait {
                    interrupt: irq_handle
                },
            ),
            KernelCallResult::Handle(Handle(1))
        );
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::InterruptWait {
                    interrupt: irq_handle
                },
            ),
            KernelCallResult::Status(Errno::ShouldWait.status())
        );
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::InterruptComplete {
                    interrupt: irq_handle
                },
            ),
            KernelCallResult::Status(Errno::Ok.status())
        );

        engine.signal_interrupt(irq_key);
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::InterruptWait {
                    interrupt: irq_handle
                },
            ),
            KernelCallResult::Handle(Handle(1))
        );
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::InterruptComplete {
                    interrupt: irq_handle
                },
            ),
            KernelCallResult::Status(Errno::Ok.status())
        );
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::InterruptComplete {
                    interrupt: irq_handle
                },
            ),
            KernelCallResult::Status(Errno::Ok.status())
        );
    }

    #[test]
    fn resource_child_irq_window_must_stay_inside_parent() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        // Parent grants only IRQs [32, 40).
        let root = engine
            .root_resource_create(&mut process, 0x0900_0000, 0x1000, 32, 8)
            .unwrap();

        // A child IRQ window reaching past the parent's is rejected.
        let outside = engine.dispatch(
            &mut process,
            KernelCall::ResourceCreateChild {
                parent: root,
                phys_base: 0x0900_0000,
                len: 0x1000,
                irq_base: 39,
                irq_count: 4,
            },
        );
        assert_eq!(
            outside,
            KernelCallResult::Status(Errno::AccessDenied.status())
        );

        // A child IRQ window inside the parent's is granted.
        let inside = engine.dispatch(
            &mut process,
            KernelCall::ResourceCreateChild {
                parent: root,
                phys_base: 0x0900_0000,
                len: 0x1000,
                irq_base: 33,
                irq_count: 1,
            },
        );
        assert!(matches!(inside, KernelCallResult::Handle(_)));
    }

    mod interrupt_lifetime {
        use super::*;

        fn setup() -> (SyscallEngine, Process, Handle, Handle) {
            let mut engine = SyscallEngine::new();
            let mut process = test_process(&mut engine);
            let resource = engine
                .root_resource_create(&mut process, 0x0900_0000, 0x1000, 33, 1)
                .unwrap();
            let KernelCallResult::Handle(irq) = engine.dispatch(
                &mut process,
                KernelCall::InterruptCreate { resource, irq: 33 },
            ) else {
                panic!("create");
            };
            (engine, process, resource, irq)
        }

        #[test]
        fn ordinary_close_preserves_a_duplicate_then_reclaims_before_rebind() {
            let (mut engine, mut process, resource, irq) = setup();
            let koid = process.handles().get(irq).unwrap().koid;
            let duplicate = process.handles_mut().duplicate(irq, Rights::WAIT).unwrap();
            let port = create_port(&mut engine, &mut process);
            engine.dispatch(&mut process, KernelCall::PortBind { port, object: irq });
            assert_eq!(
                engine.dispatch(&mut process, KernelCall::HandleClose { handle: irq }),
                KernelCallResult::Status(Errno::Ok.status())
            );
            assert_eq!(engine.interrupts.len(), 1);
            assert_eq!(engine.devices.port_bindings.len(), 1);
            engine.signal_interrupt(33);
            assert_eq!(
                engine.dispatch(
                    &mut process,
                    KernelCall::InterruptWait {
                        interrupt: duplicate
                    }
                ),
                KernelCallResult::Handle(Handle(1))
            );
            assert_eq!(
                engine.dispatch(&mut process, KernelCall::HandleClose { handle: duplicate }),
                KernelCallResult::Status(Errno::Ok.status())
            );
            assert!(engine.interrupts.is_empty() && engine.devices.port_bindings.is_empty());
            engine.signal_interrupt(33);
            let KernelCallResult::Handle(next) = engine.dispatch(
                &mut process,
                KernelCall::InterruptCreate { resource, irq: 33 },
            ) else {
                panic!("rebind");
            };
            assert_ne!(process.handles().get(next).unwrap().koid, koid);
            assert_eq!(
                engine.dispatch(&mut process, KernelCall::InterruptWait { interrupt: next }),
                KernelCallResult::Status(Errno::ShouldWait.status())
            );
        }

        #[test]
        fn copied_grants_and_drained_entries_keep_the_source_and_watch_alive() {
            let (mut engine, mut source, _, irq) = setup();
            let mut target = test_process(&mut engine);
            let grant = source
                .handles()
                .stage_grant_to(target.handles_mut(), irq, false)
                .unwrap();
            source.handles_mut().commit_grant(grant).unwrap();
            let port = create_port(&mut engine, &mut target);
            engine.dispatch(
                &mut target,
                KernelCall::PortBind {
                    port,
                    object: grant.target(),
                },
            );
            drop(source.handles_mut().drain());
            assert_eq!(
                engine.retire_interrupts_with(|_| panic!("surviving grant ignored")),
                0
            );
            engine.signal_interrupt(33);
            assert_eq!(
                engine.dispatch(
                    &mut target,
                    KernelCall::InterruptWait {
                        interrupt: grant.target()
                    }
                ),
                KernelCallResult::Handle(Handle(1))
            );
            assert_eq!(engine.devices.port_bindings.len(), 1);
            let drained = target.handles_mut().drain();
            assert_eq!(
                engine.retire_interrupts_with(|_| panic!("drained owners ignored")),
                0
            );
            drop(drained);
            let mut masks = Vec::new();
            assert_eq!(
                engine.retire_interrupts_with(|irq| {
                    masks.push(irq);
                    true
                }),
                1
            );
            assert_eq!(masks, [33]);
            assert!(engine.devices.port_bindings.is_empty());
            assert_eq!(engine.retire_interrupts_with(|_| panic!("masked twice")), 0);
        }

        #[test]
        fn second_owner_cannot_reconfigure_an_outstanding_source_or_gpio_alias() {
            let (mut engine, mut process, resource, irq) = setup();
            engine.signal_interrupt(33);
            assert_eq!(
                engine.dispatch(
                    &mut process,
                    KernelCall::InterruptCreate { resource, irq: 33 }
                ),
                KernelCallResult::Status(Errno::BadState.status())
            );
            assert_eq!(engine.interrupts.len(), 1);
            assert!(engine.interrupts[0].outstanding);
            assert_eq!(
                engine.dispatch(&mut process, KernelCall::InterruptWait { interrupt: irq }),
                KernelCallResult::Handle(Handle(1))
            );
            let root = engine
                .root_resource_create(&mut process, 0x0894_0000, 0x4000, 0, u32::MAX)
                .unwrap();
            assert!(matches!(
                engine.dispatch(
                    &mut process,
                    KernelCall::InterruptCreate {
                        resource: root,
                        irq: kumo_abi::tlmm_gpio_irq(104, 8)
                    }
                ),
                KernelCallResult::Handle(_)
            ));
            assert_eq!(
                engine.dispatch(
                    &mut process,
                    KernelCall::InterruptCreate {
                        resource: root,
                        irq: kumo_abi::tlmm_gpio_irq(104, 2)
                    }
                ),
                KernelCallResult::Status(Errno::BadState.status())
            );
        }

        #[test]
        fn failed_mask_reserves_the_source_without_retry_or_delivery() {
            let (mut engine, mut process, resource, irq) = setup();
            process.handles_mut().close(irq).unwrap();
            let mut masks = Vec::new();
            assert_eq!(
                engine.retire_interrupts_with(|irq| {
                    masks.push(irq);
                    false
                }),
                0
            );
            assert_eq!(masks, [33]);
            assert_eq!(engine.interrupts[0].state.get(), IrqState::MaskFailed);
            engine.signal_interrupt(33);
            assert_eq!(engine.interrupts[0].count, 0);
            assert_eq!(
                engine.retire_interrupts_with(|_| panic!("failed mask retried")),
                0
            );
            assert_eq!(
                engine.dispatch(
                    &mut process,
                    KernelCall::InterruptCreate { resource, irq: 33 }
                ),
                KernelCallResult::Status(Errno::BadState.status())
            );
        }

        #[test]
        fn cloned_engines_share_retirement_and_do_not_mask_a_rebound_source() {
            let (mut engine, mut process, resource, irq) = setup();
            let mut snapshot = engine.clone();
            process.handles_mut().close(irq).unwrap();
            assert_eq!(
                engine.retire_interrupts_with(|n| {
                    assert_eq!(n, 33);
                    true
                }),
                1
            );
            assert!(matches!(
                engine.dispatch(
                    &mut process,
                    KernelCall::InterruptCreate { resource, irq: 33 }
                ),
                KernelCallResult::Handle(_)
            ));
            assert_eq!(
                snapshot.retire_interrupts_with(|_| panic!("stale clone masked new owner")),
                1
            );
            assert_eq!(engine.interrupts.len(), 1);
        }

        #[test]
        fn queued_owner_survives_source_drain_and_failed_receiver_installation() {
            let mut engine = SyscallEngine::new();
            let mut source = test_process(&mut engine);
            let mut target = test_process(&mut engine);
            let (left, right) = create_channel(&mut engine, &mut source);
            let grant = source
                .handles()
                .stage_grant_to(target.handles_mut(), right, true)
                .unwrap();
            source.handles_mut().commit_grant(grant).unwrap();
            // Synthetic kernel grant: public InterruptCreate deliberately retains its
            // existing non-TRANSFER rights. Exercise the generic queued owner. — KESTREL
            let object = engine.objects.create(ObjectKind::Interrupt);
            let (irq, references) = source
                .handles_mut()
                .insert_interrupt(object, Rights::WAIT | Rights::TRANSFER)
                .unwrap();
            engine.interrupts.push(IrqBinding {
                references,
                state: Rc::new(Cell::new(IrqState::Bound)),
                irq: 33,
                koid: object.koid(),
                count: 0,
                requires_complete: false,
                outstanding: false,
            });
            assert_eq!(
                engine.dispatch(
                    &mut source,
                    KernelCall::ChannelWrite {
                        channel: left,
                        message: Message::new(1, b"irq", &[irq]).unwrap(),
                    }
                ),
                KernelCallResult::Status(Errno::Ok.status())
            );
            drop(source.handles_mut().drain());
            assert_eq!(
                engine.retire_interrupts_with(|_| panic!("queued owner ignored")),
                0
            );
            target.handles_mut().fail_insert_after(Some(0));
            assert_eq!(
                engine.dispatch(
                    &mut target,
                    KernelCall::ChannelRead {
                        channel: grant.target()
                    }
                ),
                KernelCallResult::Status(Errno::NoMemory.status())
            );
            assert!(engine.interrupts[0].references.is_referenced());
            target.handles_mut().fail_insert_after(None);
            let KernelCallResult::Message(message) = engine.dispatch(
                &mut target,
                KernelCall::ChannelRead {
                    channel: grant.target(),
                },
            ) else {
                panic!("receive");
            };
            let received = message.handles()[0];
            drop(message);
            assert_eq!(target.handles().get(received).unwrap().koid, object.koid());
            assert_eq!(
                engine.dispatch(&mut target, KernelCall::HandleClose { handle: received }),
                KernelCallResult::Status(Errno::Ok.status())
            );
            assert!(engine.interrupts.is_empty());
        }

        #[test]
        fn plain_spi_completion_checks_even_without_a_notification_and_retains_failure() {
            let (mut engine, _process, _, _) = setup();
            let binding = &mut engine.interrupts[0];
            binding.outstanding = true;
            let mut completions = Vec::new();
            assert_eq!(
                binding.complete(|irq| {
                    completions.push(irq);
                    Err(Errno::BadState)
                }),
                Err(Errno::BadState)
            );
            assert!(binding.outstanding);
            assert_eq!(
                binding.complete(|irq| {
                    completions.push(irq);
                    Ok(())
                }),
                Ok(())
            );
            assert!(!binding.outstanding);
            assert_eq!(
                binding.complete(|irq| {
                    completions.push(irq);
                    Err(Errno::BadState)
                }),
                Err(Errno::BadState)
            );
            assert_eq!(completions, [33, 33, 33]);
        }

        #[test]
        fn gpio_completion_keeps_its_no_outstanding_delivery_contract() {
            let (mut engine, _process, _, _) = setup();
            let binding = &mut engine.interrupts[0];
            binding.requires_complete = true;
            binding.outstanding = false;
            assert_eq!(
                binding.complete(|_| panic!("GPIO rearmed without delivery")),
                Ok(())
            );
            binding.outstanding = true;
            assert_eq!(
                binding.complete(|_| Err(Errno::NotSupported)),
                Err(Errno::NotSupported)
            );
            assert!(binding.outstanding);
            assert_eq!(binding.complete(|_| Ok(())), Ok(()));
            assert!(!binding.outstanding);
        }

        #[test]
        fn failed_handle_admission_leaves_no_source_reservation() {
            let mut engine = SyscallEngine::new();
            let mut process = test_process(&mut engine);
            let resource = engine
                .root_resource_create(&mut process, 0x0900_0000, 0x1000, 33, 1)
                .unwrap();
            process.handles_mut().fail_insert_after(Some(0));
            assert_eq!(
                engine.dispatch(
                    &mut process,
                    KernelCall::InterruptCreate { resource, irq: 33 }
                ),
                KernelCallResult::Status(Errno::NoMemory.status())
            );
            assert!(engine.interrupts.is_empty());
            process.handles_mut().fail_insert_after(None);
            assert!(matches!(
                engine.dispatch(
                    &mut process,
                    KernelCall::InterruptCreate { resource, irq: 33 }
                ),
                KernelCallResult::Handle(_)
            ));
        }
    }

    #[test]
    fn teardown_reclaims_interrupt_and_resource_bindings() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let resource = engine
            .root_resource_create(&mut process, 0x0900_0000, 0x1000, 33, 1)
            .unwrap();
        let irq = match engine.dispatch(
            &mut process,
            KernelCall::InterruptCreate { resource, irq: 33 },
        ) {
            KernelCallResult::Handle(handle) => handle,
            other => panic!("expected interrupt handle, got {other:?}"),
        };
        assert_eq!(engine.interrupts.len(), 1);
        assert_eq!(engine.resources.len(), 1);

        // The object koids the dying process's handles carry (as teardown drains them).
        let resource_koid = process.handles().get(resource).unwrap().koid;

        // Drained entries retain ownership until teardown actually drops them.
        let drained = process.handles_mut().drain();
        assert_eq!(
            engine.retire_interrupts_with(|_| panic!("drained owners ignored")),
            0
        );
        drop(drained);
        assert_eq!(engine.retire_interrupts(), 1);
        assert!(engine.release_resource(resource_koid));
        assert_eq!(engine.interrupts.len(), 0);
        assert_eq!(engine.resources.len(), 0);

        // Idempotent: a second reclamation finds nothing left to remove.
        assert_eq!(
            engine.retire_interrupts_with(|_| panic!("retirement repeated")),
            0
        );
        assert!(!engine.release_resource(resource_koid));
    }

    #[test]
    fn bound_interrupt_irqs_reports_each_line_once_within_the_caller_slice() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let resource = engine
            .root_resource_create(&mut process, 0x0900_0000, 0x1000, 252, 3)
            .unwrap();
        // Independent owners reserve distinct sources; duplicates share one binding.
        for irq in [252, 253] {
            match engine.dispatch(&mut process, KernelCall::InterruptCreate { resource, irq }) {
                KernelCallResult::Handle(_) => {}
                other => panic!("expected interrupt handle for {irq}, got {other:?}"),
            }
        }
        assert_eq!(engine.interrupts.len(), 2);

        let mut irqs = [0u32; 8];
        assert_eq!(engine.bound_interrupt_irqs(&mut irqs), 2);
        assert_eq!(&irqs[..2], &[252, 253]);

        // A slice too small truncates rather than overruns.
        let mut one = [0u32; 1];
        assert_eq!(engine.bound_interrupt_irqs(&mut one), 1);
        assert_eq!(one, [252]);

        // Nothing bound, nothing to mask.
        assert_eq!(SyscallEngine::new().bound_interrupt_irqs(&mut irqs), 0);
    }

    #[test]
    fn teardown_reclaims_port_bindings_on_either_endpoint() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let port = create_port(&mut engine, &mut process);
        let (left, right) = create_channel(&mut engine, &mut process);

        // Watch one endpoint via the port.
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::PortBind {
                    port,
                    object: right,
                },
            ),
            KernelCallResult::Status(Errno::Ok.status())
        );
        assert_eq!(engine.devices.port_bindings.len(), 1);

        let port_koid = process.handles().get(port).unwrap().koid;
        let right_koid = process.handles().get(right).unwrap().koid;
        let left_koid = process.handles().get(left).unwrap().koid;

        // Reclaiming by the watched object's koid drops the binding (it can never fire).
        assert!(engine.release_port_bindings(right_koid));
        assert_eq!(engine.devices.port_bindings.len(), 0);
        // A handle the binding never named leaves nothing to remove.
        assert!(!engine.release_port_bindings(left_koid));

        // Re-bind, then prove reclamation also fires when the *port* side dies.
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::PortBind {
                    port,
                    object: right,
                },
            ),
            KernelCallResult::Status(Errno::Ok.status())
        );
        assert_eq!(engine.devices.port_bindings.len(), 1);
        assert!(engine.release_port_bindings(port_koid));
        assert_eq!(engine.devices.port_bindings.len(), 0);
    }

    #[test]
    fn port_unbind_drops_exactly_the_named_watch() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let port = create_port(&mut engine, &mut process);
        let (left, right) = create_channel(&mut engine, &mut process);

        // Two watches share one port: a supervisor replacing one instance must drop only
        // that instance's watch, never a sibling's.
        for object in [right, left] {
            assert_eq!(
                engine.dispatch(&mut process, KernelCall::PortBind { port, object }),
                KernelCallResult::Status(Errno::Ok.status())
            );
        }
        assert_eq!(engine.devices.port_bindings.len(), 2);

        let port_koid = process.handles().get(port).unwrap().koid;
        let left_koid = process.handles().get(left).unwrap().koid;

        // A handle that names no object is rejected before the table is touched (the rights
        // guard runs), so a bad unbind cannot silently drop the wrong watch.
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::PortUnbind {
                    port: Handle(9999),
                    object: right,
                },
            ),
            KernelCallResult::Status(Errno::BadHandle.status())
        );
        assert_eq!(engine.devices.port_bindings.len(), 2);

        // Unbinding (port, right) removes exactly that pair; the (port, left) watch survives.
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::PortUnbind {
                    port,
                    object: right,
                },
            ),
            KernelCallResult::Status(Errno::Ok.status())
        );
        assert_eq!(engine.devices.port_bindings.len(), 1);
        assert_eq!(engine.devices.port_bindings[0], (port_koid, left_koid));

        // Idempotent: dropping the now-absent watch again is still a success and a no-op.
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::PortUnbind {
                    port,
                    object: right,
                },
            ),
            KernelCallResult::Status(Errno::Ok.status())
        );
        assert_eq!(engine.devices.port_bindings.len(), 1);
        assert_eq!(engine.devices.port_bindings[0], (port_koid, left_koid));
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
    fn channel_bind_replays_existing_readability() {
        // A serve port is commonly created after producers are already live. Binding the
        // receiver must preserve a message written in that window; otherwise the consumer parks
        // forever waiting for a transition that already happened.
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let reader_port = create_port(&mut engine, &mut process);
        let (left, right) = create_channel(&mut engine, &mut process);
        let right_koid = process.handles().get(right).unwrap().koid;

        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::ChannelWrite {
                    channel: left,
                    message: Message::new(1, b"early", &[]).unwrap(),
                },
            ),
            KernelCallResult::Status(Errno::Ok.status())
        );
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::PortBind {
                    port: reader_port,
                    object: right,
                },
            ),
            KernelCallResult::Status(Errno::Ok.status())
        );

        let KernelCallResult::PortPacket(packet) =
            engine.dispatch(&mut process, KernelCall::PortWait { port: reader_port })
        else {
            panic!("late bind must replay the channel's readable state");
        };
        assert_eq!(packet.source, right_koid);
        assert!(packet.signals.contains(kumo_abi::Signals::READABLE));
    }

    #[test]
    fn channel_read_rearms_bound_port_while_more_is_queued() {
        // Regression for DEFERRED/004: a burst of writes coalesces into one port packet,
        // but each write is a separate message. Reading one must re-arm the reader's bound
        // port so the next PortWait returns immediately and drains the rest in one quantum —
        // instead of stranding the backlog until an unrelated (keypress) signal.
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let reader_port = create_port(&mut engine, &mut process);
        let (left, right) = create_channel(&mut engine, &mut process);
        let right_koid = process.handles().get(right).unwrap().koid;

        // The reader watches the read end via a Port (drv-fb's console shape).
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::PortBind {
                    port: reader_port,
                    object: right,
                },
            ),
            KernelCallResult::Status(Errno::Ok.status())
        );

        // Two writes → two messages, but the port coalesces them into one packet.
        for body in [b"aaa".as_slice(), b"bbb".as_slice()] {
            assert_eq!(
                engine.dispatch(
                    &mut process,
                    KernelCall::ChannelWrite {
                        channel: left,
                        message: Message::new(1, body, &[]).unwrap(),
                    },
                ),
                KernelCallResult::Status(Errno::Ok.status())
            );
        }

        // First wake: one coalesced packet. Consume the first message.
        let KernelCallResult::PortPacket(first) =
            engine.dispatch(&mut process, KernelCall::PortWait { port: reader_port })
        else {
            panic!("expected the coalesced port packet");
        };
        assert_eq!(first.source, right_koid);
        let KernelCallResult::Message(msg1) =
            engine.dispatch(&mut process, KernelCall::ChannelRead { channel: right })
        else {
            panic!("expected first message");
        };
        assert_eq!(msg1.bytes(), b"aaa");

        // The read re-armed the port because a second message is still queued: PortWait
        // returns another packet WITHOUT any new write — this is what unstalls the backlog.
        let KernelCallResult::PortPacket(second) =
            engine.dispatch(&mut process, KernelCall::PortWait { port: reader_port })
        else {
            panic!("read must re-arm the bound port while more is queued");
        };
        assert_eq!(second.source, right_koid);
        let KernelCallResult::Message(msg2) =
            engine.dispatch(&mut process, KernelCall::ChannelRead { channel: right })
        else {
            panic!("expected second message");
        };
        assert_eq!(msg2.bytes(), b"bbb");

        // Channel now drained: the final read must NOT re-arm (no spurious spin).
        assert_eq!(
            engine.dispatch(&mut process, KernelCall::PortWait { port: reader_port }),
            KernelCallResult::Status(Errno::ShouldWait.status())
        );
    }

    #[test]
    fn signal_interrupt_wakes_port_bound_to_interrupt() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        // A device Resource covering exactly IRQ 33, then an Interrupt bound to it.
        let resource = engine
            .root_resource_create(&mut process, 0x0900_0000, 0x1000, 33, 1)
            .unwrap();
        let irq = match engine.dispatch(
            &mut process,
            KernelCall::InterruptCreate { resource, irq: 33 },
        ) {
            KernelCallResult::Handle(handle) => handle,
            other => panic!("expected interrupt handle, got {other:?}"),
        };
        let irq_koid = process.handles().get(irq).unwrap().koid;

        // Watch the Interrupt via a Port — the wait_many shape console-out needs.
        let watched = create_port(&mut engine, &mut process);
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::PortBind {
                    port: watched,
                    object: irq,
                },
            ),
            KernelCallResult::Status(Errno::Ok.status())
        );
        // An unrelated port, bound to nothing, must stay silent.
        let bystander = create_port(&mut engine, &mut process);

        // The IRQ fires.
        engine.signal_interrupt(33);

        // The watching port wakes with an IRQ packet sourced from the interrupt koid.
        let result = engine.dispatch(&mut process, KernelCall::PortWait { port: watched });
        let KernelCallResult::PortPacket(packet) = result else {
            panic!("expected an IRQ port packet, got {result:?}");
        };
        assert_eq!(packet.source, irq_koid);
        assert!(packet.signals.contains(kumo_abi::Signals::IRQ));

        // The unbound port saw nothing.
        assert_eq!(
            engine.dispatch(&mut process, KernelCall::PortWait { port: bystander }),
            KernelCallResult::Status(Errno::ShouldWait.status())
        );

        // The legacy InterruptWait drain path still sees the same fire (count incremented).
        assert_eq!(
            engine.dispatch(&mut process, KernelCall::InterruptWait { interrupt: irq }),
            KernelCallResult::Handle(Handle(1))
        );
    }

    #[test]
    fn one_shot_timer_signals_its_port_at_deadline_and_can_be_cancelled() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let port = create_port(&mut engine, &mut process);

        let timer =
            match engine.dispatch(&mut process, KernelCall::TimerCreate { deadline_ns: 100 }) {
                KernelCallResult::Handle(handle) => handle,
                other => panic!("expected timer handle, got {other:?}"),
            };
        let timer_entry = process.handles().get(timer).unwrap();
        assert_eq!(timer_entry.kind, ObjectKind::Timer);
        assert!(timer_entry.rights.contains(Rights::WAIT));
        assert!(!timer_entry.rights.contains(Rights::DUPLICATE));
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::PortBind {
                    port,
                    object: timer,
                },
            ),
            KernelCallResult::Status(Errno::Ok.status())
        );

        engine.signal_timers(99);
        assert_eq!(
            engine.dispatch(&mut process, KernelCall::PortWait { port }),
            KernelCallResult::Status(Errno::ShouldWait.status())
        );
        engine.signal_timers(100);
        let KernelCallResult::PortPacket(packet) =
            engine.dispatch(&mut process, KernelCall::PortWait { port })
        else {
            panic!("expected timer packet");
        };
        assert_eq!(packet.source, timer_entry.koid);
        assert!(packet.signals.contains(kumo_abi::Signals::TIMER));
        engine.signal_timers(200);
        assert_eq!(
            engine.dispatch(&mut process, KernelCall::PortWait { port }),
            KernelCallResult::Status(Errno::ShouldWait.status())
        );

        let cancelled =
            match engine.dispatch(&mut process, KernelCall::TimerCreate { deadline_ns: 300 }) {
                KernelCallResult::Handle(handle) => handle,
                other => panic!("expected timer handle, got {other:?}"),
            };
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::PortBind {
                    port,
                    object: cancelled,
                },
            ),
            KernelCallResult::Status(Errno::Ok.status())
        );
        assert_eq!(
            engine.dispatch(&mut process, KernelCall::HandleClose { handle: cancelled }),
            KernelCallResult::Status(Errno::Ok.status())
        );
        engine.signal_timers(300);
        assert_eq!(
            engine.dispatch(&mut process, KernelCall::PortWait { port }),
            KernelCallResult::Status(Errno::ShouldWait.status())
        );

        // Expiry between TimerCreate and PortBind is retained as object state; the
        // subsequent bind queues the signal instead of losing the wakeup.
        let late_bound =
            match engine.dispatch(&mut process, KernelCall::TimerCreate { deadline_ns: 400 }) {
                KernelCallResult::Handle(handle) => handle,
                other => panic!("expected timer handle, got {other:?}"),
            };
        let late_koid = process.handles().get(late_bound).unwrap().koid;
        engine.signal_timers(400);
        assert_eq!(
            engine.dispatch(
                &mut process,
                KernelCall::PortBind {
                    port,
                    object: late_bound,
                },
            ),
            KernelCallResult::Status(Errno::Ok.status())
        );
        let KernelCallResult::PortPacket(packet) =
            engine.dispatch(&mut process, KernelCall::PortWait { port })
        else {
            panic!("expected retained timer packet");
        };
        assert_eq!(packet.source, late_koid);
        assert!(packet.signals.contains(kumo_abi::Signals::TIMER));
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
    #[cfg(feature = "arch_aarch64")]
    mod rockchip_contexts {
        use super::*;
        use kumo_abi::sys::IoMmuKind;
        const BASE: u64 = 0xfdd9_7e00;
        const LEN: u64 = 0x200;
        fn handle(result: KernelCallResult) -> Handle {
            match result {
                KernelCallResult::Handle(h) => h,
                other => panic!("expected handle: {other:?}"),
            }
        }
        fn fixture() -> (SyscallEngine, Process, Handle) {
            let mut engine = SyscallEngine::new();
            engine.boot_info = Some(BootInfo::empty(kumo_abi::ABI_VERSION));
            // The parser/physical readiness seam is separately tested. Host dispatch
            // gets the exact validated descriptor and emulated private table RAM. — KESTREL
            engine.display_unit = Some(kumo_hal::active::DisplayIoMmu {
                banks: [(BASE, 0x100), (BASE + 0x100, 0x100)],
                interrupt: 188,
            });
            let mut process = test_process(&mut engine);
            let resource = engine
                .root_resource_create(&mut process, 0, u64::MAX, 0, u32::MAX)
                .unwrap();
            (engine, process, resource)
        }
        fn unit(engine: &mut SyscallEngine, process: &mut Process, resource: Handle) -> Handle {
            handle(engine.dispatch(
                process,
                KernelCall::IoMmuFromResource {
                    resource,
                    kind: IoMmuKind::RockchipV2 as u32,
                    phys_base: BASE,
                    len: LEN,
                },
            ))
        }
        fn context(engine: &mut SyscallEngine, process: &mut Process, iommu: Handle) -> Handle {
            handle(engine.dispatch(
                process,
                KernelCall::DeviceCtxCreate {
                    iommu,
                    stream_or_rid: 0,
                },
            ))
        }
        fn map(
            engine: &mut SyscallEngine,
            process: &mut Process,
            ctx: Handle,
            vmo: Handle,
            iova: u64,
            len: u64,
            rights: Rights,
        ) -> KernelCallResult {
            engine.dispatch(
                process,
                KernelCall::DeviceVmarMap {
                    ctx,
                    vmo,
                    vmo_offset: 0,
                    len,
                    iova_hint: iova,
                    rights,
                },
            )
        }
        #[test]
        fn activation_checks_map_rights_prepared_state_and_preserves_offline_on_unsupported_host() {
            let (mut e, mut p, res) = fixture();
            let io = unit(&mut e, &mut p, res);
            let ctx = context(&mut e, &mut p, io);
            let wait = p.handles_mut().duplicate(ctx, Rights::WAIT).unwrap();
            assert_eq!(
                e.dispatch(&mut p, KernelCall::DeviceCtxActivate { ctx: wait }),
                KernelCallResult::Status(Errno::AccessDenied.status())
            );
            assert_eq!(
                e.dispatch(&mut p, KernelCall::DeviceCtxActivate { ctx }),
                KernelCallResult::Status(Errno::InvalidArgs.status())
            );
            assert_eq!(
                e.dispatch(&mut p, KernelCall::DeviceCtxActivate { ctx: res }),
                KernelCallResult::Status(Errno::WrongType.status())
            );
            let vmo = create_physical_vmo(&mut e, &mut p, 0x10000000, 4096);
            assert_eq!(
                map(&mut e, &mut p, ctx, vmo, 0x60000000, 4096, Rights::READ),
                KernelCallResult::Status(0)
            );
            assert_eq!(
                e.dispatch(&mut p, KernelCall::DeviceCtxActivate { ctx }),
                KernelCallResult::Status(Errno::NotSupported.status())
            );
            let d = e
                .display_context(p.handles().get(ctx).unwrap().koid)
                .unwrap();
            assert_eq!(d.epoch.get(), 0);
            assert!(d.published.borrow().is_none());
            assert_eq!(
                d.mappings
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .walk(0x60000000, kumo_hal::active::DeviceTableAccess::Read),
                Ok(Some(0x10000000))
            );
        }
        #[test]
        fn failed_published_activation_retains_actual_pins_across_close_clone_and_engine_loss() {
            let (mut e, mut p, res) = fixture();
            let io = unit(&mut e, &mut p, res);
            let ctx = context(&mut e, &mut p, io);
            let vmo = create_physical_vmo(&mut e, &mut p, 0x10000000, 4096);
            let vmo_koid = p.handles().get(vmo).unwrap().koid;
            let weak = Rc::downgrade(e.vmos.iter().find(|v| v.koid == vmo_koid).unwrap());
            assert_eq!(
                map(&mut e, &mut p, ctx, vmo, 0x60000000, 4096, Rights::READ),
                KernelCallResult::Status(0)
            );
            *e.device_owner.0.display_controller.borrow_mut() = Some(
                kumo_hal::active::DisplayController::discover(include_bytes!(
                    "../../rk3588-orangepi-5-plus.dtb"
                ))
                .unwrap(),
            );
            // A discovered model controller can never perform physical activation on the
            // host. Exercise the post-publication failure owner, not scaffold success. — KESTREL
            assert_eq!(
                e.dispatch(&mut p, KernelCall::DeviceCtxActivate { ctx }),
                KernelCallResult::Status(Errno::BadState.status())
            );
            let d = e
                .display_context(p.handles().get(ctx).unwrap().koid)
                .unwrap();
            assert!(d.mappings.borrow().is_none());
            assert_eq!(d.epoch.get(), 1);
            let root = d.published.borrow().as_ref().unwrap().root();
            e.dispatch(&mut p, KernelCall::HandleClose { handle: vmo });
            e.vmos.retain(|v| v.koid != vmo_koid);
            assert_eq!(weak.strong_count(), 1);
            assert!(!e.devices.ram_allowed(0x10000000, 4096, None));
            assert!(!e.table_pages.all_free());
            let mut copy = e.clone();
            assert_eq!(
                copy.dispatch(
                    &mut p,
                    KernelCall::DeviceVmarUnmap {
                        ctx,
                        iova: 0x60000000,
                        len: 4096
                    }
                ),
                KernelCallResult::Status(Errno::BadState.status())
            );
            assert_eq!(
                copy.dispatch(&mut p, KernelCall::DeviceCtxActivate { ctx }),
                KernelCallResult::Status(Errno::BadState.status())
            );
            let mut info = DeviceCtxInfo {
                fault_record: 0,
                fault_addr: 0,
            };
            assert_eq!(
                copy.dispatch(
                    &mut p,
                    KernelCall::DeviceCtxInfo {
                        ctx,
                        user_ptr: (&mut info as *mut DeviceCtxInfo) as u64,
                        user_len: 16
                    }
                ),
                KernelCallResult::Status(0)
            );
            assert_eq!(
                info.fault_record,
                kumo_abi::sys::device_ctx_state::RECORD
                    | kumo_abi::sys::device_ctx_state::QUARANTINED
            );
            assert_eq!(info.fault_addr, 1);
            copy.dispatch(&mut p, KernelCall::HandleClose { handle: ctx });
            assert_eq!(d.published.borrow().as_ref().unwrap().root(), root);
            let owner = e.device_owner.clone();
            drop(copy);
            drop(e);
            assert_eq!(weak.strong_count(), 1);
            assert!(!owner.ram_allowed(0x10000000, 4096, None));
        }
        #[test]
        fn exact_unit_authority_wrong_kinds_unknown_topology_and_duplicate_admission() {
            let (mut e, mut p, res) = fixture();
            for kind in [IoMmuKind::Virtio, IoMmuKind::SmmuV3, IoMmuKind::SmmuV2] {
                assert_eq!(
                    e.dispatch(
                        &mut p,
                        KernelCall::IoMmuFromResource {
                            resource: res,
                            kind: kind as u32,
                            phys_base: BASE,
                            len: LEN
                        }
                    ),
                    KernelCallResult::Status(Errno::NotSupported.status())
                );
            }
            for (base, len) in [
                (BASE + 0x100, 0x100),
                (BASE, LEN - 1),
                (BASE - 4096, LEN + 4096),
            ] {
                assert_eq!(
                    e.dispatch(
                        &mut p,
                        KernelCall::IoMmuFromResource {
                            resource: res,
                            kind: 4,
                            phys_base: base,
                            len
                        }
                    ),
                    KernelCallResult::Status(Errno::NotSupported.status())
                );
            }
            let saved = e.display_unit.take();
            assert_eq!(
                e.dispatch(
                    &mut p,
                    KernelCall::IoMmuFromResource {
                        resource: res,
                        kind: 4,
                        phys_base: BASE,
                        len: LEN
                    }
                ),
                KernelCallResult::Status(Errno::NotSupported.status())
            );
            e.display_unit = saved;
            let ro = p.handles_mut().duplicate(res, Rights::READ).unwrap();
            assert_eq!(
                e.dispatch(
                    &mut p,
                    KernelCall::IoMmuFromResource {
                        resource: ro,
                        kind: 4,
                        phys_base: BASE,
                        len: LEN
                    }
                ),
                KernelCallResult::Status(Errno::AccessDenied.status())
            );
            let _ = unit(&mut e, &mut p, res);
            assert_eq!(
                e.dispatch(
                    &mut p,
                    KernelCall::IoMmuFromResource {
                        resource: res,
                        kind: 4,
                        phys_base: BASE,
                        len: LEN
                    }
                ),
                KernelCallResult::Status(Errno::BadState.status())
            );
        }
        #[test]
        fn a_live_engine_reserves_the_unit_against_a_second_engine() {
            let (mut first, mut process, res) = fixture();
            let owner = first.device_owner.clone();
            let descriptor = first.display_unit;
            let _ = unit(&mut first, &mut process, res);
            let mut second = SyscallEngine::with_device_owner(owner);
            second.display_unit = descriptor;
            let mut other = test_process(&mut second);
            let grant = second
                .root_resource_create(&mut other, 0, u64::MAX, 0, u32::MAX)
                .unwrap();
            assert_eq!(
                second.dispatch(
                    &mut other,
                    KernelCall::IoMmuFromResource {
                        resource: grant,
                        kind: 4,
                        phys_base: BASE,
                        len: LEN
                    }
                ),
                KernelCallResult::Status(Errno::BadState.status())
            );
            drop(process);
            drop(first);
            let _ = unit(&mut second, &mut other, grant);
        }

        #[test]
        fn selector_single_context_rights_and_final_close_recreate() {
            let (mut e, mut p, res) = fixture();
            let io = unit(&mut e, &mut p, res);
            for selector in [1, u32::MAX as u64, 1 << 32] {
                assert_eq!(
                    e.dispatch(
                        &mut p,
                        KernelCall::DeviceCtxCreate {
                            iommu: io,
                            stream_or_rid: selector
                        }
                    ),
                    KernelCallResult::Status(Errno::InvalidArgs.status())
                );
            }
            let ro = p.handles_mut().duplicate(io, Rights::READ).unwrap();
            assert_eq!(
                e.dispatch(
                    &mut p,
                    KernelCall::DeviceCtxCreate {
                        iommu: ro,
                        stream_or_rid: 0
                    }
                ),
                KernelCallResult::Status(Errno::AccessDenied.status())
            );
            let ctx = context(&mut e, &mut p, io);
            assert_eq!(
                e.dispatch(
                    &mut p,
                    KernelCall::DeviceCtxCreate {
                        iommu: io,
                        stream_or_rid: 0
                    }
                ),
                KernelCallResult::Status(Errno::InvalidArgs.status())
            );
            assert_eq!(
                e.dispatch(&mut p, KernelCall::HandleClose { handle: ctx }),
                KernelCallResult::Status(0)
            );
            assert!(e.table_pages.all_free());
            let _ = context(&mut e, &mut p, io);
        }
        #[test]
        fn real_read_only_walk_and_transactional_map_unmap_ranges() {
            let (mut e, mut p, res) = fixture();
            let io = unit(&mut e, &mut p, res);
            let ctx = context(&mut e, &mut p, io);
            let vmo = create_physical_vmo(&mut e, &mut p, 0x1000_0000, 8192);
            for (iova, len, rights) in [
                (0x40000000, 8192, Rights::WRITE),
                (0x40000000, 8192, Rights::READ | Rights::WRITE),
                (1 << 32, 4096, Rights::READ),
            ] {
                assert_ne!(
                    map(&mut e, &mut p, ctx, vmo, iova, len, rights),
                    KernelCallResult::Status(0)
                );
            }
            assert_eq!(
                map(&mut e, &mut p, ctx, vmo, 0x3ff000, 8192, Rights::READ),
                KernelCallResult::Status(0)
            );
            let koid = p.handles().get(ctx).unwrap().koid;
            let d = e.display_context(koid).unwrap();
            {
                let m = d.mappings.borrow();
                let m = m.as_ref().unwrap();
                assert_eq!(
                    m.walk(0x400123, kumo_hal::active::DeviceTableAccess::Read),
                    Ok(Some(0x10001123))
                );
                assert_eq!(
                    m.walk(0x400123, kumo_hal::active::DeviceTableAccess::Write),
                    Err(Errno::AccessDenied)
                );
            }
            assert_eq!(
                map(&mut e, &mut p, ctx, vmo, 0x400000, 8192, Rights::READ),
                KernelCallResult::Status(Errno::InvalidArgs.status())
            );
            assert_eq!(
                e.dispatch(
                    &mut p,
                    KernelCall::DeviceVmarUnmap {
                        ctx,
                        iova: 0x3ff000,
                        len: 4096
                    }
                ),
                KernelCallResult::Status(Errno::InvalidArgs.status())
            );
            assert_eq!(
                e.dispatch(
                    &mut p,
                    KernelCall::DeviceVmarUnmap {
                        ctx,
                        iova: 0x3ff000,
                        len: 8192
                    }
                ),
                KernelCallResult::Status(0)
            );
        }
        #[test]
        fn closed_source_duplicate_context_and_cloned_engine_share_one_owner() {
            let (mut e, mut p, res) = fixture();
            let io = unit(&mut e, &mut p, res);
            let ctx = context(&mut e, &mut p, io);
            let vmo = create_physical_vmo(&mut e, &mut p, 0x1000_0000, 4096);
            let koid = p.handles().get(vmo).unwrap().koid;
            let weak = Rc::downgrade(e.vmos.iter().find(|v| v.koid == koid).unwrap());
            assert_eq!(
                map(&mut e, &mut p, ctx, vmo, 0x40000000, 4096, Rights::READ),
                KernelCallResult::Status(0)
            );
            let dup = p
                .handles_mut()
                .duplicate(ctx, Rights::MAP | Rights::WAIT)
                .unwrap();
            e.dispatch(&mut p, KernelCall::HandleClose { handle: vmo });
            e.vmos.retain(|v| v.koid != koid);
            let mut copy = e.clone();
            e.dispatch(&mut p, KernelCall::HandleClose { handle: ctx });
            assert_eq!(weak.strong_count(), 1);
            assert!(!e.table_pages.all_free());
            assert_eq!(
                copy.dispatch(
                    &mut p,
                    KernelCall::DeviceVmarUnmap {
                        ctx: dup,
                        iova: 0x40000000,
                        len: 4096
                    }
                ),
                KernelCallResult::Status(0)
            );
            assert!(weak.upgrade().is_none());
            e.dispatch(&mut p, KernelCall::HandleClose { handle: dup });
            copy.retire_device_contexts();
            assert!(e.table_pages.all_free());
            assert!(
                e.devices.display_contexts.is_empty() && copy.devices.display_contexts.is_empty()
            );
        }
        #[test]
        fn drained_capabilities_delay_retirement_then_release_still_mapped_pins() {
            let (mut e, mut p, res) = fixture();
            let io = unit(&mut e, &mut p, res);
            let ctx = context(&mut e, &mut p, io);
            let vmo = create_physical_vmo(&mut e, &mut p, 0x1000_0000, 4096);
            let koid = p.handles().get(vmo).unwrap().koid;
            let weak = Rc::downgrade(e.vmos.iter().find(|v| v.koid == koid).unwrap());
            assert_eq!(
                map(&mut e, &mut p, ctx, vmo, 0x40000000, 4096, Rights::READ),
                KernelCallResult::Status(0)
            );
            let drained = p.handles_mut().drain();
            e.vmos.retain(|v| v.koid != koid);
            assert_eq!(e.retire_device_contexts(), DeviceCtxRetirement::default());
            assert_eq!(weak.strong_count(), 1);
            drop(drained);
            assert_eq!(e.retire_device_contexts().released, 1);
            assert!(weak.upgrade().is_none());
            assert!(e.table_pages.all_free());
        }
        #[test]
        fn engine_loss_retains_actual_pins_tables_and_physical_unit_across_restart() {
            let (mut e, mut p, res) = fixture();
            let io = unit(&mut e, &mut p, res);
            let ctx = context(&mut e, &mut p, io);
            let vmo = create_physical_vmo(&mut e, &mut p, 0x1000_0000, 4096);
            let koid = p.handles().get(vmo).unwrap().koid;
            let weak = Rc::downgrade(e.vmos.iter().find(|v| v.koid == koid).unwrap());
            assert_eq!(
                map(&mut e, &mut p, ctx, vmo, 0x40000000, 4096, Rights::READ),
                KernelCallResult::Status(0)
            );
            let owner = e.device_owner.clone();
            e.vmos.retain(|v| v.koid != koid);
            drop(p);
            drop(e);
            assert_eq!(weak.strong_count(), 1);
            assert!(!owner.unit_allowed(BASE, LEN));
            assert!(!owner.ram_allowed(0x10000000, 4096, None));
            assert!(!owner.0.table_pages.all_free());
            let fresh = SyscallEngine::with_device_owner(owner);
            assert!(fresh.iommu_unit_quarantined(BASE, LEN));
        }
        #[test]
        fn fixed_fault_records_survive_clones_and_consumption_does_not_release_quarantine() {
            use kumo_abi::sys::device_ctx_fault as f;
            use kumo_hal::active::{
                DisplayFault, DisplayInterruptBank as B, DisplayInterruptError as E,
            };
            let (mut e, mut p, res) = fixture();
            let io = unit(&mut e, &mut p, res);
            let ctx = context(&mut e, &mut p, io);
            let vmo = create_physical_vmo(&mut e, &mut p, 0x10000000, 4096);
            assert_eq!(
                map(&mut e, &mut p, ctx, vmo, 0x40000000, 4096, Rights::READ),
                KernelCallResult::Status(0)
            );
            let mut copy = e.clone();
            let capacity = e.devices.device_ctx_faults.capacity();
            assert!(e.interrupts.is_empty());
            let fault = DisplayFault {
                epoch: 0,
                error: E::MmuSource,
                line_masked: true,
                banks: Some([
                    B {
                        status: 0x1a,
                        fault_address: 0x60001000,
                        raw: 1,
                        mask: 0,
                        pending: 0,
                    },
                    B {
                        status: 0x18,
                        fault_address: 0x70002000,
                        raw: 2,
                        mask: 3,
                        pending: 2,
                    },
                ]),
            };
            e.signal_display_fault(fault);
            assert_eq!(e.devices.device_ctx_faults.capacity(), capacity);
            assert_eq!(
                copy.dispatch(&mut p, KernelCall::DeviceCtxWaitFault { ctx }),
                KernelCallResult::Status(0)
            );
            let mut info = DeviceCtxInfo {
                fault_record: 0,
                fault_addr: 0,
            };
            let pointer = &mut info as *mut _ as u64;
            let no_wait = p.handles_mut().duplicate(ctx, Rights::MAP).unwrap();
            assert_eq!(
                e.dispatch(
                    &mut p,
                    KernelCall::DeviceCtxInfo {
                        ctx: no_wait,
                        user_ptr: pointer,
                        user_len: 16
                    }
                ),
                KernelCallResult::Status(Errno::AccessDenied.status())
            );
            for (bank, address) in [(0, 0x60001000), (1, 0x70002000)] {
                assert_eq!(
                    e.dispatch(
                        &mut p,
                        KernelCall::DeviceCtxInfo {
                            ctx,
                            user_ptr: pointer,
                            user_len: 16
                        }
                    ),
                    KernelCallResult::Status(0)
                );
                assert_eq!(info.fault_addr, address);
                assert_eq!((info.fault_record >> f::BANK_SHIFT) & 3, bank);
            }
            copy.signal_display_fault(DisplayFault {
                epoch: 0,
                error: E::NotReady,
                banks: None,
                line_masked: false,
            });
            assert_eq!(
                e.dispatch(&mut p, KernelCall::DeviceCtxWaitFault { ctx }),
                KernelCallResult::Status(Errno::ShouldWait.status())
            );
            assert_eq!(
                e.dispatch(
                    &mut p,
                    KernelCall::DeviceVmarUnmap {
                        ctx,
                        iova: 0x40000000,
                        len: 4096
                    }
                ),
                KernelCallResult::Status(Errno::BadState.status())
            );
            e.dispatch(&mut p, KernelCall::HandleClose { handle: ctx });
            e.dispatch(&mut p, KernelCall::HandleClose { handle: no_wait });
            assert!(!e.table_pages.all_free());
            assert_eq!(
                e.devices.device_ctxs[0].state.get(),
                DeviceCtxState::Quarantined
            );
            assert!(e.iommu_unit_quarantined(BASE, LEN));
        }
    }
}
