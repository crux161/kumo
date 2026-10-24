#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]
//j572
//j574
//j575
//j576
//j579

use core::fmt::{self, Write};
use drv_gpu::{
    enable_as0, latched_fault, map_range, mcu_boot, mmu_info, power_l2, probe, quiet_mmu,
    soft_reset, ClockIo, Completion, CompletionIo, Firmware, GlobalInterface, MmioIo, ProbeError,
    ProbeIo, ResetPath, SectionFlags, TableArena, TableMemory, APERTURE_BASE, APERTURE_LEN,
    CSF_SHARED_REGION_START, PAGE_SIZE,
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

/// G3b: after the reset, read the vendored firmware out of the initrd, map its sections into
/// address space 0 with their own flags, boot the MCU, and validate the global interface the
/// firmware publishes. The NOTICE lines are the ones a boot report relies on. — TIMBERDOODLE
fn run_mali(device: &mut Device, initrd: Handle) -> u8 {
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
            run_mali(&mut device, Handle(initrd as u32))
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
