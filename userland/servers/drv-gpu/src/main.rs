#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]
//j591
//j593
//j595
//j644
//j645

mod client;

use core::fmt::{self, Write};
use drv_gpu::{
    ack_cs_irqs, ack_group_events, await_job_irq, check_paint, classify_job_wait, clear_job_irqs,
    cs_irq_req, csf_doorbell, enable_as, enable_as0, first_stream, flush_page_tables,
    init_global_interface, job_irq_csg, job_slot, job_wrapper, kumo_paint_data, kumo_paint_stream,
    latched_fault, latest_flush_id, map_range, mcu_boot, mmu_info, mmu_rawstat,
    paint_reference_checksum, power_l2, probe, program_group, program_queue, publish_insert,
    queue_extract, queue_insert, quiet_mmu, read_cs_control, read_cs_status, read_csg_control,
    read_fault_as, ring_offset, shared_region_contains, soft_reset, start_group, store_stream,
    surface_chunks, wake_stream, write_job_words, ClockIo, Completion, CompletionIo, CsControl,
    CsFeatures, CsgControl, Firmware, FirmwareMemory, GlbIo, GlobalInterface, GroupConfig, JobCall,
    JobWait, MmioIo, PageAttrs, ProbeError, ProbeIo, QueueConfig, QueueIface, ResetPath,
    SectionFlags, TableArena, TableMemory, APERTURE_BASE, APERTURE_LEN, BOOTSTRAP_FIRMWARE,
    BOOTSTRAP_VIEW_CHANNEL, BOOTSTRAP_VIEW_SURFACE, CSF_SHARED_REGION_START, CSG_CONTROL_BASE,
    CSG_STREAM_CONTROL_OFFSET, CS_LINE_BYTES, GLB_DOORBELL, GLB_DOORBELL_REQ, GLB_PING_TIMEOUT_NS,
    GPU_VIEW, JOB_IRQ_GLOBAL_IF, KUMO_PAINT_BINARY_VA, KUMO_PAINT_DATA_VA, KUMO_PAINT_V10,
    KUMO_PAINT_WORKGROUPS, PAGE_SIZE, PAINT_BYTES, PAINT_HEIGHT, PAINT_STREAM_MAX, PAINT_SURFACE,
    PAINT_WIDTH, SURFACE_CHUNK_BYTES, SYNC_STATUS_OFFSET,
};
use kumo_abi::initrd::{
    find_entry, INITRD_ENTRY_LEN, INITRD_HEADER_LEN, INITRD_TABLE_MAX, MALI_FIRMWARE_PATH,
};
use kumo_abi::{sys::log_level, DevicePowerStatus, Errno, Handle, Status, VmarFlags};
use kumo_graphics::{Present, Reply, Status as PresentStatus, REPLY_LEN};

kumo_rt::entry!(main);

const MMIO_VA: u64 = 0x1100_0000;
/// The vendored image is 282624 bytes. The staging buffer lives in BSS, so the ELF loader maps it
/// zero-filled and the initrd does not carry it.
const FIRMWARE_MAX: usize = 320 * 1024;
/// Seven loadable sections need L0, L1, L2 and one L3 per 2 MiB region the image touches: nine
/// tables for the vendored image, with room to spare.
const TABLE_ARENA_LEN: u64 = 16 * PAGE_SIZE;
const MAX_SECTIONS: usize = 8;
/// The RK3588 arch timer runs at 24 MHz (the boot report's GIC/TIMER line and RESEARCH/009 §9.3);
/// the firmware's timers are expressed in 1024-tick units of it.
const GLB_TIMER_HZ: u32 = 24_000_000;
/// The group's structures in the MCU's address space (AS0): the queue's two interface pages and
/// the suspend buffer. The firmware confines host-supplied MCU addresses to its shared region
/// (RESEARCH/009 §7.8), so these live inside 0x0400_0000-0x07FF_FFFF, clear of the section the
/// image loads at the region's start. Metal (J584): an address outside the region truncated to
/// 0x0100_1000 and faulted writing the read-only firmware image. — TIMBERDOODLE
const MCU_IO_PAGES_VA: u64 = 0x0410_0000;
const MCU_SUSPEND_VA: u64 = 0x0410_2000;
const _: () = assert!(shared_region_contains(MCU_IO_PAGES_VA));
const _: () = assert!(shared_region_contains(MCU_SUSPEND_VA));
/// The group's own address space (AS1), one page each: the ring; the target words (the first
/// stream's at +0, wrapped job n's at +64n); the streams the §10.6 wrapper CALLs, one 64-byte
/// line per job; and the queue's sync object. All of it is GPU-uncached and no-execute, as
/// panthor maps its rings and sync objects.
const AS1_WORK_VA: u64 = 0x1000_0000;
const RING_SIZE: u64 = 0x1000;
const AS1_TARGET_VA: u64 = AS1_WORK_VA + PAGE_SIZE;
const AS1_STREAMS_VA: u64 = AS1_WORK_VA + 2 * PAGE_SIZE;
const AS1_SYNC_VA: u64 = AS1_WORK_VA + 3 * PAGE_SIZE;
const WORK_LEN: u64 = 4 * PAGE_SIZE;
/// Two wrapped jobs: the second proves the ring advances past a wrapper and the sync object
/// counts rather than being set.
const WRAPPED_JOBS: u64 = 2;
const WRAPPED_VALUE_BASE: u32 = 0xC0DE_0000;
/// G5c-2: job 3 dispatches kumo_paint. Its stream sits 1 KiB into the streams page, clear of
/// the store jobs' lines; the target is its own 128 KiB in AS1, inside the same 2 MiB table as
/// the work buffer.
const PAINT_JOB: u64 = WRAPPED_JOBS + 1;
const PAINT_STREAM_VA: u64 = AS1_STREAMS_VA + 0x400;
const AS1_PAINT_VA: u64 = AS1_WORK_VA + 0x10_0000;
/// DESIGN/019's clip poison, never a value of the reference pattern.
const PAINT_POISON: u32 = 0x00FF_00FF;
const _: () = assert!(PAINT_STREAM_VA + (PAINT_STREAM_MAX as u64) * 8 <= AS1_SYNC_VA);
const GROUP_SLOT: u32 = 0;
const GROUP_AS: u32 = 1;
const GROUP_START_TIMEOUT_NS: u64 = 100_000_000;
const WORK_TIMEOUT_NS: u64 = 1_000_000_000;
/// After the stream lands, how long the firmware's extract may take to catch up (polled).
const DRAIN_TIMEOUT_NS: u64 = 100_000_000;
const FIRST_WORK_VALUE: u32 = 0x00C0_FFEE;
const TILER_PRESENT_LO: u32 = 0x110;
const TILER_PRESENT_HI: u32 = 0x114;

static mut FIRMWARE_IMAGE: [u8; FIRMWARE_MAX] = [0; FIRMWARE_MAX];

/// From G2a on the aperture is mapped writable Device memory: reset, L2 and MMU writes land here.
struct Device {
    resource: Handle,
    interrupt: Option<Handle>,
    port: Option<Handle>,
    timer: Option<Handle>,
}

/// One contiguous DMA allocation mapped Normal-NC into this process. The GPU sees `phys`; the CPU
/// writes through `va` with volatile stores only (the J539/J540 noalias lesson).
#[derive(Clone, Copy)]
struct Mapping {
    /// Kept open for the life of the process: the handle is what keeps the VMO (and the
    /// firmware's memory) alive; nothing reads it after the mapping is made.
    #[allow(dead_code)]
    handle: Handle,
    va: u64,
    phys: u64,
}

fn map_contiguous(len: u64) -> Option<Mapping> {
    let (raw, phys) = kumo_rt::vmo_create_contiguous(len);
    let handle = valid_handle(raw)?;
    let (status, va) = kumo_rt::vmar_map_anywhere(
        handle,
        len,
        (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
    );
    if status != 0 || va == 0 || va & (PAGE_SIZE - 1) != 0 {
        let _ = kumo_rt::handle_close(handle);
        return None;
    }
    Some(Mapping { handle, va, phys })
}

fn zero_mapping(mapping: &Mapping, len: u64) {
    for offset in 0..len {
        // SAFETY: byte inside the mapped DMA VMO.
        unsafe { ((mapping.va + offset) as *mut u8).write_volatile(0) };
    }
}

fn copy_mapping(mapping: &Mapping, bytes: &[u8]) {
    for (offset, byte) in bytes.iter().enumerate() {
        // SAFETY: byte inside the mapped DMA VMO.
        unsafe { ((mapping.va + offset as u64) as *mut u8).write_volatile(*byte) };
    }
}

/// Page-table access for the library's mapper: every physical address it asks about lies in the
/// arena's contiguous VMO, so a subtraction maps it into this process's uncached window.
struct TableDma {
    arena: Mapping,
}

impl TableMemory for TableDma {
    fn read64(&mut self, phys: u64) -> u64 {
        // SAFETY: as `write64`.
        unsafe { ((self.arena.va + (phys - self.arena.phys)) as *const u64).read_volatile() }
    }

    fn write64(&mut self, phys: u64, value: u64) {
        // SAFETY: the library only passes arena physical addresses, 8-byte aligned inside the VMO.
        unsafe { ((self.arena.va + (phys - self.arena.phys)) as *mut u64).write_volatile(value) };
    }
}

#[derive(Clone, Copy)]
struct Loaded {
    va: u64,
    len: u64,
    mapping: Mapping,
    flags: SectionFlags,
}

/// Handle-returning syscalls report failure as a negative status; 0 is never a valid handle.
fn valid_handle(raw: u64) -> Option<Handle> {
    if raw == 0 || (raw as i64) < 0 {
        None
    } else {
        Some(Handle(raw as u32))
    }
}

// SAFETY: every caller runs only after the aperture's Device mapping succeeded, and only uses
// fixed words inside the 2 MiB grant. — TIMBERDOODLE
fn read_aperture(offset: u32) -> u32 {
    unsafe { ((MMIO_VA + u64::from(offset)) as *const u32).read_volatile() }
}

// SAFETY: as `read_aperture`. The dsb orders each store before the next MMIO access, matching
// the kernel's `PhysPowerIo` convention for device registers. — TIMBERDOODLE
fn write_aperture(offset: u32, value: u32) {
    unsafe { ((MMIO_VA + u64::from(offset)) as *mut u32).write_volatile(value) };
    dsb();
}

#[inline(always)]
fn dsb() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}

impl ProbeIo for Device {
    fn power_up(&mut self) -> Status {
        kumo_rt::device_power_up(self.resource, APERTURE_BASE)
    }

    fn power_status(&mut self) -> Result<DevicePowerStatus, Status> {
        let mut record = DevicePowerStatus::default();
        let status = kumo_rt::device_power_status(self.resource, APERTURE_BASE, &mut record);
        log(
            log_level::INFO,
            format_args!(
                "drv-gpu: power status={} version={} flags=0x{:02x}\n",
                status, record.version, record.flags
            ),
        );
        if status == 0 {
            Ok(record)
        } else {
            Err(status)
        }
    }

    fn map_aperture(&mut self) -> Result<(), ProbeError> {
        let raw = kumo_rt::resource_mint_mmio(self.resource, APERTURE_BASE, APERTURE_LEN);
        let Some(vmo) = valid_handle(raw) else {
            return Err(ProbeError::Mint);
        };
        let status = kumo_rt::vmar_map(
            Handle(0),
            vmo,
            0,
            MMIO_VA,
            APERTURE_LEN,
            (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::DEVICE).0,
        ) as Status;
        let _ = kumo_rt::handle_close(vmo);
        if status == 0 {
            Ok(())
        } else {
            Err(ProbeError::Map(status))
        }
    }

    fn read32(&mut self, offset: u32) -> u32 {
        read_aperture(offset)
    }
}

impl MmioIo for Device {
    fn read32(&mut self, offset: u32) -> u32 {
        read_aperture(offset)
    }

    fn write32(&mut self, offset: u32, value: u32) {
        write_aperture(offset, value);
    }
}

impl ClockIo for Device {
    fn now_ns(&mut self) -> u64 {
        kumo_rt::clock_get()
    }

    fn delay_us(&mut self, us: u64) {
        let deadline = kumo_rt::clock_get().saturating_add(us.saturating_mul(1_000));
        while kumo_rt::clock_get() < deadline {
            core::hint::spin_loop();
        }
    }
}

impl CompletionIo for Device {
    fn arm_completion(&mut self, irq: u32, timeout_ns: u64) -> Result<(), Status> {
        // Bind the port wait BEFORE the caller issues its command: a completion that fired
        // between the command and the bind would mask the SPI with no waiter left to complete
        // it, the same arm-after-assert race drv-xhci lost on metal. — TIMBERDOODLE
        let raw = kumo_rt::interrupt_create(self.resource, irq);
        let Some(interrupt) = valid_handle(raw) else {
            return Err(raw as u32 as i32);
        };
        let Some(port) = valid_handle(kumo_rt::port_create()) else {
            let _ = kumo_rt::handle_close(interrupt);
            return Err(Errno::Internal.status());
        };
        let timer_raw = kumo_rt::timer_create(timeout_ns);
        let Some(timer) = valid_handle(timer_raw) else {
            let _ = kumo_rt::handle_close(port);
            let _ = kumo_rt::handle_close(interrupt);
            return Err(Errno::Internal.status());
        };
        if kumo_rt::port_bind(port, interrupt) != 0 || kumo_rt::port_bind(port, timer) != 0 {
            let _ = kumo_rt::port_unbind(port, interrupt);
            let _ = kumo_rt::port_unbind(port, timer);
            let _ = kumo_rt::handle_close(timer);
            let _ = kumo_rt::handle_close(port);
            let _ = kumo_rt::handle_close(interrupt);
            return Err(Errno::Internal.status());
        }
        self.interrupt = Some(interrupt);
        self.port = Some(port);
        self.timer = Some(timer);
        Ok(())
    }

    fn wait_completion(&mut self) -> Completion {
        let (Some(port), Some(timer), Some(interrupt)) = (self.port, self.timer, self.interrupt)
        else {
            return Completion::TimedOut;
        };
        let timer_koid = kumo_rt::handle_koid(timer);
        let source = kumo_rt::port_wait(port);
        if source == 0 || source == timer_koid {
            return Completion::TimedOut;
        }
        let _ = kumo_rt::interrupt_wait(interrupt);
        Completion::Fired
    }

    fn complete(&mut self) {
        if let Some(port) = self.port.take() {
            if let Some(interrupt) = self.interrupt {
                let _ = kumo_rt::port_unbind(port, interrupt);
            }
            if let Some(timer) = self.timer.take() {
                let _ = kumo_rt::port_unbind(port, timer);
                let _ = kumo_rt::handle_close(timer);
            }
            let _ = kumo_rt::handle_close(port);
        }
        if let Some(interrupt) = self.interrupt.take() {
            let _ = kumo_rt::interrupt_complete(interrupt);
            let _ = kumo_rt::handle_close(interrupt);
        }
    }
}

fn log(level: u8, args: fmt::Arguments<'_>) {
    struct Line {
        bytes: [u8; 256],
        len: usize,
    }
    impl Write for Line {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            let end = self.len + text.len();
            let dest = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
            dest.copy_from_slice(text.as_bytes());
            self.len = end;
            Ok(())
        }
    }
    let mut line = Line {
        bytes: [0; 256],
        len: 0,
    };
    let _ = line.write_fmt(args);
    let _ = kumo_rt::log_write(level, &line.bytes[..line.len]);
}

/// Copy `dest.len()` bytes from the initrd VMO, `VmoRead`'s 256-byte cap at a time.
fn read_initrd(initrd: Handle, offset: u64, dest: &mut [u8]) -> bool {
    let mut copied = 0usize;
    while copied < dest.len() {
        let chunk = (dest.len() - copied).min(256);
        // SAFETY: `copied < dest.len()` and `chunk` stays inside the slice.
        let ptr = unsafe { dest.as_mut_ptr().add(copied) };
        if kumo_rt::vmo_read(initrd, offset + copied as u64, ptr, chunk) != 0 {
            return false;
        }
        copied += chunk;
    }
    true
}

/// Read the initrd header and entry table into `table`, returning the table's byte length.
fn read_initrd_table(initrd: Handle, table: &mut [u8; INITRD_TABLE_MAX]) -> Option<usize> {
    if !read_initrd(initrd, 0, &mut table[..INITRD_HEADER_LEN]) {
        return None;
    }
    let count = u32::from_le_bytes([table[12], table[13], table[14], table[15]]) as usize;
    let table_bytes = INITRD_HEADER_LEN.checked_add(count.checked_mul(INITRD_ENTRY_LEN)?)?;
    if table_bytes > INITRD_TABLE_MAX {
        return None;
    }
    if !read_initrd(
        initrd,
        INITRD_HEADER_LEN as u64,
        &mut table[INITRD_HEADER_LEN..table_bytes],
    ) {
        return None;
    }
    Some(table_bytes)
}

/// One MCU address window this process has mapped: `mcu..mcu+len` corresponds to `cpu..`.
#[derive(Clone, Copy, Default)]
struct McuWindow {
    mcu: u64,
    cpu: u64,
    len: u64,
}

/// The firmware's shared input/output blocks, the global doorbell, and every MCU window G4a/G4b
/// touch. The windows translate firmware VAs into this process's Normal-NC mappings.
struct SharedGlb<'a> {
    device: &'a mut Device,
    input: u64,
    output: u64,
    windows: [McuWindow; 3],
}

impl<'a> SharedGlb<'a> {
    fn new(device: &'a mut Device, input: u64, output: u64, windows: [McuWindow; 3]) -> Self {
        Self {
            device,
            input,
            output,
            windows,
        }
    }

    fn cpu_for(&self, mcu: u64) -> Option<u64> {
        self.windows
            .iter()
            .find(|window| window.len != 0 && mcu >= window.mcu && mcu < window.mcu + window.len)
            .map(|window| window.cpu + (mcu - window.mcu))
    }
}

/// The shared section alone, for reading the CSG/CS control blocks before the other windows
/// exist.
struct SharedMemory {
    mcu: u64,
    cpu: u64,
    len: u64,
}

impl FirmwareMemory for SharedMemory {
    fn read32(&mut self, mcu: u64) -> u32 {
        let Some(offset) = mcu
            .checked_sub(self.mcu)
            .filter(|offset| *offset < self.len)
        else {
            return 0;
        };
        // SAFETY: the offset stays inside the shared section this process maps.
        unsafe { ((self.cpu + offset) as *const u32).read_volatile() }
    }

    fn write32(&mut self, mcu: u64, value: u32) {
        let Some(offset) = mcu
            .checked_sub(self.mcu)
            .filter(|offset| *offset < self.len)
        else {
            return;
        };
        // SAFETY: as `read32`.
        unsafe { ((self.cpu + offset) as *mut u32).write_volatile(value) };
    }
}

impl FirmwareMemory for SharedGlb<'_> {
    fn read32(&mut self, mcu: u64) -> u32 {
        match self.cpu_for(mcu) {
            // SAFETY: windows only cover memory this process mapped.
            Some(cpu) => unsafe { (cpu as *const u32).read_volatile() },
            None => 0,
        }
    }

    fn write32(&mut self, mcu: u64, value: u32) {
        match self.cpu_for(mcu) {
            // SAFETY: as `read32`. The driver only writes firmware-provided VAs it has checked
            // against a window, so the None arm is unreachable.
            Some(cpu) => unsafe { (cpu as *mut u32).write_volatile(value) },
            None => {}
        }
    }
}

impl GlbIo for SharedGlb<'_> {
    fn read_input(&mut self, offset: u32) -> u32 {
        // SAFETY: the input block lies inside the shared section this process maps.
        unsafe { ((self.input + u64::from(offset)) as *const u32).read_volatile() }
    }

    fn write_input(&mut self, offset: u32, value: u32) {
        // SAFETY: as `read_input`.
        unsafe { ((self.input + u64::from(offset)) as *mut u32).write_volatile(value) };
    }

    fn read_output(&mut self, offset: u32) -> u32 {
        // SAFETY: the output block lies inside the same section.
        unsafe { ((self.output + u64::from(offset)) as *const u32).read_volatile() }
    }

    fn ring_doorbell(&mut self) {
        // Order the Normal-NC input stores before the Device doorbell write.
        dsb();
        self.device.write32(GLB_DOORBELL, 1);
    }
}

impl ClockIo for SharedGlb<'_> {
    fn now_ns(&mut self) -> u64 {
        self.device.now_ns()
    }

    fn delay_us(&mut self, us: u64) {
        self.device.delay_us(us);
    }
}

/// Everything a stalled stream leaves behind, at NOTICE so the boot console shows it: the CS
/// status (current instruction, what it waits on, fault and fatal words), both request/ack
/// pairs, the queue's counters, and any AS1 fault. J585-J587 used these to tell a dead doorbell
/// from a misplaced queue page; the wrapper's waits and sync add show up in `wait`/`sync`.
fn report_stream_stall(
    device: &mut Device,
    input: u64,
    output: u64,
    windows: [McuWindow; 3],
    csg: CsgControl,
    cs: CsControl,
) {
    {
        let mut glb_io = SharedGlb::new(&mut *device, input, output, windows);
        let status = read_cs_status(&mut glb_io, cs.output_va);
        log(
            log_level::NOTICE,
            format_args!(
                "drv-gpu: CS status cmd_ptr=0x{:x} blocked={} wait=0x{:08x} sync=0x{:x}@0x{:x} fault=0x{:08x} fatal=0x{:08x}\n",
                status.cmd_ptr,
                status.blocked_reason,
                status.wait,
                status.wait_sync_value,
                status.wait_sync_ptr,
                status.fault,
                status.fatal
            ),
        );
        let cs_req = glb_io.read32(cs.input_va);
        let cs_ack = glb_io.read32(cs.output_va);
        let csg_doorbell_req = glb_io.read32(csg.input_va + 0x08);
        let csg_doorbell_ack = glb_io.read32(csg.output_va + 0x08);
        let cs_irq_ack = glb_io.read32(csg.input_va + 0x0C);
        let cs_irq_req = glb_io.read32(csg.output_va + 0x0C);
        log(
            log_level::NOTICE,
            format_args!(
                "drv-gpu: CS req=0x{:08x} ack=0x{:08x}; CSG doorbell=0x{:x}/0x{:x} irq=0x{:x}/0x{:x}\n",
                cs_req, cs_ack, csg_doorbell_req, csg_doorbell_ack, cs_irq_ack, cs_irq_req
            ),
        );
        let iface = QueueIface::at(MCU_IO_PAGES_VA);
        let insert = queue_insert(&mut glb_io, iface);
        let extract = u64::from(glb_io.read32(iface.input_va + 0x08))
            | (u64::from(glb_io.read32(iface.input_va + 0x0C)) << 32);
        let consumed = queue_extract(&mut glb_io, iface);
        log(
            log_level::NOTICE,
            format_args!(
                "drv-gpu: queue insert=0x{:x} extract=0x{:x} consumed=0x{:x}\n",
                insert, extract, consumed
            ),
        );
    }
    let rawstat = mmu_rawstat(device);
    log(
        log_level::NOTICE,
        format_args!("drv-gpu: MMU rawstat=0x{:08x}\n", rawstat),
    );
    // Zeroed fault registers decode to "OK Atomic"; print them only when AS1's rawstat bit says a
    // fault is actually latched.
    if rawstat & (1u32 << GROUP_AS) != 0 {
        if let Some(fault) = read_fault_as(device, GROUP_AS) {
            log(
                log_level::NOTICE,
                format_args!(
                    "drv-gpu: AS{} fault at VA=0x{:016x}: {} {:?} (raw=0x{:08x})\n",
                    GROUP_AS,
                    fault.address,
                    drv_gpu::exception_name(fault.exception),
                    fault.access,
                    fault.raw
                ),
            );
        }
    }
}

/// G3b: after the reset, read the vendored firmware out of the initrd, map its sections into
/// address space 0 with their own flags, boot the MCU, and validate the global interface the
/// firmware publishes. The NOTICE lines are the ones a boot report relies on. — TIMBERDOODLE
fn run_mali(device: &mut Device, initrd: Handle, bootstrap: Handle, shader_present: u64) -> u8 {
    let mut tag = [0u8; 1];
    let (len, raw) = kumo_rt::channel_read_with_handle(bootstrap, tag.as_mut_ptr(), 1);
    let Some(client_channel) =
        valid_handle(raw).filter(|_| len == 1 && tag[0] == drv_gpu::BOOTSTRAP_CLIENT)
    else {
        log(
            log_level::ERR,
            format_args!("drv-gpu: no client session grant\n"),
        );
        return 1;
    };
    match soft_reset(device) {
        Ok(ResetPath::Interrupt) => log(
            log_level::NOTICE,
            format_args!("drv-gpu: soft reset complete: completion interrupt\n"),
        ),
        Ok(ResetPath::PolledAfterTimeout) => log(
            log_level::NOTICE,
            format_args!("drv-gpu: soft reset complete: rawstat recheck\n"),
        ),
        Err(error) => {
            log(
                log_level::ERR,
                format_args!("drv-gpu: soft reset failed: {:?}\n", error),
            );
            return 1;
        }
    }

    let mmu = mmu_info(device);
    log(
        log_level::INFO,
        format_args!(
            "drv-gpu: MMU features=0x{:08x} va={} pa={} as_present=0x{:02x}\n",
            mmu.features, mmu.va_bits, mmu.pa_bits, mmu.as_present
        ),
    );
    if mmu.va_bits != 48 {
        log(
            log_level::ERR,
            format_args!(
                "drv-gpu: the four-level tables need 48 VA bits, hardware reports {}\n",
                mmu.va_bits
            ),
        );
        return 1;
    }
    if mmu.as_present & 1 == 0 {
        log(
            log_level::ERR,
            format_args!(
                "drv-gpu: no address space 0 (as_present=0x{:02x})\n",
                mmu.as_present
            ),
        );
        return 1;
    }

    match power_l2(device) {
        Ok(l2) => log(
            log_level::NOTICE,
            format_args!(
                "drv-gpu: L2 online, present=0x{:x} ready=0x{:x}, coherency none (features=0x{:08x})\n",
                l2.present, l2.ready, l2.coherency_features
            ),
        ),
        Err(error) => {
            log(
                log_level::ERR,
                format_args!("drv-gpu: L2 power-on failed: {:?}\n", error),
            );
            return 1;
        }
    }

    // The launcher granted this process a read-only initrd: find the vendored image, copy it in
    // 256-byte VmoRead chunks, and validate it before any GPU state depends on it.
    let mut table = [0u8; INITRD_TABLE_MAX];
    let Some(table_bytes) = read_initrd_table(initrd, &mut table) else {
        log(
            log_level::ERR,
            format_args!("drv-gpu: initrd table unreadable\n"),
        );
        return 1;
    };
    let entry = match find_entry(&table[..table_bytes], MALI_FIRMWARE_PATH) {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            log(
                log_level::ERR,
                format_args!("drv-gpu: firmware image missing from the initrd\n"),
            );
            return 1;
        }
        Err(_) => {
            log(
                log_level::ERR,
                format_args!("drv-gpu: initrd entry table invalid\n"),
            );
            return 1;
        }
    };
    if entry.len as usize > FIRMWARE_MAX {
        log(
            log_level::ERR,
            format_args!("drv-gpu: firmware image larger than the staging buffer\n"),
        );
        return 1;
    }
    // SAFETY: one driver process, and the staging buffer is only used during this boot.
    let buffer = unsafe {
        core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(FIRMWARE_IMAGE) as *mut u8,
            FIRMWARE_MAX,
        )
    };
    if !read_initrd(initrd, entry.offset, &mut buffer[..entry.len as usize]) {
        log(
            log_level::ERR,
            format_args!("drv-gpu: firmware image unreadable\n"),
        );
        return 1;
    }
    let firmware = match Firmware::parse(&buffer[..entry.len as usize]) {
        Ok(firmware) => firmware,
        Err(error) => {
            log(
                log_level::ERR,
                format_args!("drv-gpu: firmware parse failed: {:?}\n", error),
            );
            return 1;
        }
    };
    let build = firmware
        .build_sha()
        .and_then(|sha| core::str::from_utf8(sha).ok())
        .unwrap_or("unknown");
    log(
        log_level::INFO,
        format_args!(
            "drv-gpu: firmware image {}b, version hash 0x{:08x}, build {}\n",
            entry.len,
            firmware.version_hash(),
            build
        ),
    );

    // Page tables come from one contiguous arena; every loadable section gets its own contiguous
    // allocation, zero-filled and then initialized from the image.
    let Some(arena_mapping) = map_contiguous(TABLE_ARENA_LEN) else {
        log(
            log_level::ERR,
            format_args!("drv-gpu: table arena allocation failed\n"),
        );
        return 1;
    };
    zero_mapping(&arena_mapping, TABLE_ARENA_LEN);
    let mut table_memory = TableDma {
        arena: arena_mapping,
    };
    let Some(mut arena) = TableArena::new(arena_mapping.phys, TABLE_ARENA_LEN) else {
        log(
            log_level::ERR,
            format_args!("drv-gpu: table arena rejected\n"),
        );
        return 1;
    };
    let Some(transtab) = arena.alloc_table(&mut table_memory) else {
        log(log_level::ERR, format_args!("drv-gpu: no L0 table\n"));
        return 1;
    };

    let mut loaded: [Option<Loaded>; MAX_SECTIONS] = [None; MAX_SECTIONS];
    let mut count = 0usize;
    for section in firmware.loadable() {
        if count == MAX_SECTIONS {
            log(
                log_level::ERR,
                format_args!("drv-gpu: more firmware sections than the loader holds\n"),
            );
            let _ = soft_reset(device);
            return 1;
        }
        let Some(mapping) = map_contiguous(section.len) else {
            log(
                log_level::ERR,
                format_args!(
                    "drv-gpu: section at VA=0x{:016x} allocation failed\n",
                    section.va
                ),
            );
            let _ = soft_reset(device);
            return 1;
        };
        zero_mapping(&mapping, section.len);
        copy_mapping(&mapping, section.data);
        let attrs = section.flags.page_attrs();
        if let Err(error) = map_range(
            &mut table_memory,
            &mut arena,
            transtab,
            section.va,
            mapping.phys,
            section.len,
            attrs,
        ) {
            log(
                log_level::ERR,
                format_args!(
                    "drv-gpu: section at VA=0x{:016x} map failed: {:?}\n",
                    section.va, error
                ),
            );
            let _ = soft_reset(device);
            return 1;
        }
        loaded[count] = Some(Loaded {
            va: section.va,
            len: section.len,
            mapping,
            flags: section.flags,
        });
        count += 1;
    }
    if count == 0 {
        log(
            log_level::ERR,
            format_args!("drv-gpu: no loadable firmware sections\n"),
        );
        let _ = soft_reset(device);
        return 1;
    }
    dsb();
    log(
        log_level::INFO,
        format_args!(
            "drv-gpu: {} sections loaded, transtab=0x{:x} arena=0x{:x}\n",
            count, transtab, arena_mapping.phys
        ),
    );

    quiet_mmu(device);
    if let Err(error) = enable_as0(device, transtab, mmu.va_bits) {
        log(
            log_level::ERR,
            format_args!("drv-gpu: AS0 enable failed: {:?}\n", error),
        );
        return 1;
    }
    log(
        log_level::NOTICE,
        format_args!(
            "drv-gpu: AS0 online, transtab=0x{:x} sections={}\n",
            transtab, count
        ),
    );

    match mcu_boot(device) {
        Ok(observed) => log(
            log_level::INFO,
            format_args!(
                "drv-gpu: MCU boot completed ({:?}), rawstat=0x{:08x}\n",
                observed.completion, observed.rawstat
            ),
        ),
        Err(error) => {
            log(
                log_level::ERR,
                format_args!("drv-gpu: firmware boot failed: {:?}\n", error),
            );
            if let Some(fault) = latched_fault(device) {
                log(
                    log_level::ERR,
                    format_args!(
                        "drv-gpu: AS0 fault at VA=0x{:016x}: {} {:?} (raw=0x{:08x})\n",
                        fault.address,
                        drv_gpu::exception_name(fault.exception),
                        fault.access,
                        fault.raw
                    ),
                );
            }
            let _ = soft_reset(device);
            return 1;
        }
    }

    let shared = loaded[..count]
        .iter()
        .flatten()
        .find(|section| section.va == CSF_SHARED_REGION_START);
    let Some(shared) = shared else {
        log(
            log_level::ERR,
            format_args!("drv-gpu: no shared firmware section was loaded\n"),
        );
        let _ = soft_reset(device);
        return 1;
    };
    let mut control = [0u32; 8];
    for (index, word) in control.iter_mut().enumerate() {
        // SAFETY: the control block sits at the start of the shared section, which this process
        // maps Normal-NC; volatile reads see the MCU's stores.
        *word = unsafe { ((shared.mapping.va + index as u64 * 4) as *const u32).read_volatile() };
    }
    match GlobalInterface::read(control, shared.va, shared.len) {
        Ok(glb) => {
            log(
                log_level::NOTICE,
                format_args!(
                    "drv-gpu: firmware v{}.{}.{} running, features=0x{:08x}\n",
                    glb.major(),
                    glb.minor(),
                    glb.patch(),
                    glb.features
                ),
            );
            log(
                log_level::INFO,
                format_args!(
                    "drv-gpu: global interface groups={} stride=0x{:x} in=0x{:x} out=0x{:x} flags=0x{:08x}\n",
                    glb.group_num,
                    glb.group_stride,
                    glb.input_va,
                    glb.output_va,
                    shared.flags.0
                ),
            );
            let input = shared.mapping.va + (glb.input_va - shared.va);
            let output = shared.mapping.va + (glb.output_va - shared.va);
            let ping = {
                let mut glb_io =
                    SharedGlb::new(&mut *device, input, output, [McuWindow::default(); 3]);
                init_global_interface(
                    &mut glb_io,
                    shader_present,
                    GLB_TIMER_HZ,
                    GLB_PING_TIMEOUT_NS,
                )
            };
            match ping {
                Ok(report) => {
                    log(
                        log_level::NOTICE,
                        format_args!(
                            "drv-gpu: global interface initialized, ping acknowledged (req=0x{:08x} ack=0x{:08x})\n",
                            report.req, report.ack
                        ),
                    );
                    // The config and ping acknowledgements are in GLB_INIT_ACK_IRQ_MASK, so they
                    // latch JOB IRQ bit 31 while the line is masked; the handshake above polled
                    // them, so the bit is stale. J588's job 1 still read it (0x80000001).
                    clear_job_irqs(device, JOB_IRQ_GLOBAL_IF);
                }
                Err(error) => {
                    log(
                        log_level::ERR,
                        format_args!("drv-gpu: global interface init failed: {:?}\n", error),
                    );
                    let _ = soft_reset(device);
                    return 1;
                }
            }
            // G4b: the first command-stream group. Its control blocks live in the shared
            // section; the queue's interface pages and the suspend buffer go into the MCU's
            // address space at VAs the image leaves free; the ring and the target word get the
            // group's own space (AS1).
            let mut shared_memory = SharedMemory {
                mcu: shared.va,
                cpu: shared.mapping.va,
                len: shared.len,
            };
            let csg_mcu = shared.va + u64::from(CSG_CONTROL_BASE);
            let csg = read_csg_control(&mut shared_memory, csg_mcu);
            let cs = read_cs_control(
                &mut shared_memory,
                csg_mcu + u64::from(CSG_STREAM_CONTROL_OFFSET),
            );
            let inside = |va: u64| va >= shared.va && va < shared.va + shared.len;
            if !inside(csg.input_va)
                || !inside(csg.output_va)
                || !inside(cs.input_va)
                || !inside(cs.output_va)
            {
                log(
                    log_level::ERR,
                    format_args!("drv-gpu: CSG/CS blocks outside the shared section\n"),
                );
                let _ = soft_reset(device);
                return 1;
            }
            if !(8..=32).contains(&csg.stream_num) {
                log(
                    log_level::ERR,
                    format_args!(
                        "drv-gpu: unexpected CSG stream count {} (stride 0x{:x})\n",
                        csg.stream_num, csg.stream_stride
                    ),
                );
                let _ = soft_reset(device);
                return 1;
            }
            log(
                log_level::INFO,
                format_args!(
                    "drv-gpu: CSG 0 streams={} stride=0x{:x} suspend={}b in=0x{:x} out=0x{:x}\n",
                    csg.stream_num,
                    csg.stream_stride,
                    csg.suspend_size,
                    csg.input_va,
                    csg.output_va
                ),
            );
            log(
                log_level::INFO,
                format_args!(
                    "drv-gpu: CS 0 in=0x{:x} out=0x{:x} features=0x{:08x}\n",
                    cs.input_va, cs.output_va, cs.features
                ),
            );

            let noexec_uncached = PageAttrs {
                cached: false,
                readonly: false,
                executable: false,
            };
            let Some(io_pages) = map_contiguous(2 * PAGE_SIZE) else {
                log(
                    log_level::ERR,
                    format_args!("drv-gpu: queue interface pages allocation failed\n"),
                );
                let _ = soft_reset(device);
                return 1;
            };
            zero_mapping(&io_pages, 2 * PAGE_SIZE);
            if let Err(error) = map_range(
                &mut table_memory,
                &mut arena,
                transtab,
                MCU_IO_PAGES_VA,
                io_pages.phys,
                2 * PAGE_SIZE,
                noexec_uncached,
            ) {
                log(
                    log_level::ERR,
                    format_args!("drv-gpu: queue interface pages map failed: {:?}\n", error),
                );
                let _ = soft_reset(device);
                return 1;
            }

            let suspend_len = (u64::from(csg.suspend_size) + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
            let mut suspend_va = 0u64;
            let mut suspend_window = McuWindow::default();
            if suspend_len > 0 {
                let Some(mapping) = map_contiguous(suspend_len) else {
                    log(
                        log_level::ERR,
                        format_args!("drv-gpu: suspend buffer allocation failed\n"),
                    );
                    let _ = soft_reset(device);
                    return 1;
                };
                zero_mapping(&mapping, suspend_len);
                if let Err(error) = map_range(
                    &mut table_memory,
                    &mut arena,
                    transtab,
                    MCU_SUSPEND_VA,
                    mapping.phys,
                    suspend_len,
                    noexec_uncached,
                ) {
                    log(
                        log_level::ERR,
                        format_args!("drv-gpu: suspend buffer map failed: {:?}\n", error),
                    );
                    let _ = soft_reset(device);
                    return 1;
                }
                suspend_va = MCU_SUSPEND_VA;
                suspend_window = McuWindow {
                    mcu: MCU_SUSPEND_VA,
                    cpu: mapping.va,
                    len: suspend_len,
                };
            }
            // AS0 gained mappings while it is live; panthor flushes page-table caches after
            // every such change, and so must KUMO. — TIMBERDOODLE
            if let Err(error) = flush_page_tables(device, 0) {
                log(
                    log_level::ERR,
                    format_args!("drv-gpu: AS0 page-table flush failed: {:?}\n", error),
                );
                let _ = soft_reset(device);
                return 1;
            }

            let Some(work) = map_contiguous(WORK_LEN) else {
                log(
                    log_level::ERR,
                    format_args!("drv-gpu: group work buffer allocation failed\n"),
                );
                let _ = soft_reset(device);
                return 1;
            };
            zero_mapping(&work, WORK_LEN);
            let Some(as1_l0) = arena.alloc_table(&mut table_memory) else {
                log(log_level::ERR, format_args!("drv-gpu: no table for AS1\n"));
                let _ = soft_reset(device);
                return 1;
            };
            if let Err(error) = map_range(
                &mut table_memory,
                &mut arena,
                as1_l0,
                AS1_WORK_VA,
                work.phys,
                WORK_LEN,
                noexec_uncached,
            ) {
                log(
                    log_level::ERR,
                    format_args!("drv-gpu: AS1 work map failed: {:?}\n", error),
                );
                let _ = soft_reset(device);
                return 1;
            }
            // G5a's offscreen target: one 256x128 XRGB8888 view, poisoned so a pixel the GPU
            // never wrote shows up as a mismatch.
            let Some(paint) = map_contiguous(PAINT_BYTES) else {
                log(
                    log_level::ERR,
                    format_args!("drv-gpu: paint target allocation failed\n"),
                );
                let _ = soft_reset(device);
                return 1;
            };
            for offset in (0..PAINT_BYTES).step_by(4) {
                // SAFETY: a word inside the paint target's Normal-NC DMA mapping.
                unsafe { ((paint.va + offset) as *mut u32).write_volatile(PAINT_POISON) };
            }
            if let Err(error) = map_range(
                &mut table_memory,
                &mut arena,
                as1_l0,
                AS1_PAINT_VA,
                paint.phys,
                PAINT_BYTES,
                noexec_uncached,
            ) {
                log(
                    log_level::ERR,
                    format_args!("drv-gpu: AS1 paint target map failed: {:?}\n", error),
                );
                let _ = soft_reset(device);
                return 1;
            }
            // Separate GPU permissions: the binary is cached read-only executable; descriptor
            // and push data are uncached read-only no-execute. CPU uploads use Normal-NC, and
            // AS1 is still disabled while these mappings are installed. — TIMBERDOODLE
            let data = match kumo_paint_data(AS1_PAINT_VA) {
                Ok(data) => data,
                Err(error) => {
                    log(
                        log_level::ERR,
                        format_args!("drv-gpu: kernel data refused: {:?}\n", error),
                    );
                    let _ = soft_reset(device);
                    return 1;
                }
            };
            for (gpu_va, bytes, executable) in [
                (KUMO_PAINT_BINARY_VA, KUMO_PAINT_V10.binary, true),
                (KUMO_PAINT_DATA_VA, data.as_slice(), false),
            ] {
                let Some(mapping) = map_contiguous(PAGE_SIZE) else {
                    log(
                        log_level::ERR,
                        format_args!("drv-gpu: kernel allocation failed at 0x{:x}\n", gpu_va),
                    );
                    let _ = soft_reset(device);
                    return 1;
                };
                zero_mapping(&mapping, PAGE_SIZE);
                copy_mapping(&mapping, bytes);
                if let Err(error) = map_range(
                    &mut table_memory,
                    &mut arena,
                    as1_l0,
                    gpu_va,
                    mapping.phys,
                    PAGE_SIZE,
                    PageAttrs {
                        cached: executable,
                        readonly: true,
                        executable,
                    },
                ) {
                    log(
                        log_level::ERR,
                        format_args!(
                            "drv-gpu: kernel map failed at 0x{:x}: {:?}\n",
                            gpu_va, error
                        ),
                    );
                    let _ = soft_reset(device);
                    return 1;
                }
            }
            log(log_level::NOTICE, format_args!(
                "drv-gpu: kumo_paint executable at 0x{:x} ({} bytes), descriptors at 0x{:x}, workgroups={}x{}x{} local={}x{}x{}\n",
                KUMO_PAINT_BINARY_VA, KUMO_PAINT_V10.binary.len(), KUMO_PAINT_DATA_VA,
                KUMO_PAINT_WORKGROUPS[0], KUMO_PAINT_WORKGROUPS[1], KUMO_PAINT_WORKGROUPS[2],
                KUMO_PAINT_V10.local_size[0], KUMO_PAINT_V10.local_size[1], KUMO_PAINT_V10.local_size[2]
            ));
            dsb();
            if let Err(error) = enable_as(device, GROUP_AS, as1_l0, mmu.va_bits) {
                log(
                    log_level::ERR,
                    format_args!("drv-gpu: AS1 enable failed: {:?}\n", error),
                );
                let _ = soft_reset(device);
                return 1;
            }
            log(
                log_level::INFO,
                format_args!(
                    "drv-gpu: AS1 online for the group, transtab=0x{:x}\n",
                    as1_l0
                ),
            );

            let tiler_present = u64::from(read_aperture(TILER_PRESENT_LO))
                | (u64::from(read_aperture(TILER_PRESENT_HI)) << 32);
            let windows = [
                McuWindow {
                    mcu: shared.va,
                    cpu: shared.mapping.va,
                    len: shared.len,
                },
                McuWindow {
                    mcu: MCU_IO_PAGES_VA,
                    cpu: io_pages.va,
                    len: 2 * PAGE_SIZE,
                },
                suspend_window,
            ];
            // One source for the queue interface pages: program_queue's ringbuf fields and
            // publish_insert take the same value, so the host and the firmware cannot disagree
            // about which page carries insert/extract. — TIMBERDOODLE
            let queue_iface = QueueIface::at(MCU_IO_PAGES_VA);
            let queue = QueueConfig {
                ring_va: AS1_WORK_VA,
                ring_size: RING_SIZE,
                ring_input_va: queue_iface.input_va,
                ring_output_va: queue_iface.output_va,
                doorbell: GROUP_SLOT + 1,
                priority: 0,
            };
            let group = GroupConfig {
                compute_mask: shader_present,
                fragment_mask: shader_present,
                tiler_mask: tiler_present,
                max_compute: shader_present.count_ones(),
                max_fragment: shader_present.count_ones(),
                max_tiler: tiler_present.count_ones(),
                priority: 0,
                address_space: GROUP_AS,
                suspend_va,
                protm_suspend_va: 0,
                queue_mask: 1,
            };
            log(
                log_level::NOTICE,
                format_args!(
                    "drv-gpu: group masks compute=0x{:x} tiler=0x{:x} endpoint=0x{:08x} as={} ring=0x{:x} pages=0x{:x} suspend=0x{:x}\n",
                    shader_present,
                    tiler_present,
                    (group.max_compute & 0xFF)
                        | ((group.max_fragment & 0xFF) << 8)
                        | ((group.max_tiler & 0xF) << 16),
                    GROUP_AS,
                    AS1_WORK_VA,
                    MCU_IO_PAGES_VA,
                    suspend_va
                ),
            );
            let start = {
                let mut glb_io = SharedGlb::new(&mut *device, input, output, windows);
                program_queue(&mut glb_io, cs.input_va, queue);
                program_group(&mut glb_io, csg.input_va, csg.output_va, group);
                start_group(
                    &mut glb_io,
                    csg.input_va,
                    csg.output_va,
                    GROUP_SLOT,
                    GROUP_START_TIMEOUT_NS,
                )
            };
            match start {
                Ok(ack) => log(
                    log_level::NOTICE,
                    format_args!(
                        "drv-gpu: group started on CSG {} (ack=0x{:08x})\n",
                        GROUP_SLOT, ack
                    ),
                ),
                Err(error) => {
                    log(
                        log_level::ERR,
                        format_args!("drv-gpu: group start failed: {:?}\n", error),
                    );
                    // Name what the firmware did with the request: a stale doorbell, a CSG state
                    // word that never moved, a stream fault, or an AS0 MMU fault on the queue
                    // pages all point at different causes. — TIMBERDOODLE
                    {
                        let mut glb_io = SharedGlb::new(&mut *device, input, output, windows);
                        let doorbell_ack = glb_io.read_output(GLB_DOORBELL_REQ);
                        log(
                            log_level::INFO,
                            format_args!("drv-gpu: GLB doorbell_ack=0x{:08x}\n", doorbell_ack),
                        );
                        let ack = glb_io.read32(csg.output_va);
                        let doorbell = glb_io.read32(csg.output_va + 0x08);
                        let cs_irq_req = glb_io.read32(csg.output_va + 0x0C);
                        let state = glb_io.read32(csg.output_va + 0x18);
                        log(
                            log_level::INFO,
                            format_args!(
                                "drv-gpu: CSG out ack=0x{:08x} doorbell=0x{:08x} cs_irq_req=0x{:08x} state=0x{:08x}\n",
                                ack, doorbell, cs_irq_req, state
                            ),
                        );
                        let status = read_cs_status(&mut glb_io, cs.output_va);
                        log(
                            log_level::INFO,
                            format_args!(
                                "drv-gpu: CS status cmd_ptr=0x{:x} blocked={} fault=0x{:08x} fatal=0x{:08x}\n",
                                status.cmd_ptr, status.blocked_reason, status.fault, status.fatal
                            ),
                        );
                    }
                    if let Some(fault) = latched_fault(device) {
                        log(
                            log_level::ERR,
                            format_args!(
                                "drv-gpu: AS0 fault at VA=0x{:016x}: {} {:?} (raw=0x{:08x})\n",
                                fault.address,
                                drv_gpu::exception_name(fault.exception),
                                fault.access,
                                fault.raw
                            ),
                        );
                    }
                    let _ = soft_reset(device);
                    return 1;
                }
            }

            {
                let mut glb_io = SharedGlb::new(&mut *device, input, output, windows);
                let cs_ack = glb_io.read32(cs.output_va);
                log(
                    log_level::NOTICE,
                    format_args!("drv-gpu: CS 0 ack=0x{:08x}\n", cs_ack),
                );
            }
            let stream = first_stream(AS1_TARGET_VA);
            let position = ring_offset(0, RING_SIZE);
            for (index, word) in stream.iter().enumerate() {
                // SAFETY: five words inside the ring's first page, which this process maps
                // Normal-NC in AS1's backing buffer.
                unsafe {
                    ((work.va + (position + index * 8) as u64) as *mut u64).write_volatile(*word)
                };
            }
            dsb();
            {
                let mut glb_io = SharedGlb::new(&mut *device, input, output, windows);
                let queue_input_va = queue_iface.input_va;
                publish_insert(&mut glb_io, queue_iface, (stream.len() * 8) as u64);
                let insert = u64::from(glb_io.read32(queue_input_va))
                    | (u64::from(glb_io.read32(queue_input_va + 4)) << 32);
                log(
                    log_level::NOTICE,
                    format_args!("drv-gpu: queue published insert=0x{:x}\n", insert),
                );
            }
            dsb();
            {
                let mut glb_io = SharedGlb::new(&mut *device, input, output, windows);
                wake_stream(&mut glb_io, csg.input_va, csg.output_va, GROUP_SLOT, 0);
            }
            device.write32(csf_doorbell(GROUP_SLOT + 1), 1);
            log(
                log_level::INFO,
                format_args!(
                    "drv-gpu: first stream submitted ({} bytes, target VA 0x{:016x})\n",
                    stream.len() * 8,
                    AS1_TARGET_VA
                ),
            );

            let target = work.va + (AS1_TARGET_VA - AS1_WORK_VA);
            let deadline = device.now_ns().saturating_add(WORK_TIMEOUT_NS);
            loop {
                // SAFETY: the target word is the work buffer's second page, Normal-NC.
                let value = unsafe { (target as *const u32).read_volatile() };
                if value == FIRST_WORK_VALUE {
                    log(
                        log_level::NOTICE,
                        format_args!(
                            "drv-gpu: first GPU work: the command stream wrote 0x{:08x}\n",
                            value
                        ),
                    );
                    break;
                }
                if device.now_ns() >= deadline {
                    log(
                        log_level::ERR,
                        format_args!(
                            "drv-gpu: the first stream did not land within the timeout (target=0x{:08x})\n",
                            value
                        ),
                    );
                    report_stream_stall(&mut *device, input, output, windows, csg, cs);
                    let _ = soft_reset(device);
                    return 1;
                }
                device.delay_us(100);
            }

            {
                // RESEARCH/009 §9.7 and §11.5: once the stream lands the firmware's extract should
                // reach the published insert, and any pending per-stream event is acknowledged by
                // copying cs_irq_req into cs_irq_ack. — TIMBERDOODLE
                let expected_insert = (stream.len() * 8) as u64;
                let iface = QueueIface::at(MCU_IO_PAGES_VA);
                let drain_deadline = device.now_ns() + DRAIN_TIMEOUT_NS;
                let mut glb_io = SharedGlb::new(&mut *device, input, output, windows);
                let extract = loop {
                    let extract = queue_extract(&mut glb_io, iface);
                    if extract >= expected_insert || glb_io.now_ns() >= drain_deadline {
                        break extract;
                    }
                    glb_io.delay_us(100);
                };
                let status = read_cs_status(&mut glb_io, cs.output_va);
                let req = cs_irq_req(&mut glb_io, csg.output_va);
                let ack = glb_io.read32(csg.input_va + 0x0C);
                log(
                    log_level::NOTICE,
                    format_args!(
                        "drv-gpu: queue drained extract=0x{:x} cmd_ptr=0x{:x} blocked={} irq=0x{:x}/0x{:x} pending=0x{:x}\n",
                        extract, status.cmd_ptr, status.blocked_reason, req, ack, req ^ ack
                    ),
                );
                ack_cs_irqs(&mut glb_io, csg.input_va, req);
                let acked = glb_io.read32(csg.input_va + 0x0C);
                log(
                    log_level::NOTICE,
                    format_args!("drv-gpu: stream irqs acknowledged (ack=0x{:x})\n", acked),
                );
            }

            // G4c: two jobs through RESEARCH/009 §10.6's wrapper, the way panthor submits every
            // job. Each wrapper flushes, CALLs its own store stream, waits for every scoreboard
            // and adds 1 to the queue's sync object; a job is done when the seqno reaches its
            // number (§11.4). Metal (J588): SYNC_UPDATE and IDLE are raised per job and JOB IRQ
            // bit 0 follows, so G4d waits on that interrupt. — TIMBERDOODLE
            let features = CsFeatures::decode(cs.features);
            let iface = QueueIface::at(MCU_IO_PAGES_VA);
            let sync = work.va + (AS1_SYNC_VA - AS1_WORK_VA);
            let mut seqno = 0;
            let mut flush_ids = [0u32; PAINT_JOB as usize];
            for job in 1..=PAINT_JOB {
                // Jobs 1 and 2 store one value each; job 3 runs the compute kernel.
                let painting = job == PAINT_JOB;
                let target_va = AS1_TARGET_VA + job * CS_LINE_BYTES;
                let value = WRAPPED_VALUE_BASE | job as u32;
                let mut words = [0u64; PAINT_STREAM_MAX];
                let (stream_va, len) = if painting {
                    let stream = match kumo_paint_stream(features) {
                        Ok(stream) => stream,
                        Err(error) => {
                            log(
                                log_level::ERR,
                                format_args!("drv-gpu: compute stream refused: {:?}\n", error),
                            );
                            let _ = soft_reset(device);
                            return 1;
                        }
                    };
                    words[..stream.len()].copy_from_slice(&stream);
                    (PAINT_STREAM_VA, stream.len())
                } else {
                    let stream = store_stream(target_va, value);
                    words[..stream.len()].copy_from_slice(&stream);
                    (AS1_STREAMS_VA + (job - 1) * CS_LINE_BYTES, stream.len())
                };
                let stream_cpu = work.va + (stream_va - AS1_WORK_VA);
                for (index, word) in words[..len].iter().enumerate() {
                    // SAFETY: the stream lies in the streams page of AS1's Normal-NC backing
                    // buffer: a store job on its own line, the paint stream from 1 KiB (the
                    // const assert keeps it below the sync page).
                    unsafe { ((stream_cpu + index as u64 * 8) as *mut u64).write_volatile(*word) };
                }
                dsb();
                let call = JobCall {
                    latest_flush: latest_flush_id(device),
                    stream_va,
                    stream_len: (len * 8) as u32,
                    sync_va: AS1_SYNC_VA,
                };
                flush_ids[job as usize - 1] = call.latest_flush;
                let wrapper = match job_wrapper(features, call) {
                    Ok(words) => words,
                    Err(error) => {
                        log(
                            log_level::ERR,
                            format_args!(
                                "drv-gpu: job {} wrapper refused: {:?} (CS features=0x{:08x})\n",
                                job, error, cs.features
                            ),
                        );
                        let _ = soft_reset(device);
                        return 1;
                    }
                };
                let (insert, extract) = {
                    let mut glb_io = SharedGlb::new(&mut *device, input, output, windows);
                    (
                        queue_insert(&mut glb_io, iface),
                        queue_extract(&mut glb_io, iface),
                    )
                };
                let Some(slot) = job_slot(insert, extract, RING_SIZE) else {
                    log(
                        log_level::ERR,
                        format_args!(
                            "drv-gpu: job {} has no room in the ring (insert=0x{:x} extract=0x{:x})\n",
                            job, insert, extract
                        ),
                    );
                    let _ = soft_reset(device);
                    return 1;
                };
                // G4d: wait for the job on the JOB interrupt (INTID 124) instead of polling. The
                // first wait is armed before the doorbell rings (inside `await_job_irq`), so a
                // fast job cannot complete into nothing; a wake that leaves the seqno short, such
                // as a late IDLE from the previous job, re-arms for the time left. Each wake runs
                // §11.5's order: JOB_INT is already cleared, then the global events, the stream
                // level, the group events.
                //
                // Before arming, every event still pending is acknowledged and only then are the
                // JOB_INT bits behind them cleared. J589's first metal run armed with the direct
                // stream's group IDLE (and a global IDLE) unacknowledged, and the firmware raised
                // no JOB interrupt for the job's SYNC_UPDATE: it signals a group whose events are
                // all acknowledged, which panthor's handler always leaves behind. — TIMBERDOODLE
                let csg_bit = job_irq_csg(GROUP_SLOT);
                let stale = {
                    let mut glb_io = SharedGlb::new(&mut *device, input, output, windows);
                    ack_group_events(&mut glb_io, csg.input_va, csg.output_va)
                };
                if stale.any() {
                    log(
                        log_level::NOTICE,
                        format_args!(
                            "drv-gpu: job {} acknowledged stale events before arming: csg=0x{:08x} glb=0x{:08x} cs_irq=0x{:x}/0x{:x}\n",
                            job, stale.csg, stale.glb, stale.cs_irq_req, stale.cs_irq_ack
                        ),
                    );
                }
                clear_job_irqs(device, csg_bit | JOB_IRQ_GLOBAL_IF);
                let started = device.now_ns();
                let deadline = started.saturating_add(WORK_TIMEOUT_NS);
                let mut waits = 0u32;
                let mut fired = 0u32;
                let (mut rawstat, mut stat, mut events, mut glb_events) = (0u32, 0u32, 0u32, 0u32);
                let (last, irq_req, irq_ack) = loop {
                    let remaining = deadline.saturating_sub(device.now_ns()).max(1_000);
                    let submit = waits == 0;
                    let irq = match await_job_irq(device, csg_bit, remaining, |device| {
                        if !submit {
                            return;
                        }
                        write_job_words(insert, slot, &wrapper, RING_SIZE, |offset, word| {
                            // SAFETY: `offset` is below RING_SIZE, inside the ring page of AS1's
                            // Normal-NC backing buffer.
                            unsafe { ((work.va + offset as u64) as *mut u64).write_volatile(word) };
                        });
                        dsb();
                        {
                            let mut glb_io = SharedGlb::new(&mut *device, input, output, windows);
                            publish_insert(&mut glb_io, iface, slot.end);
                        }
                        dsb();
                        {
                            let mut glb_io = SharedGlb::new(&mut *device, input, output, windows);
                            wake_stream(&mut glb_io, csg.input_va, csg.output_va, GROUP_SLOT, 0);
                        }
                        device.write32(csf_doorbell(GROUP_SLOT + 1), 1);
                    }) {
                        Ok(irq) => irq,
                        Err(status) => {
                            log(
                                log_level::ERR,
                                format_args!(
                                    "drv-gpu: job {} could not arm the JOB interrupt wait: {}\n",
                                    job, status
                                ),
                            );
                            let _ = soft_reset(device);
                            return 1;
                        }
                    };
                    if submit {
                        log(
                            log_level::INFO,
                            format_args!(
                                "drv-gpu: job {} submitted: wrapper 0x{:x}..0x{:x} calls 0x{:x} ({} bytes), flush id 0x{:x}\n",
                                job, slot.start, slot.end, stream_va, call.stream_len, call.latest_flush
                            ),
                        );
                    }
                    waits += 1;
                    if irq.completion == Completion::Fired && irq.rawstat & csg_bit != 0 {
                        fired += 1;
                    }
                    rawstat |= irq.rawstat;
                    stat |= irq.stat;
                    let acked = {
                        let mut glb_io = SharedGlb::new(&mut *device, input, output, windows);
                        ack_group_events(&mut glb_io, csg.input_va, csg.output_va)
                    };
                    glb_events |= acked.glb;
                    events |= acked.csg;
                    let (irq_req, irq_ack) = (acked.cs_irq_req, acked.cs_irq_ack);
                    // SAFETY: the sync object is 8-byte aligned in AS1's Normal-NC sync page; one
                    // 64-bit load sees the GPU's 64-bit add whole.
                    seqno = unsafe { (sync as *const u64).read_volatile() };
                    if seqno >= job
                        || irq.completion == Completion::TimedOut
                        || device.now_ns() >= deadline
                    {
                        break (irq.completion, irq_req, irq_ack);
                    }
                };
                let elapsed_us = (device.now_ns() - started) / 1_000;
                // SAFETY: the status word of the same sync object.
                let status = unsafe { ((sync + SYNC_STATUS_OFFSET) as *const u32).read_volatile() };
                // SAFETY: job n's target word on its own line of the target page.
                let landed = unsafe {
                    ((work.va + (target_va - AS1_WORK_VA)) as *const u32).read_volatile()
                };
                let (consumed, cs_req, cs_ack) = {
                    let mut glb_io = SharedGlb::new(&mut *device, input, output, windows);
                    (
                        queue_extract(&mut glb_io, iface),
                        glb_io.read32(cs.input_va),
                        glb_io.read32(cs.output_va),
                    )
                };
                let outcome = classify_job_wait(seqno >= job, last, fired);
                match outcome {
                    JobWait::Interrupt => log(
                        log_level::NOTICE,
                        format_args!(
                            "drv-gpu: job {} signalled on the JOB interrupt: seqno={} status=0x{:x} target=0x{:08x} extract=0x{:x} (waits={}, {} us)\n",
                            job, seqno, status, landed, consumed, waits, elapsed_us
                        ),
                    ),
                    JobWait::Stalled => log(
                        log_level::ERR,
                        format_args!(
                            "drv-gpu: job {} did not signal within the timeout (seqno={} status=0x{:x} target=0x{:08x} waits={} fired={})\n",
                            job, seqno, status, landed, waits, fired
                        ),
                    ),
                    JobWait::NoInterrupt => log(
                        log_level::ERR,
                        format_args!(
                            "drv-gpu: job {} reached seqno={} but the JOB interrupt never fired (waits={} target=0x{:08x})\n",
                            job, seqno, waits, landed
                        ),
                    ),
                    JobWait::LateSeqno => log(
                        log_level::ERR,
                        format_args!(
                            "drv-gpu: job {}'s JOB interrupt fired before its seqno landed (seqno={} after {} waits, {} fired)\n",
                            job, seqno, waits, fired
                        ),
                    ),
                }
                log(
                    log_level::NOTICE,
                    format_args!(
                        "drv-gpu: job {} events csg=0x{:08x} glb=0x{:08x} cs_irq=0x{:x}/0x{:x} cs=0x{:08x}/0x{:08x} job_irq=0x{:08x}/0x{:08x} (acknowledged)\n",
                        job, events, glb_events, irq_req, irq_ack, cs_req, cs_ack, rawstat, stat
                    ),
                );
                if outcome != JobWait::Interrupt {
                    report_stream_stall(&mut *device, input, output, windows, csg, cs);
                    let _ = soft_reset(device);
                    return 1;
                }
                if seqno != job || status != 0 || (!painting && landed != value) {
                    log(
                        log_level::ERR,
                        format_args!(
                            "drv-gpu: job {} finished wrong (expected seqno={} status=0x0 target=0x{:08x})\n",
                            job, job, value
                        ),
                    );
                    report_stream_stall(&mut *device, input, output, windows, csg, cs);
                    let _ = soft_reset(device);
                    return 1;
                }
                if painting {
                    // G5c-2: the shader wrote the surface through a GPU-uncached mapping and cleaned
                    // L2/LSC before signalling, and the CPU maps it Normal-NC, so a volatile read
                    // after the interrupt sees the result with no cache maintenance (§13).
                    let check = check_paint(|x, y| {
                        // SAFETY: (x, y) lies inside the 256x128 target; one aligned word.
                        unsafe {
                            ((paint.va + u64::from(y * PAINT_WIDTH + x) * 4) as *const u32)
                                .read_volatile()
                        }
                    });
                    let reference = paint_reference_checksum();
                    log(
                        log_level::NOTICE,
                        format_args!(
                            "drv-gpu: job {} painted {}x{} with kumo_paint compute: mismatches={} checksum=0x{:08x} (reference 0x{:08x})\n",
                            job, PAINT_WIDTH, PAINT_HEIGHT, check.mismatches, check.checksum, reference
                        ),
                    );
                    if let Some((x, y, got, want)) = check.first {
                        log(
                            log_level::ERR,
                            format_args!(
                                "drv-gpu: first wrong pixel ({}, {}): got 0x{:08x}, want 0x{:08x}{}\n",
                                x,
                                y,
                                got,
                                want,
                                if got == PAINT_POISON { " (never written)" } else { "" }
                            ),
                        );
                        report_stream_stall(&mut *device, input, output, windows, csg, cs);
                        let _ = soft_reset(device);
                        return 1;
                    }
                }
            }
            log(
                log_level::NOTICE,
                format_args!(
                    "drv-gpu: {} wrapped jobs complete; queue sync object seqno={}; flush ids 0x{:x} 0x{:x} 0x{:x}\n",
                    PAINT_JOB, seqno, flush_ids[0], flush_ids[1], flush_ids[2]
                ),
            );
            // G5b: only a pixel-exact paint reaches here, so the view never shows a bad image.
            present_paint(bootstrap, paint.va);

            client::serve(
                client_channel,
                device,
                &mut table_memory,
                &mut arena,
                as1_l0,
                work,
                input,
                output,
                windows,
                csg,
                cs,
                shader_present,
                mmu.features,
            )
        }
        Err(error) => {
            log(
                log_level::ERR,
                format_args!("drv-gpu: global interface invalid: {:?}\n", error),
            );
            let _ = soft_reset(device);
            1
        }
    }
}

/// G5b: take view [`GPU_VIEW`] from the bootstrap channel, copy the painted target into its
/// surface and present it through drv-fb, the DESIGN/019 F0 contract `gfx-client` proved (J569).
/// The target is Normal-NC and the paint already completed and cleaned the caches, so volatile
/// reads see the GPU's pixels (§13). The view's handles stay open: the view remains the GPU's
/// for the life of the boot. — TIMBERDOODLE
fn present_paint(bootstrap: Handle, paint_va: u64) {
    let mut tag = [0u8; 1];
    let (len, channel) = kumo_rt::channel_read_with_handle(bootstrap, tag.as_mut_ptr(), 1);
    let Some(channel) =
        valid_handle(channel).filter(|_| len == 1 && tag[0] == BOOTSTRAP_VIEW_CHANNEL)
    else {
        log(
            log_level::NOTICE,
            format_args!("drv-gpu: no view granted; the painted target stays offscreen\n"),
        );
        return;
    };
    let (len, surface) = kumo_rt::channel_read_with_handle(bootstrap, tag.as_mut_ptr(), 1);
    let Some(surface) =
        valid_handle(surface).filter(|_| len == 1 && tag[0] == BOOTSTRAP_VIEW_SURFACE)
    else {
        log(
            log_level::ERR,
            format_args!("drv-gpu: view {} arrived without its surface\n", GPU_VIEW),
        );
        return;
    };
    let _ = kumo_rt::handle_close(bootstrap);
    for (offset, _, _) in surface_chunks() {
        let mut bytes = [0u8; SURFACE_CHUNK_BYTES];
        for (index, pixel) in bytes.chunks_exact_mut(4).enumerate() {
            // SAFETY: a word of the 256x128 target; `surface_chunks` stays inside it.
            let value =
                unsafe { ((paint_va + offset + index as u64 * 4) as *const u32).read_volatile() };
            pixel.copy_from_slice(&value.to_le_bytes());
        }
        if kumo_rt::vmo_write(surface, offset, bytes.as_ptr(), bytes.len()) != 0 {
            log(
                log_level::ERR,
                format_args!(
                    "drv-gpu: view {} surface write failed at 0x{:x}\n",
                    GPU_VIEW, offset
                ),
            );
            return;
        }
    }
    let sequence = 1;
    let frame = Present {
        sequence,
        surface: PAINT_SURFACE,
    }
    .encode();
    if kumo_rt::channel_write(channel, frame.as_ptr(), frame.len()) != 0 {
        log(
            log_level::ERR,
            format_args!("drv-gpu: view {} present request failed\n", GPU_VIEW),
        );
        return;
    }
    let mut reply = [0u8; REPLY_LEN + 1];
    let len = kumo_rt::channel_read(channel, reply.as_mut_ptr(), reply.len()) as usize;
    match reply.get(..len).and_then(Reply::decode) {
        Some(reply) if reply.sequence == sequence && reply.status == PresentStatus::Copied => log(
            log_level::NOTICE,
            format_args!(
                "drv-gpu: view {} on screen: drv-fb copied the GPU-painted {}x{} surface (software GOP)\n",
                GPU_VIEW, PAINT_WIDTH, PAINT_HEIGHT
            ),
        ),
        Some(reply) => log(
            log_level::ERR,
            format_args!(
                "drv-gpu: drv-fb answered sequence {} with {:?}\n",
                reply.sequence, reply.status
            ),
        ),
        None => log(
            log_level::ERR,
            format_args!("drv-gpu: no reply from drv-fb for view {}\n", GPU_VIEW),
        ),
    }
}

/// The firmware runs on memory this process owns: exiting would free the sections under the MCU.
/// Park instead, waking only to sleep again if the empty port ever returns without a signal.
fn park() -> ! {
    if let Some(port) = valid_handle(kumo_rt::port_create()) {
        loop {
            let source = kumo_rt::port_wait(port);
            if source == 0 {
                let _ = kumo_rt::sleep_ns(1_000_000_000);
            }
        }
    }
    loop {
        let _ = kumo_rt::sleep_ns(1_000_000_000);
    }
}

#[no_mangle]
extern "C" fn main(resource: u64, bootstrap: u64) -> ! {
    // G5b: x1 is a bootstrap channel. Its first message carries the initrd handle the firmware
    // is read from (x1 itself until J590); the view arrives later on the same channel.
    let bootstrap = Handle(bootstrap as u32);
    let mut tag = [0u8; 1];
    let (len, initrd) = kumo_rt::channel_read_with_handle(bootstrap, tag.as_mut_ptr(), 1);
    let Some(initrd) = valid_handle(initrd).filter(|_| len == 1 && tag[0] == BOOTSTRAP_FIRMWARE)
    else {
        log(
            log_level::ERR,
            format_args!("drv-gpu: no firmware handle on the bootstrap channel\n"),
        );
        kumo_rt::process_exit(1)
    };
    let mut device = Device {
        resource: Handle(resource as u32),
        interrupt: None,
        port: None,
        timer: None,
    };
    log(
        log_level::INFO,
        format_args!(
            "drv-gpu: power-up requested, aperture=0x{:08x}\n",
            APERTURE_BASE
        ),
    );
    let exit_code = match probe(&mut device) {
        Ok(identity) => {
            log(
                log_level::NOTICE,
                format_args!(
                    "drv-gpu: online, Mali-G610 GPU_ID=0x{:08x}, {} cores\n",
                    identity.gpu_id,
                    identity.cores()
                ),
            );
            log(
                log_level::INFO,
                format_args!(
                    "drv-gpu: SHADER_PRESENT=0x{:016x}; identity probe complete\n",
                    identity.shader_present
                ),
            );
            run_mali(&mut device, initrd, bootstrap, identity.shader_present)
        }
        Err(ProbeError::PowerUp(status)) if status == Errno::NotSupported.status() => {
            log(
                log_level::INFO,
                format_args!("drv-gpu: DevicePowerUp NotSupported; aperture untouched\n"),
            );
            0
        }
        Err(ProbeError::NotReady(power)) => {
            log(
                log_level::ERR,
                format_args!(
                    "drv-gpu: aperture refused: power version={} flags=0x{:02x}\n",
                    power.version, power.flags
                ),
            );
            1
        }
        Err(error) => {
            log(
                log_level::ERR,
                format_args!("drv-gpu: probe failed: {:?}\n", error),
            );
            1
        }
    };
    let _ = kumo_rt::handle_close(device.resource);
    kumo_rt::process_exit(u64::from(exit_code))
}
