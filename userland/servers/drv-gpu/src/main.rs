#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]
//j572
//j574
//j575

use core::fmt::{self, Write};
use drv_gpu::{
    disable_as0, enable_as0, exception_name, install_mapping, mcu_fault, mmu_info, power_l2, probe,
    quiet_mmu, soft_reset, stop_mcu, ClockIo, Completion, CompletionIo, Mapping, McuError, MmioIo,
    ProbeError, ProbeIo, ResetPath, TableWriter, APERTURE_BASE, APERTURE_LEN, PAGE_SIZE,
    PAGE_TABLE_ENTRIES,
};
use kumo_abi::{sys::log_level, DevicePowerStatus, Errno, Handle, Status, VmarFlags};

kumo_rt::entry!(main);

const MMIO_VA: u64 = 0x1100_0000;
/// Four 4 KiB translation tables (L0-L3) plus one scratch page the deliberate mapping points
/// at. RESEARCH/009 §7.2: 48 VA bits with a 4 KiB granule is four levels, so every table has
/// 512 eight-byte entries.
const TABLE_PAGES: u64 = 4;
const DMA_PAGES: u64 = TABLE_PAGES + 1;

/// From G2a on the aperture is mapped writable Device memory: reset, L2 and MMU writes land here.
struct Device {
    resource: Handle,
    interrupt: Option<Handle>,
    port: Option<Handle>,
    timer: Option<Handle>,
}

/// One contiguous DMA allocation holding address space 0's tables and the scratch page a
/// deliberate mapping points at. The CPU sees it Normal-NC through an uncached userspace map;
/// the GPU walks it through the physical addresses returned by the allocation.
struct Dma {
    handle: Handle,
    va: u64,
    phys: u64,
}

impl Dma {
    fn new() -> Option<Self> {
        let len = DMA_PAGES * PAGE_SIZE;
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
        Some(Self { handle, va, phys })
    }

    /// Zero every table and the scratch page with volatile stores: these bytes are
    /// device-visible DMA memory, never compiler-visible slices (the J539/J540 noalias lesson).
    fn zero(&self) {
        for page in 0..DMA_PAGES {
            for index in 0..PAGE_TABLE_ENTRIES {
                // SAFETY: aligned u64 inside the mapped DMA VMO.
                unsafe { self.entry(page, index).write_volatile(0) };
            }
        }
    }

    fn write_entry(&self, level: u32, index: usize, value: u64) {
        // SAFETY: as `zero`; `level` is 0-3, one of the four table pages.
        unsafe { self.entry(u64::from(level), index).write_volatile(value) };
    }

    fn entry(&self, page: u64, index: usize) -> *mut u64 {
        (self.va as usize + page as usize * PAGE_SIZE as usize + index * 8) as *mut u64
    }

    fn table_phys(&self) -> [u64; 4] {
        [
            self.phys,
            self.phys + PAGE_SIZE,
            self.phys + 2 * PAGE_SIZE,
            self.phys + 3 * PAGE_SIZE,
        ]
    }

    fn scratch_phys(&self) -> u64 {
        self.phys + TABLE_PAGES * PAGE_SIZE
    }
}

struct Tables<'a> {
    dma: &'a Dma,
}

impl TableWriter for Tables<'_> {
    fn write_entry(&mut self, level: u32, index: usize, value: u64) {
        self.dma.write_entry(level, index, value);
    }
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

/// G2b: after the reset, power the L2 and run the MMU experiment. Each step logs at its own
/// level; the NOTICE lines are the ones a boot report can rely on.
fn run_mali(device: &mut Device) -> u8 {
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

    let Some(dma) = Dma::new() else {
        log(
            log_level::ERR,
            format_args!("drv-gpu: contiguous DMA allocation failed\n"),
        );
        return 1;
    };
    log(
        log_level::INFO,
        format_args!(
            "drv-gpu: AS0 tables phys=0x{:x} mapped=0x{:x}\n",
            dma.phys, dma.va
        ),
    );
    dma.zero();
    dsb();
    quiet_mmu(device);
    if let Err(error) = enable_as0(device, dma.phys, mmu.va_bits) {
        log(
            log_level::ERR,
            format_args!("drv-gpu: AS0 enable failed: {:?}\n", error),
        );
        return 1;
    }
    log(
        log_level::NOTICE,
        format_args!(
            "drv-gpu: AS0 online, transtab=0x{:x} va={} pa={}\n",
            dma.phys, mmu.va_bits, mmu.pa_bits
        ),
    );

    // Stage 1: the MCU's first fetch goes through empty tables, so address space 0 must fault.
    let first = match mcu_fault(device) {
        Ok(observed) => observed.fault,
        Err(error) => {
            log(
                log_level::ERR,
                format_args!(
                    "drv-gpu: the deliberate fault was not observed: {:?}\n",
                    error
                ),
            );
            let _ = stop_mcu(device);
            let _ = disable_as0(device);
            return 1;
        }
    };
    log(
        log_level::NOTICE,
        format_args!(
            "drv-gpu: deliberate fault at VA=0x{:016x}: {} {:?} (raw=0x{:08x})\n",
            first.address,
            exception_name(first.exception),
            first.access,
            first.raw
        ),
    );

    // Containment: stop the MCU before touching the space; a stuck MCU is recovered by the
    // G2a soft reset rather than left running against a faulted AS.
    match stop_mcu(device) {
        Ok(status) => log(
            log_level::INFO,
            format_args!("drv-gpu: MCU stopped, status={}\n", status),
        ),
        Err(McuError::StopTimeout { status }) => match soft_reset(device) {
            Ok(_) => {
                log(
                    log_level::NOTICE,
                    format_args!(
                        "drv-gpu: MCU stuck in status {}; soft reset recovered the GPU\n",
                        status
                    ),
                );
                let _ = kumo_rt::handle_close(dma.handle);
                return 0;
            }
            Err(error) => {
                log(
                    log_level::ERR,
                    format_args!(
                        "drv-gpu: MCU stuck in status {} and reset failed: {:?}\n",
                        status, error
                    ),
                );
                return 1;
            }
        },
    }
    if let Err(error) = disable_as0(device) {
        log(
            log_level::ERR,
            format_args!("drv-gpu: AS0 disable failed: {:?}\n", error),
        );
        return 1;
    }
    log(
        log_level::INFO,
        format_args!("drv-gpu: AS0 disabled after the fault\n"),
    );

    // Recovery: panthor re-activates a faulted VM by clearing the fault and updating again. If
    // the faulting VA is inside the 48-bit window, point its page at the scratch page first, so
    // a second fault at a different address proves the installed table translated.
    let mappable = first.address < (1u64 << 48);
    if mappable {
        let page = first.address & !(PAGE_SIZE - 1);
        install_mapping(
            &mut Tables { dma: &dma },
            Mapping {
                va: page,
                pa: dma.scratch_phys(),
                cached: false,
                readonly: false,
                noexec: false,
            },
            dma.table_phys(),
        );
        dsb();
        log(
            log_level::INFO,
            format_args!(
                "drv-gpu: mapped 4 KiB at VA=0x{:016x} for the retry\n",
                page
            ),
        );
    }
    if let Err(error) = enable_as0(device, dma.phys, mmu.va_bits) {
        log(
            log_level::ERR,
            format_args!("drv-gpu: AS0 re-enable failed: {:?}\n", error),
        );
        return 1;
    }
    log(
        log_level::NOTICE,
        format_args!("drv-gpu: fault contained and AS0 re-enabled\n"),
    );

    let second = if mappable {
        match mcu_fault(device) {
            Ok(observed) => Some(observed.fault),
            Err(error) => {
                log(
                    log_level::ERR,
                    format_args!("drv-gpu: the retry fault was not observed: {:?}\n", error),
                );
                None
            }
        }
    } else {
        None
    };

    let stopped = stop_mcu(device);
    let disabled = disable_as0(device);
    let _ = kumo_rt::handle_close(dma.handle);
    match (stopped, disabled) {
        (Ok(_), Ok(())) => {}
        (Err(error), _) => {
            log(
                log_level::ERR,
                format_args!(
                    "drv-gpu: the MCU did not stop after the retry: {:?}\n",
                    error
                ),
            );
            return 1;
        }
        (_, Err(error)) => {
            log(
                log_level::ERR,
                format_args!(
                    "drv-gpu: AS0 did not disable after the retry: {:?}\n",
                    error
                ),
            );
            return 1;
        }
    }

    if let Some(second) = second {
        if second.address != first.address {
            log(
                log_level::NOTICE,
                format_args!(
                    "drv-gpu: the mapped page translated; the next fault moved to VA=0x{:016x}\n",
                    second.address
                ),
            );
        } else {
            log(
                log_level::ERR,
                format_args!(
                    "drv-gpu: the second fault was at the same VA (0x{:016x}); the mapping did not take\n",
                    second.address
                ),
            );
        }
    }
    0
}

#[no_mangle]
extern "C" fn main(resource: u64) -> ! {
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
            run_mali(&mut device)
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
