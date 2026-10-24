#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]
//j579
//j580
//j581
//j583
//j584

use core::fmt::{self, Write};
use drv_gpu::{
    csf_doorbell, enable_as, enable_as0, first_stream, flush_page_tables, init_global_interface,
    latched_fault, map_range, mcu_boot, mmu_info, power_l2, probe, program_group, program_queue,
    publish_insert, quiet_mmu, read_cs_control, read_cs_status, read_csg_control, ring_offset,
    shared_region_contains, soft_reset, start_group, ClockIo, Completion, CompletionIo, Firmware,
    FirmwareMemory, GlbIo, GlobalInterface, GroupConfig, MmioIo, PageAttrs, ProbeError, ProbeIo,
    QueueConfig, QueueIface, ResetPath, SectionFlags, TableArena, TableMemory, APERTURE_BASE,
    APERTURE_LEN, CSF_SHARED_REGION_START, CSG_CONTROL_BASE, CSG_STREAM_CONTROL_OFFSET,
    GLB_DOORBELL, GLB_DOORBELL_REQ, GLB_PING_TIMEOUT_NS, PAGE_SIZE,
};
use kumo_abi::initrd::{
    find_entry, INITRD_ENTRY_LEN, INITRD_HEADER_LEN, INITRD_TABLE_MAX, MALI_FIRMWARE_PATH,
};
use kumo_abi::{sys::log_level, DevicePowerStatus, Errno, Handle, Status, VmarFlags};

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
/// The group's own address space (AS1) holds the ring and the first stream's target word.
const AS1_WORK_VA: u64 = 0x1000_0000;
const RING_SIZE: u64 = 0x1000;
const WORK_LEN: u64 = 0x2000;
const GROUP_SLOT: u32 = 0;
const GROUP_AS: u32 = 1;
const GROUP_START_TIMEOUT_NS: u64 = 100_000_000;
const WORK_TIMEOUT_NS: u64 = 1_000_000_000;
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

/// G3b: after the reset, read the vendored firmware out of the initrd, map its sections into
/// address space 0 with their own flags, boot the MCU, and validate the global interface the
/// firmware publishes. The NOTICE lines are the ones a boot report relies on. — TIMBERDOODLE
fn run_mali(device: &mut Device, initrd: Handle, shader_present: u64) -> u8 {
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
                Ok(report) => log(
                    log_level::NOTICE,
                    format_args!(
                        "drv-gpu: global interface initialized, ping acknowledged (req=0x{:08x} ack=0x{:08x})\n",
                        report.req, report.ack
                    ),
                ),
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
            let queue = QueueConfig {
                ring_va: AS1_WORK_VA,
                ring_size: RING_SIZE,
                ring_input_va: MCU_IO_PAGES_VA,
                ring_output_va: MCU_IO_PAGES_VA + PAGE_SIZE,
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

            let stream = first_stream(AS1_WORK_VA + PAGE_SIZE);
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
                publish_insert(
                    &mut glb_io,
                    QueueIface {
                        input_va: cs.input_va,
                        output_va: cs.output_va,
                    },
                    (stream.len() * 8) as u64,
                );
            }
            dsb();
            device.write32(csf_doorbell(GROUP_SLOT + 1), 1);
            log(
                log_level::INFO,
                format_args!(
                    "drv-gpu: first stream submitted ({} bytes, target VA 0x{:016x})\n",
                    stream.len() * 8,
                    AS1_WORK_VA + PAGE_SIZE
                ),
            );

            let target = work.va + PAGE_SIZE;
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
                    {
                        let mut glb_io = SharedGlb::new(&mut *device, input, output, windows);
                        let status = read_cs_status(&mut glb_io, cs.output_va);
                        log(
                            log_level::ERR,
                            format_args!(
                                "drv-gpu: stream status cmd_ptr=0x{:x} blocked={} fault=0x{:08x} fatal=0x{:08x}\n",
                                status.cmd_ptr,
                                status.blocked_reason,
                                status.fault,
                                status.fatal
                            ),
                        );
                    }
                    let _ = soft_reset(device);
                    return 1;
                }
                device.delay_us(100);
            }

            log(
                log_level::INFO,
                format_args!("drv-gpu: GPU resident; firmware keeps the device until reboot\n"),
            );
            park()
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
extern "C" fn main(resource: u64, initrd: u64) -> ! {
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
            run_mali(&mut device, Handle(initrd as u32), identity.shader_present)
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
