#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
//j593
//j594
//j595
//j596
//j655

//! G1b–G5c: power, identity, reset, the first MMU state and the CSF firmware image for the
//! RK3588 Mali-G610. Recovery class: soft-state; all state is rediscovered on launch and
//! DevicePowerUp is idempotent. The driver probes identity, soft-resets the GPU, powers the L2,
//! builds address space 0 in contiguous DMA memory. G3a parses the vendored CSF image; G3b maps
//! its sections into address space 0, boots the MCU, and validates the global interface the
//! firmware publishes when it comes up. G4a initializes that interface and proves a ping
//! round-trip. G4b starts a command-stream group and has the GPU write a value. G4c wraps each
//! job the way panthor does (§10.6) and counts completions in the queue's sync object (§11.4).
//! G4d waits for each job on the JOB interrupt and handles it in §11.5's order. G5a paints a
//! test pattern with the command stream alone and checks it against a CPU reference.
//! G5b presents it in one of drv-fb's views (DESIGN/019).
//! G5c-1 carries the offline-compiled kernel (shaders/mali) and packs its descriptors exactly as
//! Mesa does. G5c-2 maps it executable and dispatches it as the pixel producer.

use kumo_abi::{DevicePowerStatus, Errno, Status};

/// Shared GPU channel contract and bounded session state.
pub use kumo_gpu as protocol;
/// Shared Mali instructions, kernel, descriptors and CPU pixel reference.
pub use kumo_mali::*;

/// DESIGN/020's device grant. The kernel's live DTB gate decides whether this unit exists.
pub const APERTURE_BASE: u64 = 0xfb00_0000;
pub const APERTURE_LEN: u64 = 0x20_0000;
pub const IRQ_BASE: u32 = 124;
pub const IRQ_COUNT: u32 = 3;
/// The GPU block's interrupt (INTID 126 = SPI 94; RESEARCH/009 §4.1/§4.5). The job and MMU
/// blocks are `IRQ_BASE` and `IRQ_BASE + 1`.
pub const GPU_IRQ: u32 = IRQ_BASE + 2;
/// The MMU block's interrupt (INTID 125 = SPI 93); bit 0 of its raw status is an address-space-0
/// page fault (RESEARCH/009 §4.5).
pub const MMU_IRQ: u32 = IRQ_BASE + 1;

// RESEARCH/009 §1.2 and §4.2: GPU_CONTROL identity and feature registers.
const GPU_ID: u32 = 0x000;
const GPU_MMU_FEATURES: u32 = 0x014;
const GPU_AS_PRESENT: u32 = 0x018;
const SHADER_PRESENT_LO: u32 = 0x100;
const SHADER_PRESENT_HI: u32 = 0x104;
const GPU_L2_PRESENT_LO: u32 = 0x120;
const GPU_L2_PRESENT_HI: u32 = 0x124;
const L2_READY_LO: u32 = 0x160;
const L2_READY_HI: u32 = 0x164;
const L2_PWRON_LO: u32 = 0x1A0;
const L2_PWRON_HI: u32 = 0x1A4;
const L2_PWRTRANS_LO: u32 = 0x220;
const L2_PWRTRANS_HI: u32 = 0x224;
const GPU_COHERENCY_FEATURES: u32 = 0x300;
const GPU_COHERENCY_PROTOCOL: u32 = 0x304;
/// RESEARCH/009 §1.2 and §6.4: RK3588 is not `dma-coherent`, so the GPU speaks no coherency
/// protocol. The write must precede L2 power-up.
const GPU_COHERENCY_NONE: u32 = 31;

// RESEARCH/009 §4.2, §4.4 and §4.5: the GPU_CONTROL interrupt and command words.
const GPU_INT_RAWSTAT: u32 = 0x020;
const GPU_INT_CLEAR: u32 = 0x024;
const GPU_INT_MASK: u32 = 0x028;
const GPU_CMD: u32 = 0x030;
const GPU_IRQ_RESET_COMPLETED: u32 = 1 << 8;
const GPU_CMD_SOFT_RESET: u32 = 0x0000_0101;
/// panthor's reset timeout (RESEARCH/009 §4.4).
pub const RESET_TIMEOUT_NS: u64 = 100_000_000;

// RESEARCH/009 §4.7 and §8.6/§8.7: the MCU control block.
const MCU_CONTROL: u32 = 0x700;
const MCU_CONTROL_AUTO: u32 = 2;
const MCU_CONTROL_DISABLE: u32 = 0;
const MCU_STATUS: u32 = 0x704;
const MCU_STATUS_DISABLED: u32 = 0;

// RESEARCH/009 §4.5 and §7: the MMU block and address space 0's register window.
const MMU_INT_RAWSTAT: u32 = 0x2000;
const MMU_INT_CLEAR: u32 = 0x2004;
const MMU_INT_MASK: u32 = 0x2008;
const MMU_AS0: u32 = 0x2400;
const AS_TRANSTAB: u32 = MMU_AS0 + 0x00;
const AS_MEMATTR: u32 = MMU_AS0 + 0x08;
const AS_LOCKADDR: u32 = MMU_AS0 + 0x10;
const AS_COMMAND: u32 = MMU_AS0 + 0x18;
const AS_FAULTSTATUS: u32 = MMU_AS0 + 0x1C;
const AS_FAULTADDRESS: u32 = MMU_AS0 + 0x20;
const AS_STATUS: u32 = MMU_AS0 + 0x28;
const AS_TRANSCFG: u32 = MMU_AS0 + 0x30;
/// MMU status bit n (n < 16) is a page fault in address space n; bit 16 + n is that space's
/// command-complete bit and stays masked (RESEARCH/009 §4.5).
pub const MMU_AS0_FAULT_BIT: u32 = 1;
const AS_STATUS_AS_ACTIVE: u32 = 1;
const AS_COMMAND_UPDATE: u32 = 1;
const AS_COMMAND_LOCK: u32 = 2;
const AS_COMMAND_UNLOCK: u32 = 3;
const AS_COMMAND_FLUSH_PT: u32 = 4;
const AS_COMMAND_FLUSH_MEM: u32 = 5;
/// panthor locks the whole 64-bit range before a space's UPDATE: region width 63, base 0
/// (RESEARCH/009 §7.6's packing with start 0 and size 2^64).
const AS_LOCKADDR_WHOLE: u64 = 63;
/// RESEARCH/009 §7.3: panthor's non-coherent `mair_to_memattr` byte table for RK3588.
pub const AS_MEMATTR_NON_COHERENT: u64 = 0x4C4C_4C4C_8C4C_8F4C;
const AS_TRANSCFG_ADRMODE_AARCH64_4K: u64 = 6;
const AS_TRANSCFG_INA_BITS_SHIFT: u32 = 6;
const AS_TRANSCFG_PTW_MEMATTR_WB: u64 = 2 << 24;
const AS_TRANSCFG_PTW_RA: u64 = 1 << 30;
const AS_TRANSCFG_ADRMODE_UNMAPPED: u64 = 1;

/// RESEARCH/009 §6.4: panthor polls L2 transitions every 100 µs and gives the power-on 20 ms.
pub const L2_POWER_TIMEOUT_NS: u64 = 20_000_000;
const L2_POLL_INTERVAL_US: u64 = 100;
/// RESEARCH/009 §7.5: panthor waits for `AS_ACTIVE` to clear every 10 µs for up to 100 ms.
pub const MMU_COMMAND_TIMEOUT_NS: u64 = 100_000_000;
const MMU_POLL_INTERVAL_US: u64 = 10;
/// RESEARCH/009 §7.7/§8.7: the MCU's stop timeout, and the wait a deliberate fetch fault gets
/// before the driver calls it absent.
pub const MCU_STOP_TIMEOUT_NS: u64 = 100_000_000;
pub const FAULT_TIMEOUT_NS: u64 = 100_000_000;

/// Raw 32-bit register access, kept behind a trait so every sequence below is host-testable.
pub trait MmioIo {
    fn read32(&mut self, offset: u32) -> u32;
    fn write32(&mut self, offset: u32, value: u32);
}

/// Monotonic time and a cooperative delay, so register polling has a real deadline.
pub trait ClockIo {
    /// Nanoseconds since boot (the `ClockGet` value).
    fn now_ns(&mut self) -> u64;
    /// Give at least `us` microseconds back before the caller polls again.
    fn delay_us(&mut self, us: u64);
}

pub trait ProbeIo {
    fn power_up(&mut self) -> Status;
    fn power_status(&mut self) -> Result<DevicePowerStatus, Status>;
    fn map_aperture(&mut self) -> Result<(), ProbeError>;
    fn read32(&mut self, offset: u32) -> u32;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeError {
    PowerUp(Status),
    PowerStatus(Status),
    NotReady(DevicePowerStatus),
    Mint,
    Map(Status),
    Identity { gpu_id: u32, shader_present: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Identity {
    pub gpu_id: u32,
    pub shader_present: u64,
    pub power: DevicePowerStatus,
}

impl Identity {
    pub fn cores(self) -> u32 {
        self.shader_present.count_ones()
    }
}

/// No aperture access is possible through this path until power-up and the complete status
/// check succeed. The kernel independently guards the MMIO mint. — KESTREL
pub fn probe(io: &mut impl ProbeIo) -> Result<Identity, ProbeError> {
    let status = io.power_up();
    if status != Errno::Ok.status() {
        return Err(ProbeError::PowerUp(status));
    }
    let power = io.power_status().map_err(ProbeError::PowerStatus)?;
    let required = DevicePowerStatus::POWERED
        | DevicePowerStatus::REPAIR_DONE
        | DevicePowerStatus::CLOCK_AT_BOOT_RATE
        | DevicePowerStatus::APERTURE_MINTABLE;
    let idle = DevicePowerStatus::BUS_IDLE | DevicePowerStatus::BUS_IDLE_REQUESTED;
    if power.version != DevicePowerStatus::VERSION
        || !power.contains(required)
        || power.flags & idle != 0
    {
        return Err(ProbeError::NotReady(power));
    }
    io.map_aperture()?;
    let gpu_id = io.read32(GPU_ID);
    // Identify the documented model before reading any further GPU register. — KESTREL
    if gpu_id & 0xffff_0000 != 0xa867_0000 {
        return Err(ProbeError::Identity {
            gpu_id,
            shader_present: 0,
        });
    }
    let shader_present =
        u64::from(io.read32(SHADER_PRESENT_LO)) | (u64::from(io.read32(SHADER_PRESENT_HI)) << 32);
    if shader_present == 0 {
        return Err(ProbeError::Identity {
            gpu_id,
            shader_present,
        });
    }
    Ok(Identity {
        gpu_id,
        shader_present,
        power,
    })
}

/// The outcome of the bound completion-interrupt wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Completion {
    Fired,
    TimedOut,
}

/// How the reset completion was observed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResetPath {
    Interrupt,
    /// The wait expired, but panthor's RAWSTAT recheck showed completion anyway.
    PolledAfterTimeout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResetError {
    /// The completion wait could not be armed; no GPU register was touched.
    Arm(Status),
    /// No completion interrupt, and RAWSTAT does not show the reset bit.
    Timeout { rawstat: u32 },
    /// The wait fired, but RAWSTAT does not show the reset bit.
    UnexpectedCompletion { rawstat: u32 },
}

/// The device-side half of a bound completion wait, kept behind a trait so sequences are
/// host-testable.
pub trait CompletionIo {
    /// Create the interrupt for `irq` and bind its wait **before** the event source runs.
    /// The wait must expire after `timeout_ns`.
    fn arm_completion(&mut self, irq: u32, timeout_ns: u64) -> Result<(), Status>;
    fn wait_completion(&mut self) -> Completion;
    /// Release the wait objects and acknowledge the interrupt.
    fn complete(&mut self);
}

/// G2a: ask the GPU for a soft reset and require its completion interrupt. Clears any stale
/// completion, unmasks only `RESET_COMPLETED`, and always restores a quiet mask before
/// acknowledging, so a failed or late reset cannot storm a line nobody serves. — TIMBERDOODLE
pub fn soft_reset(io: &mut (impl MmioIo + CompletionIo)) -> Result<ResetPath, ResetError> {
    io.arm_completion(GPU_IRQ, RESET_TIMEOUT_NS)
        .map_err(ResetError::Arm)?;
    io.write32(GPU_INT_CLEAR, GPU_IRQ_RESET_COMPLETED);
    io.write32(GPU_INT_MASK, GPU_IRQ_RESET_COMPLETED);
    io.write32(GPU_CMD, GPU_CMD_SOFT_RESET);
    let completion = io.wait_completion();
    // panthor rechecks RAWSTAT after its timeout before declaring failure (RESEARCH/009 §4.4).
    let rawstat = io.read32(GPU_INT_RAWSTAT);
    io.write32(GPU_INT_CLEAR, GPU_IRQ_RESET_COMPLETED);
    io.write32(GPU_INT_MASK, 0);
    io.complete();
    if rawstat & GPU_IRQ_RESET_COMPLETED == 0 {
        return Err(match completion {
            Completion::Fired => ResetError::UnexpectedCompletion { rawstat },
            Completion::TimedOut => ResetError::Timeout { rawstat },
        });
    }
    Ok(match completion {
        Completion::Fired => ResetPath::Interrupt,
        Completion::TimedOut => ResetPath::PolledAfterTimeout,
    })
}

fn read64(io: &mut impl MmioIo, lo: u32, hi: u32) -> u64 {
    u64::from(io.read32(lo)) | (u64::from(io.read32(hi)) << 32)
}

fn write64(io: &mut impl MmioIo, lo: u32, hi: u32, value: u64) {
    io.write32(lo, value as u32);
    io.write32(hi, (value >> 32) as u32);
}

/// Poll one word until `ready` accepts it, returning the accepted value or the last one seen.
fn poll32(
    io: &mut (impl MmioIo + ClockIo),
    offset: u32,
    timeout_ns: u64,
    interval_us: u64,
    mut ready: impl FnMut(u32) -> bool,
) -> Result<u32, u32> {
    let deadline = io.now_ns().saturating_add(timeout_ns);
    loop {
        let value = io.read32(offset);
        if ready(value) {
            return Ok(value);
        }
        if io.now_ns() >= deadline {
            return Err(value);
        }
        io.delay_us(interval_us);
    }
}

/// The L2 group panthor powers on this part; `L2_PRESENT` bit 0 must be set (RESEARCH/009 §6.4).
pub const L2_POWER_MASK: u64 = 1;

/// What the L2 power-on observed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct L2Power {
    pub present: u64,
    pub ready: u64,
    pub coherency_features: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum L2Error {
    /// `L2_PRESENT` bit 0 is clear: there is no cache group to power.
    Absent,
    /// `L2_PWRTRANS` never cleared.
    Transition,
    /// The cache never reported ready.
    Readiness { ready: u64 },
}

/// RESEARCH/009 §6.4: set the coherency protocol, wait out any pending transition, power L2
/// group 0 and require it to report ready. The READY test is panthor v6.10's
/// `(mask & val) == mask`; master's `(mask & val) == val` is already true while READY reads 0
/// and would wait for nothing. — TIMBERDOODLE
pub fn power_l2(io: &mut (impl MmioIo + ClockIo)) -> Result<L2Power, L2Error> {
    let present = read64(io, GPU_L2_PRESENT_LO, GPU_L2_PRESENT_HI);
    if present & L2_POWER_MASK == 0 {
        return Err(L2Error::Absent);
    }
    let coherency_features = io.read32(GPU_COHERENCY_FEATURES);
    // The protocol precedes L2 power-up (RESEARCH/009 §6.4 step 1).
    io.write32(GPU_COHERENCY_PROTOCOL, GPU_COHERENCY_NONE);
    let clear = |value: u32| value & L2_POWER_MASK as u32 == 0;
    poll32(
        io,
        L2_PWRTRANS_LO,
        L2_POWER_TIMEOUT_NS,
        L2_POLL_INTERVAL_US,
        clear,
    )
    .map_err(|_| L2Error::Transition)?;
    poll32(
        io,
        L2_PWRTRANS_HI,
        L2_POWER_TIMEOUT_NS,
        L2_POLL_INTERVAL_US,
        clear,
    )
    .map_err(|_| L2Error::Transition)?;
    write64(io, L2_PWRON_LO, L2_PWRON_HI, L2_POWER_MASK);
    let deadline = io.now_ns().saturating_add(L2_POWER_TIMEOUT_NS);
    loop {
        let ready = read64(io, L2_READY_LO, L2_READY_HI);
        if ready & L2_POWER_MASK == L2_POWER_MASK {
            return Ok(L2Power {
                present,
                ready,
                coherency_features,
            });
        }
        if io.now_ns() >= deadline {
            return Err(L2Error::Readiness { ready });
        }
        io.delay_us(L2_POLL_INTERVAL_US);
    }
}

/// The MMU's shape, from `MMU_FEATURES` [7:0] virtual-address bits and [15:8] physical-address
/// bits, plus which address spaces exist (RESEARCH/009 §4.3/§7.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmuInfo {
    pub features: u32,
    pub va_bits: u32,
    pub pa_bits: u32,
    pub as_present: u32,
}

pub fn mmu_info(io: &mut impl MmioIo) -> MmuInfo {
    let features = io.read32(GPU_MMU_FEATURES);
    MmuInfo {
        features,
        va_bits: features & 0xff,
        pa_bits: (features >> 8) & 0xff,
        as_present: io.read32(GPU_AS_PRESENT),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AsError {
    /// `AS_ACTIVE` would not clear around a command.
    Busy { status: u32 },
    /// The hardware has no such address space (eight exist; RESEARCH/009 §7.1).
    Space { space: u32 },
}

/// RESEARCH/009 §7.4: AArch64 4 KiB stage-1 walks, write-back and read-allocate, with the
/// input-address width the hardware reports (55 − VA bits). 48 VA bits give 0x4200_01C6.
pub fn as_transcfg(va_bits: u32) -> u64 {
    AS_TRANSCFG_ADRMODE_AARCH64_4K
        | (u64::from(55 - va_bits) << AS_TRANSCFG_INA_BITS_SHIFT)
        | AS_TRANSCFG_PTW_MEMATTR_WB
        | AS_TRANSCFG_PTW_RA
}

/// The MMU's per-space register window: space n starts at `0x2400 + n*0x40` (RESEARCH/009
/// §4.8), and this GPU reports eight spaces (`AS_PRESENT = 0xFF`). — TIMBERDOODLE
pub fn as_window(space: u32) -> Option<u32> {
    if space >= 8 {
        return None;
    }
    Some(MMU_AS0 + space * 0x40)
}

/// The window-relative offset of an AS0-absolute register constant, so the offsets are written
/// once above and every space's helper adds them to its own window.
const fn as_offset(register: u32) -> u32 {
    register - MMU_AS0
}

fn wait_as_idle(io: &mut (impl MmioIo + ClockIo), window: u32) -> Result<(), AsError> {
    match poll32(
        io,
        window + as_offset(AS_STATUS),
        MMU_COMMAND_TIMEOUT_NS,
        MMU_POLL_INTERVAL_US,
        |value| value & AS_STATUS_AS_ACTIVE == 0,
    ) {
        Ok(_) => Ok(()),
        Err(status) => Err(AsError::Busy { status }),
    }
}

fn as_command(io: &mut (impl MmioIo + ClockIo), window: u32, command: u32) -> Result<(), AsError> {
    wait_as_idle(io, window)?;
    io.write32(window + as_offset(AS_COMMAND), command);
    wait_as_idle(io, window)
}

/// panthor's opening move on a space: lock the whole range and flush its memory
/// (RESEARCH/009 §7.5; `panthor_mmu_as_enable`/`_disable`).
fn lock_and_flush_mem(io: &mut (impl MmioIo + ClockIo), window: u32) -> Result<(), AsError> {
    let lockaddr = window + as_offset(AS_LOCKADDR);
    write64(io, lockaddr, lockaddr + 4, AS_LOCKADDR_WHOLE);
    as_command(io, window, AS_COMMAND_LOCK)?;
    as_command(io, window, AS_COMMAND_FLUSH_MEM)
}

/// RESEARCH/009 §7.5: publish `transtab`, the memory-attribute table and the translation
/// config, then `UPDATE` so the MMU loads them.
pub fn enable_as(
    io: &mut (impl MmioIo + ClockIo),
    space: u32,
    transtab: u64,
    va_bits: u32,
) -> Result<(), AsError> {
    let Some(window) = as_window(space) else {
        return Err(AsError::Space { space });
    };
    lock_and_flush_mem(io, window)?;
    let transtab_lo = window + as_offset(AS_TRANSTAB);
    write64(io, transtab_lo, transtab_lo + 4, transtab);
    let memattr_lo = window + as_offset(AS_MEMATTR);
    write64(io, memattr_lo, memattr_lo + 4, AS_MEMATTR_NON_COHERENT);
    let transcfg_lo = window + as_offset(AS_TRANSCFG);
    write64(io, transcfg_lo, transcfg_lo + 4, as_transcfg(va_bits));
    as_command(io, window, AS_COMMAND_UPDATE)
}

/// Address space 0 is the MCU's; G3b uses this wrapper.
pub fn enable_as0(
    io: &mut (impl MmioIo + ClockIo),
    transtab: u64,
    va_bits: u32,
) -> Result<(), AsError> {
    enable_as(io, 0, transtab, va_bits)
}

/// RESEARCH/009 §7.5: flush, then leave the space unmapped.
pub fn disable_as(io: &mut (impl MmioIo + ClockIo), space: u32) -> Result<(), AsError> {
    let Some(window) = as_window(space) else {
        return Err(AsError::Space { space });
    };
    lock_and_flush_mem(io, window)?;
    let transtab_lo = window + as_offset(AS_TRANSTAB);
    write64(io, transtab_lo, transtab_lo + 4, 0);
    let memattr_lo = window + as_offset(AS_MEMATTR);
    write64(io, memattr_lo, memattr_lo + 4, 0);
    let transcfg_lo = window + as_offset(AS_TRANSCFG);
    write64(
        io,
        transcfg_lo,
        transcfg_lo + 4,
        AS_TRANSCFG_ADRMODE_UNMAPPED,
    );
    as_command(io, window, AS_COMMAND_UPDATE)
}

pub fn disable_as0(io: &mut (impl MmioIo + ClockIo)) -> Result<(), AsError> {
    disable_as(io, 0)
}

/// Flush a live space's page-table caches: lock the whole range, `FLUSH_PT`, unlock
/// (RESEARCH/009 §7.6; panthor runs this after every live mapping change). KUMO maps the queue
/// pages and the suspend buffer into AS0 after it is enabled, so those edits need it. — TIMBERDOODLE
pub fn flush_page_tables(io: &mut (impl MmioIo + ClockIo), space: u32) -> Result<(), AsError> {
    let Some(window) = as_window(space) else {
        return Err(AsError::Space { space });
    };
    let lockaddr = window + as_offset(AS_LOCKADDR);
    write64(io, lockaddr, lockaddr + 4, AS_LOCKADDR_WHOLE);
    as_command(io, window, AS_COMMAND_LOCK)?;
    as_command(io, window, AS_COMMAND_FLUSH_PT)?;
    as_command(io, window, AS_COMMAND_UNLOCK)
}

/// Clear a latched address-space-0 fault and keep the MMU line masked.
pub fn quiet_mmu(io: &mut impl MmioIo) {
    io.write32(MMU_INT_CLEAR, MMU_AS0_FAULT_BIT);
    io.write32(MMU_INT_MASK, 0);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McuError {
    StopTimeout { status: u32 },
}

/// RESEARCH/009 §8.7: disable the MCU and require it to report disabled.
///
/// Metal (J576): with no firmware loaded this never leaves ENABLED — the stop path expects the
/// GLB halt handshake only firmware can acknowledge, so a faulted firmware-less MCU is contained
/// by a soft reset instead. This stays as G3's post-halt stop primitive. — TIMBERDOODLE
pub fn stop_mcu(io: &mut (impl MmioIo + ClockIo)) -> Result<u32, McuError> {
    io.write32(MCU_CONTROL, MCU_CONTROL_DISABLE);
    match poll32(
        io,
        MCU_STATUS,
        MCU_STOP_TIMEOUT_NS,
        MMU_POLL_INTERVAL_US,
        |value| value == MCU_STATUS_DISABLED,
    ) {
        Ok(status) => Ok(status),
        Err(status) => Err(McuError::StopTimeout { status }),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultAccess {
    Atomic,
    Execute,
    Read,
    Write,
}

/// A decoded `AS_FAULTSTATUS`/`AS_FAULTADDRESS` pair (RESEARCH/009 §5.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fault {
    pub raw: u32,
    pub address: u64,
    /// Exception code, `AS_FAULTSTATUS[7:0]` (RESEARCH/009 §5).
    pub exception: u8,
    /// `AS_FAULTSTATUS[9:8]`.
    pub access: FaultAccess,
    /// `AS_FAULTSTATUS[10]`: 1 decoder fault, 0 slave fault.
    pub decoder: bool,
    /// `AS_FAULTSTATUS[31:16]`.
    pub source: u16,
}

pub fn decode_fault(raw: u32, address: u64) -> Fault {
    Fault {
        raw,
        address,
        exception: (raw & 0xff) as u8,
        access: match (raw >> 8) & 0x3 {
            0 => FaultAccess::Atomic,
            1 => FaultAccess::Execute,
            2 => FaultAccess::Read,
            _ => FaultAccess::Write,
        },
        decoder: raw & (1 << 10) != 0,
        source: (raw >> 16) as u16,
    }
}

/// RESEARCH/009 §5: the exception codes the GPU shares between `AS_FAULTSTATUS` and
/// `GPU_FAULT_STATUS`.
pub fn exception_name(code: u8) -> &'static str {
    match code {
        0x00 => "OK",
        0x04 => "TERMINATED",
        0x05 => "KABOOM",
        0x06 => "EUREKA",
        0x80 => "GPU_BUS_FAULT",
        0x88 => "GPU_SHAREABILITY_FAULT",
        0x89 => "SYS_SHAREABILITY_FAULT",
        0x8a => "GPU_CACHEABILITY_FAULT",
        0xc0..=0xc4 => "TRANSLATION_FAULT",
        0xc8..=0xcb => "PERMISSION_FAULT",
        0xd9..=0xdb => "ACCESS_FLAG_FAULT",
        0xe0 => "ADDRESS_SIZE_FAULT_IN",
        0xe4..=0xe7 => "ADDRESS_SIZE_FAULT_OUT",
        0xe8..=0xeb => "MEMORY_ATTRIBUTE_FAULT",
        _ => "UNKNOWN",
    }
}

fn read_fault(io: &mut impl MmioIo) -> Fault {
    let raw = io.read32(AS_FAULTSTATUS);
    let address = read64(io, AS_FAULTADDRESS, AS_FAULTADDRESS + 4);
    decode_fault(raw, address)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FaultObserved {
    pub fault: Fault,
    /// `Fired` when the bound interrupt delivered the fault; `TimedOut` when only the RAWSTAT
    /// recheck saw it.
    pub completion: Completion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultWait {
    /// The completion wait could not be armed; no MMU register was touched.
    Arm(Status),
    /// No fault interrupt, and RAWSTAT does not show bit 0.
    Timeout { rawstat: u32 },
    /// The wait fired, but RAWSTAT does not show bit 0.
    UnexpectedCompletion { rawstat: u32 },
}

/// G2b's deliberate fault: arm the MMU line, start the MCU, and wait for address space 0 to
/// fault. The line is armed before `MCU_CONTROL` is written, and every path restores a quiet
/// mask before acknowledging, so a late fault cannot storm a line nobody serves. A timeout
/// takes panthor's `MMU_INT_RAWSTAT` recheck. — TIMBERDOODLE
pub fn mcu_fault(io: &mut (impl MmioIo + CompletionIo)) -> Result<FaultObserved, FaultWait> {
    io.arm_completion(MMU_IRQ, FAULT_TIMEOUT_NS)
        .map_err(FaultWait::Arm)?;
    io.write32(MMU_INT_CLEAR, MMU_AS0_FAULT_BIT);
    io.write32(MMU_INT_MASK, MMU_AS0_FAULT_BIT);
    // RESEARCH/009 §8.6: AUTO starts the MCU alongside the already-powered L2.
    io.write32(MCU_CONTROL, MCU_CONTROL_AUTO);
    let completion = io.wait_completion();
    let rawstat = io.read32(MMU_INT_RAWSTAT);
    let fault = if rawstat & MMU_AS0_FAULT_BIT != 0 {
        Some(read_fault(io))
    } else {
        None
    };
    io.write32(MMU_INT_CLEAR, MMU_AS0_FAULT_BIT);
    io.write32(MMU_INT_MASK, 0);
    io.complete();
    match fault {
        Some(fault) => Ok(FaultObserved { fault, completion }),
        None => Err(match completion {
            Completion::Fired => FaultWait::UnexpectedCompletion { rawstat },
            Completion::TimedOut => FaultWait::Timeout { rawstat },
        }),
    }
}

/// RESEARCH/009 §7.2: 48 VA bits with a 4 KiB granule is four levels of 512 eight-byte
/// descriptors, and the MMU's physical-address field is 40 bits wide.
pub const PAGE_SIZE: u64 = 4096;
pub const PAGE_TABLE_ENTRIES: usize = 512;
pub const PAGE_TABLE_LEVELS: u32 = 4;
const PHYSICAL_ADDRESS_MASK: u64 = 0x0000_00ff_ffff_f000;
const TABLE_DESCRIPTOR_TYPE: u64 = 0b11;
/// The known-good no-execute encoding: panthor sets XN in bits 53 and 54 and the GPU accepts
/// it, even though the Arm ARM calls bit 53 RES0 (RESEARCH/009 §7.2).
pub const NO_EXECUTE: u64 = 0b11 << 53;

/// The 9-bit index a level takes out of a 48-bit address: L0 [47:39], L1 [38:30], L2 [29:21],
/// L3 [20:12].
pub fn table_index(va: u64, level: u32) -> usize {
    ((va >> (12 + 9 * (PAGE_TABLE_LEVELS - 1 - level))) & 0x1ff) as usize
}

/// A next-level table descriptor: type 3 and the child table's physical address.
pub fn table_descriptor(next_phys: u64) -> u64 {
    (next_phys & PHYSICAL_ADDRESS_MASK) | TABLE_DESCRIPTOR_TYPE
}

/// A leaf descriptor with panthor's attribute choices (RESEARCH/009 §7.2): cached pages use
/// AttrIndx 1 and inner shareability, uncached ones AttrIndx 0 and outer; AP[2] marks a page
/// read-only; AP[1], AF and nG are always set.
pub fn page_descriptor(pa: u64, cached: bool, readonly: bool, noexec: bool) -> u64 {
    let mut descriptor = (pa & PHYSICAL_ADDRESS_MASK) | 0xC43;
    descriptor |= if cached { 0x4 | 0x300 } else { 0x200 };
    if readonly {
        descriptor |= 0x80;
    }
    if noexec {
        descriptor |= NO_EXECUTE;
    }
    descriptor
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mapping {
    pub va: u64,
    pub pa: u64,
    pub cached: bool,
    pub readonly: bool,
    pub noexec: bool,
}

/// Sink for page-table entries: the driver writes them as volatile stores into DMA memory.
pub trait TableWriter {
    fn write_entry(&mut self, level: u32, index: usize, value: u64);
}

/// Link one 4 KiB page at one VA through all four levels. `table_phys` is the physical address
/// of each level's 4 KiB table, L0 first; the caller has already zeroed every table.
pub fn install_mapping(writer: &mut impl TableWriter, mapping: Mapping, table_phys: [u64; 4]) {
    for level in 0..(PAGE_TABLE_LEVELS - 1) {
        writer.write_entry(
            level,
            table_index(mapping.va, level),
            table_descriptor(table_phys[level as usize + 1]),
        );
    }
    writer.write_entry(
        PAGE_TABLE_LEVELS - 1,
        table_index(mapping.va, PAGE_TABLE_LEVELS - 1),
        page_descriptor(mapping.pa, mapping.cached, mapping.readonly, mapping.noexec),
    );
}

// ---- G3a: the CSF firmware image (RESEARCH/009 §8) ----

/// RESEARCH/009 §8.2: the image header is 20 bytes; every integer is little-endian.
pub const FIRMWARE_HEADER_LEN: usize = 20;
pub const FIRMWARE_MAGIC: u32 = 0xC3F1_3A6E;
/// RESEARCH/009 §7.8/§8.4: the MCU's shared region, whose first section the firmware marks
/// SHARED and panthor requires to exist.
pub const CSF_SHARED_REGION_START: u64 = 0x0400_0000;
/// RESEARCH/009 §7.8: the shared region ends here; the firmware confines host-supplied MCU
/// addresses to it. Metal (J584) showed an address outside the region truncated into the low
/// firmware image, so every KUMO allocation the firmware addresses must live inside.
pub const CSF_SHARED_REGION_END: u64 = 0x0800_0000;

pub const fn shared_region_contains(va: u64) -> bool {
    va >= CSF_SHARED_REGION_START && va < CSF_SHARED_REGION_END
}
/// The MCU's VM uses a 4 KiB granule (RESEARCH/009 §7.2), so every section VA is page-aligned.
pub const FIRMWARE_PAGE_SIZE: u64 = 4096;
const FIRMWARE_ENTRY_UPDATE: u32 = 1 << 30;
const FIRMWARE_ENTRY_OPTIONAL: u32 = 1 << 31;

/// `CSF_FW_BINARY_IFACE_ENTRY_*` (RESEARCH/009 §8.4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SectionFlags(pub u32);

impl SectionFlags {
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const EXECUTE: Self = Self(1 << 2);
    const CACHE_MODE_MASK: u32 = 0b11 << 3;
    pub const PROT: Self = Self(1 << 5);
    pub const SHARED: Self = Self(1 << 30);
    pub const ZERO: Self = Self(1 << 31);
    const SUPPORTED: u32 = Self::READ.0
        | Self::WRITE.0
        | Self::EXECUTE.0
        | Self::CACHE_MODE_MASK
        | Self::PROT.0
        | Self::SHARED.0
        | Self::ZERO.0;

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// RESEARCH/009 §8.4: the cache mode in bits [4:3]; 0 none, 1 cached, 2 uncached-coherent,
    /// 3 cached-coherent.
    pub const fn cache_mode(self) -> u8 {
        ((self.0 >> 3) & 0b11) as u8
    }

    /// RESEARCH/009 §8.4: panthor maps a section uncached unless the cache mode is CACHED; the
    /// two coherent modes are also mapped uncached.
    pub const fn cached(self) -> bool {
        self.cache_mode() == 1
    }

    pub const fn writable(self) -> bool {
        self.0 & Self::WRITE.0 != 0
    }

    pub const fn executable(self) -> bool {
        self.0 & Self::EXECUTE.0 != 0
    }
}

impl core::ops::BitOr for SectionFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

/// One validated IFACE entry (RESEARCH/009 §8.4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FirmwareSection<'a> {
    /// The entry's trailing bytes. The format does not promise text: the arch10.8 image in
    /// `resources/` carries binary values here, so this stays opaque. — TIMBERDOODLE
    pub name: &'a [u8],
    /// The MCU virtual address the section starts at.
    pub va: u64,
    /// `va.end - va.start`.
    pub len: u64,
    /// The image bytes that initialize the section (`data.start..data.end`).
    pub data: &'a [u8],
    pub flags: SectionFlags,
    /// The entry's UPDATE bit. panthor does not act on it at load time.
    pub update: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FirmwareError {
    /// The image is shorter than the 20-byte header.
    HeaderShort {
        len: usize,
    },
    Magic {
        magic: u32,
    },
    /// `major` must be 0 (RESEARCH/009 §8.2).
    Major {
        major: u8,
    },
    /// A header padding word was not zero.
    HeaderPadding,
    /// The declared size is below the header or beyond the file.
    Size {
        size: u32,
        len: usize,
    },
    /// An entry's size is not a positive multiple of 4.
    EntrySize {
        offset: usize,
        size: u8,
    },
    /// An entry lies outside the declared image size.
    EntryOverrun {
        offset: usize,
        size: u8,
    },
    /// An unknown entry type without the OPTIONAL bit (RESEARCH/009 §8.3).
    UnknownEntry {
        offset: usize,
        kind: u8,
    },
    /// An IFACE entry is too small for the five-word section header.
    SectionShort {
        offset: usize,
    },
    /// `data.end < data.start` or `va.end < va.start`.
    SectionRange {
        offset: usize,
    },
    /// `data.end` lies beyond the file.
    SectionData {
        offset: usize,
    },
    /// A section VA is not page-aligned.
    SectionAlignment {
        va_start: u32,
        va_end: u32,
    },
    /// A section carries flag bits outside `SUPPORTED`.
    SectionFlags {
        offset: usize,
        flags: u32,
    },
    /// The data is larger than the section it initializes (RESEARCH/009 §8.4).
    SectionTooLarge {
        offset: usize,
    },
    /// The section at the shared region start lacks SHARED.
    SharedSectionNotShared {
        va: u64,
    },
    /// No non-PROT section marks the shared region.
    SharedSectionMissing,
    /// A build-info entry is too small for its two-word header.
    BuildInfoShort {
        offset: usize,
    },
}

struct Header {
    magic: u32,
    minor: u8,
    major: u8,
    padding1: u16,
    version_hash: u32,
    padding2: u32,
    size: u32,
}

fn header(bytes: &[u8]) -> Option<Header> {
    let raw = bytes.get(..FIRMWARE_HEADER_LEN)?;
    Some(Header {
        magic: u32::from_le_bytes(raw[0..4].try_into().ok()?),
        minor: raw[4],
        major: raw[5],
        padding1: u16::from_le_bytes(raw[6..8].try_into().ok()?),
        version_hash: u32::from_le_bytes(raw[8..12].try_into().ok()?),
        padding2: u32::from_le_bytes(raw[12..16].try_into().ok()?),
        size: u32::from_le_bytes(raw[16..20].try_into().ok()?),
    })
}

enum Entry<'a> {
    Section(FirmwareSection<'a>),
    Build(Option<&'a [u8]>),
    Ignored,
}

/// Parse one entry and return it with the offset of the next one. The rules and their order
/// follow panthor as source-as-spec (RESEARCH/009 §8.3/§8.4). — TIMBERDOODLE
fn entry_at<'a>(
    bytes: &'a [u8],
    offset: usize,
    end: usize,
) -> Result<(Entry<'a>, usize), FirmwareError> {
    if offset % 4 != 0 {
        return Err(FirmwareError::EntrySize { offset, size: 0 });
    }
    let Some(raw) = bytes.get(offset..offset.saturating_add(4)) else {
        return Err(FirmwareError::EntryOverrun { offset, size: 0 });
    };
    let ehdr = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
    let kind = (ehdr & 0xff) as u8;
    let size = ((ehdr >> 8) & 0xff) as usize;
    if size % 4 != 0 || size < 4 {
        return Err(FirmwareError::EntrySize {
            offset,
            size: size as u8,
        });
    }
    let next = offset + size;
    if next > end {
        return Err(FirmwareError::EntryOverrun {
            offset,
            size: size as u8,
        });
    }
    let body = offset + 4;
    let entry = match kind {
        0 => Entry::Section(section(bytes, body, next, ehdr)?),
        6 => Entry::Build(build_sha(bytes, body, next)?),
        1..=4 => Entry::Ignored,
        _ if ehdr & FIRMWARE_ENTRY_OPTIONAL != 0 => Entry::Ignored,
        _ => return Err(FirmwareError::UnknownEntry { offset, kind }),
    };
    Ok((entry, next))
}

fn section<'a>(
    bytes: &'a [u8],
    body: usize,
    end: usize,
    ehdr: u32,
) -> Result<FirmwareSection<'a>, FirmwareError> {
    if end.saturating_sub(body) < 20 {
        return Err(FirmwareError::SectionShort { offset: body });
    }
    let raw = &bytes[body..end];
    let flags = SectionFlags(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]));
    let va_start = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
    let va_end = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]);
    let data_start = u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]);
    let data_end = u32::from_le_bytes([raw[16], raw[17], raw[18], raw[19]]);
    if data_end < data_start || va_end < va_start {
        return Err(FirmwareError::SectionRange { offset: body });
    }
    if data_end as usize > bytes.len() {
        return Err(FirmwareError::SectionData { offset: body });
    }
    if u64::from(va_start) % FIRMWARE_PAGE_SIZE != 0 || u64::from(va_end) % FIRMWARE_PAGE_SIZE != 0
    {
        return Err(FirmwareError::SectionAlignment { va_start, va_end });
    }
    if flags.0 & !SectionFlags::SUPPORTED != 0 {
        return Err(FirmwareError::SectionFlags {
            offset: body,
            flags: flags.0,
        });
    }
    // panthor skips a protected-mode section before its shared-region check.
    if !flags.contains(SectionFlags::PROT)
        && u64::from(va_start) == CSF_SHARED_REGION_START
        && !flags.contains(SectionFlags::SHARED)
    {
        return Err(FirmwareError::SharedSectionNotShared {
            va: u64::from(va_start),
        });
    }
    // RESEARCH/009 §8.4's loading rule. panthor copies without this check, so a malformed image
    // that reaches it would overflow its section buffer; the parser refuses it here instead.
    if data_end - data_start > va_end - va_start {
        return Err(FirmwareError::SectionTooLarge { offset: body });
    }
    Ok(FirmwareSection {
        name: &bytes[body + 20..end],
        va: u64::from(va_start),
        len: u64::from(va_end - va_start),
        data: &bytes[data_start as usize..data_end as usize],
        flags,
        update: ehdr & FIRMWARE_ENTRY_UPDATE != 0,
    })
}

/// RESEARCH/009 §8.5: the metadata points at `"git_sha: <sha>\0"`. panthor treats corrupt
/// metadata as non-fatal, so this reports `None` rather than an error for anything unusable.
fn build_sha<'a>(
    bytes: &'a [u8],
    body: usize,
    end: usize,
) -> Result<Option<&'a [u8]>, FirmwareError> {
    if end.saturating_sub(body) < 8 {
        return Err(FirmwareError::BuildInfoShort { offset: body });
    }
    let raw = &bytes[body..end];
    let meta_start = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
    let meta_size = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
    let Some(meta_end) = meta_start.checked_add(meta_size) else {
        return Ok(None);
    };
    if meta_end > bytes.len() {
        return Ok(None);
    }
    let text = &bytes[meta_start..meta_end];
    const PREFIX: &[u8] = b"git_sha: ";
    if text.len() <= PREFIX.len() || !text.starts_with(PREFIX) || text[text.len() - 1] != 0 {
        return Ok(None);
    }
    Ok(Some(&text[PREFIX.len()..text.len() - 1]))
}

/// A validated CSF firmware image (RESEARCH/009 §8.2-§8.5). `parse` walks the whole entry table
/// once and refuses every malformed case the references name, so `sections` can re-walk without
/// error handling. The bytes are borrowed; the caller keeps the image alive. — TIMBERDOODLE
#[derive(Clone, Copy, Debug)]
pub struct Firmware<'a> {
    bytes: &'a [u8],
    minor: u8,
    version_hash: u32,
    end: usize,
    build_sha: Option<&'a [u8]>,
}

impl<'a> Firmware<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, FirmwareError> {
        let Some(header) = header(bytes) else {
            return Err(FirmwareError::HeaderShort { len: bytes.len() });
        };
        if header.magic != FIRMWARE_MAGIC {
            return Err(FirmwareError::Magic {
                magic: header.magic,
            });
        }
        if header.major != 0 {
            return Err(FirmwareError::Major {
                major: header.major,
            });
        }
        if header.padding1 != 0 || header.padding2 != 0 {
            return Err(FirmwareError::HeaderPadding);
        }
        let end = header.size as usize;
        if end < FIRMWARE_HEADER_LEN || end > bytes.len() {
            return Err(FirmwareError::Size {
                size: header.size,
                len: bytes.len(),
            });
        }
        let mut offset = FIRMWARE_HEADER_LEN;
        let mut shared = false;
        let mut build_sha = None;
        while offset < end {
            let (entry, next) = entry_at(bytes, offset, end)?;
            match entry {
                Entry::Section(section) => {
                    if section.va == CSF_SHARED_REGION_START
                        && !section.flags.contains(SectionFlags::PROT)
                        && section.flags.contains(SectionFlags::SHARED)
                    {
                        shared = true;
                    }
                }
                Entry::Build(sha) => build_sha = sha,
                Entry::Ignored => {}
            }
            offset = next;
        }
        // panthor refuses an image whose shared interface section is missing.
        if !shared {
            return Err(FirmwareError::SharedSectionMissing);
        }
        Ok(Self {
            bytes,
            minor: header.minor,
            version_hash: header.version_hash,
            end,
            build_sha,
        })
    }

    pub const fn minor(&self) -> u8 {
        self.minor
    }

    pub const fn version_hash(&self) -> u32 {
        self.version_hash
    }

    pub const fn build_sha(&self) -> Option<&'a [u8]> {
        self.build_sha
    }

    /// Every IFACE section, PROT included. The loader skips PROT sections (RESEARCH/009 §8.4);
    /// `parse` only counts a non-PROT section as the shared interface.
    pub fn sections(&self) -> Sections<'a> {
        Sections {
            bytes: self.bytes,
            offset: FIRMWARE_HEADER_LEN,
            end: self.end,
        }
    }
}

/// Walks the image a second time. `Firmware::parse` already proved every entry valid, so a
/// parse failure here cannot occur; it ends the iteration rather than panicking.
pub struct Sections<'a> {
    bytes: &'a [u8],
    offset: usize,
    end: usize,
}

impl<'a> Iterator for Sections<'a> {
    type Item = FirmwareSection<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.offset < self.end {
            let (entry, next) = entry_at(self.bytes, self.offset, self.end).ok()?;
            self.offset = next;
            if let Entry::Section(section) = entry {
                return Some(section);
            }
        }
        None
    }
}

// ---- G3b: loading the firmware into address space 0 (RESEARCH/009 §7.2/§7.5/§8.6/§9.3) ----

/// Physical access to page-table memory. The driver implements this over an uncached DMA mapping
/// of the table arena; host tests implement it over their own store.
pub trait TableMemory {
    fn read64(&mut self, phys: u64) -> u64;
    fn write64(&mut self, phys: u64, value: u64);
}

/// A bump allocator over one contiguous physical range of 4 KiB tables. Tables are zeroed when
/// handed out, so a fresh walk always starts from invalid entries. — TIMBERDOODLE
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TableArena {
    next: u64,
    end: u64,
}

impl TableArena {
    pub fn new(base_phys: u64, len: u64) -> Option<Self> {
        if base_phys % PAGE_SIZE != 0 || len < PAGE_SIZE || len % PAGE_SIZE != 0 {
            return None;
        }
        Some(Self {
            next: base_phys,
            end: base_phys.checked_add(len)?,
        })
    }

    /// Hand out and zero one table. The first call is the caller's L0 (the `TRANSTAB` value).
    pub fn alloc_table(&mut self, memory: &mut impl TableMemory) -> Option<u64> {
        if self.next >= self.end {
            return None;
        }
        let table = self.next;
        self.next += PAGE_SIZE;
        zero_table(memory, table);
        Some(table)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TableError {
    /// The arena has no free table.
    OutOfTables,
    /// The range is empty, unaligned, or leaves the 48-bit window.
    Range { va: u64, len: u64 },
    /// An entry that must be a table is a block descriptor.
    Collision { level: u32 },
}

/// The attributes a leaf page is mapped with (RESEARCH/009 §7.2; panthor's map flags).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageAttrs {
    pub cached: bool,
    pub readonly: bool,
    pub executable: bool,
}

impl SectionFlags {
    /// RESEARCH/009 §8.4: read-only unless WR, no-execute unless EX, uncached unless the cache
    /// mode is CACHED. PROT handling stays with the caller, which skips those sections.
    pub const fn page_attrs(self) -> PageAttrs {
        PageAttrs {
            cached: self.cached(),
            readonly: !self.writable(),
            executable: self.executable(),
        }
    }
}

fn zero_table(memory: &mut impl TableMemory, phys: u64) {
    for index in 0..PAGE_TABLE_ENTRIES {
        memory.write64(phys + index as u64 * 8, 0);
    }
}

/// Walk (and allocate) the chain from `transtab` to the table holding `va`'s entries at `target`
/// level (3 for 4 KiB pages). Existing table descriptors are reused, so ranges sharing a parent
/// share its tables. — TIMBERDOODLE
fn resolve_table(
    memory: &mut impl TableMemory,
    arena: &mut TableArena,
    transtab: u64,
    va: u64,
    target: u32,
) -> Result<u64, TableError> {
    let mut table = transtab;
    for level in 0..target {
        let entry = table + table_index(va, level) as u64 * 8;
        let value = memory.read64(entry);
        table = if value == 0 {
            let next = arena.alloc_table(memory).ok_or(TableError::OutOfTables)?;
            memory.write64(entry, table_descriptor(next));
            next
        } else if value & 0b11 == 0b11 {
            value & PHYSICAL_ADDRESS_MASK
        } else {
            return Err(TableError::Collision { level });
        };
    }
    Ok(table)
}

/// Map `len` bytes at `pa` to `va` through `transtab`, one 4 KiB page at a time, allocating
/// intermediate tables from `arena`. Only pages are produced; panthor's 2 MiB blocks are not
/// needed by a firmware image whose sections are page-granular. — TIMBERDOODLE
pub fn map_range(
    memory: &mut impl TableMemory,
    arena: &mut TableArena,
    transtab: u64,
    va: u64,
    pa: u64,
    len: u64,
    attrs: PageAttrs,
) -> Result<(), TableError> {
    let Some(end) = va.checked_add(len) else {
        return Err(TableError::Range { va, len });
    };
    if len == 0
        || va % PAGE_SIZE != 0
        || pa % PAGE_SIZE != 0
        || len % PAGE_SIZE != 0
        || end > (1u64 << 48)
        || pa.checked_add(len).is_none()
    {
        return Err(TableError::Range { va, len });
    }
    let mut offset = 0u64;
    while offset < len {
        let page_va = va + offset;
        let table = resolve_table(memory, arena, transtab, page_va, 3)?;
        memory.write64(
            table + table_index(page_va, 3) as u64 * 8,
            page_descriptor(pa + offset, attrs.cached, attrs.readonly, !attrs.executable),
        );
        offset += PAGE_SIZE;
    }
    Ok(())
}

impl<'a> Firmware<'a> {
    /// The sections G3b loads: panthor skips PROT entries, and a zero-length VA range allocates
    /// nothing (RESEARCH/009 §8.4).
    pub fn loadable(&self) -> impl Iterator<Item = FirmwareSection<'a>> {
        self.sections()
            .filter(|section| !section.flags.contains(SectionFlags::PROT) && section.len != 0)
    }
}

// RESEARCH/009 §4.5: the JOB block (INTID 124).
const JOB_INT_RAWSTAT: u32 = 0x1000;
const JOB_INT_CLEAR: u32 = 0x1004;
const JOB_INT_MASK: u32 = 0x1008;
const JOB_INT_STAT: u32 = 0x100c;
pub const JOB_IRQ_GLOBAL_IF: u32 = 1 << 31;
pub const JOB_IRQ: u32 = IRQ_BASE;

/// JOB_INT bit for an event on CSG slot `slot` (§4.5).
pub const fn job_irq_csg(slot: u32) -> u32 {
    1 << slot
}

/// RESEARCH/009 §4.5: JOB_INT_RAWSTAT bit g is an event on CSG slot g, bit 31 the global
/// interface. The mask stays 0 after the boot, so reading it asserts nothing.
pub fn job_irq_rawstat(io: &mut impl MmioIo) -> u32 {
    io.read32(JOB_INT_RAWSTAT)
}

pub fn clear_job_irqs(io: &mut impl MmioIo, bits: u32) {
    io.write32(JOB_INT_CLEAR, bits);
}

/// RESEARCH/009 §4.9: the completed-flush counter a job's FLUSH_CACHE2 compares against. Read only
/// while the GPU is powered; a powered-down GPU here can hang the bus.
const LATEST_FLUSH_ID: u32 = 0x1_0000;

pub fn latest_flush_id(io: &mut impl MmioIo) -> u32 {
    io.read32(LATEST_FLUSH_ID)
}

/// What the JOB block showed when one wait ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobIrq {
    pub completion: Completion,
    pub rawstat: u32,
    pub stat: u32,
}

/// RESEARCH/009 §11.5 step 1 around one wait: arm the interrupt wait, unmask `bits`, run
/// `submit` (the doorbell, or nothing when waiting again), wait, then clear what the JOB block
/// raised and re-mask it before acknowledging the interrupt. The line is level-triggered, so the
/// device must be quiet before the SPI is unmasked again, the discipline `mcu_boot` already
/// follows. `submit` runs only after the wait is armed, so a fast completion cannot fire into
/// nothing. — TIMBERDOODLE
pub fn await_job_irq<I: MmioIo + CompletionIo>(
    io: &mut I,
    bits: u32,
    timeout_ns: u64,
    submit: impl FnOnce(&mut I),
) -> Result<JobIrq, Status> {
    io.arm_completion(JOB_IRQ, timeout_ns)?;
    io.write32(JOB_INT_MASK, bits);
    submit(io);
    let completion = io.wait_completion();
    let rawstat = io.read32(JOB_INT_RAWSTAT);
    let stat = io.read32(JOB_INT_STAT);
    io.write32(JOB_INT_CLEAR, rawstat);
    io.write32(JOB_INT_MASK, 0);
    io.complete();
    Ok(JobIrq {
        completion,
        rawstat,
        stat,
    })
}

/// How a job's interrupt-driven wait ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobWait {
    /// The JOB interrupt fired and the seqno had reached the job: the intended path.
    Interrupt,
    /// The interrupt fired, but the seqno only showed up after a later wait timed out: the event
    /// overtook the sync object's write.
    LateSeqno,
    /// The seqno reached the job, but no wait ever fired with the group's bit.
    NoInterrupt,
    /// The deadline passed with the seqno short of the job.
    Stalled,
}

/// The decision table for one job: `reached` is whether the seqno got to the job, `last` how
/// the final wait ended, `fired` how many waits fired with the group's bit set.
pub const fn classify_job_wait(reached: bool, last: Completion, fired: u32) -> JobWait {
    match (reached, last, fired) {
        (false, _, _) => JobWait::Stalled,
        (true, Completion::Fired, 1..) => JobWait::Interrupt,
        (true, _, 0) => JobWait::NoInterrupt,
        (true, _, _) => JobWait::LateSeqno,
    }
}

/// RESEARCH/009 §8.6: panthor gives the firmware boot one second.
pub const BOOT_TIMEOUT_NS: u64 = 1_000_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootObserved {
    pub completion: Completion,
    pub rawstat: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootError {
    /// The boot wait could not be armed; the MCU was not started.
    Arm(Status),
    /// No global-interface interrupt, and `JOB_INT_STAT` does not show it either.
    Timeout { rawstat: u32, mcu_status: u32 },
    /// The wait fired, but neither the raw nor the masked status shows the global bit.
    UnexpectedCompletion { rawstat: u32, stat: u32 },
}

/// RESEARCH/009 §8.6 steps 1-4: clear and unmask the JOB block, arm the boot wait **before**
/// starting the MCU, and require the global-interface bit, with panthor's `JOB_INT_STAT`
/// recheck after the timeout. The line is quieted afterwards; G4 takes over the per-group
/// interrupts. — TIMBERDOODLE
pub fn mcu_boot(io: &mut (impl MmioIo + CompletionIo)) -> Result<BootObserved, BootError> {
    io.arm_completion(JOB_IRQ, BOOT_TIMEOUT_NS)
        .map_err(BootError::Arm)?;
    io.write32(JOB_INT_CLEAR, u32::MAX);
    io.write32(JOB_INT_MASK, u32::MAX);
    io.write32(MCU_CONTROL, MCU_CONTROL_AUTO);
    let completion = io.wait_completion();
    let rawstat = io.read32(JOB_INT_RAWSTAT);
    let stat = io.read32(JOB_INT_STAT);
    io.write32(JOB_INT_CLEAR, JOB_IRQ_GLOBAL_IF);
    io.write32(JOB_INT_MASK, 0);
    io.complete();
    if rawstat & JOB_IRQ_GLOBAL_IF != 0 || stat & JOB_IRQ_GLOBAL_IF != 0 {
        return Ok(BootObserved {
            completion,
            rawstat,
        });
    }
    match completion {
        Completion::Fired => Err(BootError::UnexpectedCompletion { rawstat, stat }),
        Completion::TimedOut => Err(BootError::Timeout {
            rawstat,
            mcu_status: io.read32(MCU_STATUS),
        }),
    }
}

/// RESEARCH/009 §9.3: the global control block at the start of the shared section, in the
/// firmware's MCU address space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GlobalInterface {
    pub version: u32,
    pub features: u32,
    pub input_va: u64,
    pub output_va: u64,
    pub group_num: u32,
    pub group_stride: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GlbError {
    /// Version 0 means the firmware never wrote the interface.
    NotBooted,
    InputOutside,
    OutputOutside,
    GroupCount {
        group_num: u32,
    },
}

impl GlobalInterface {
    /// Validate the control words against the shared section (RESEARCH/009 §8.6 step 5/§9.3).
    pub fn read(control: [u32; 8], shared_va: u64, shared_len: u64) -> Result<Self, GlbError> {
        let version = control[0];
        if version == 0 {
            return Err(GlbError::NotBooted);
        }
        let inside = |va: u64| va >= shared_va && va < shared_va.saturating_add(shared_len);
        let input_va = u64::from(control[2]);
        let output_va = u64::from(control[3]);
        if !inside(input_va) {
            return Err(GlbError::InputOutside);
        }
        if !inside(output_va) {
            return Err(GlbError::OutputOutside);
        }
        let group_num = control[4];
        if !(3..=31).contains(&group_num) {
            return Err(GlbError::GroupCount { group_num });
        }
        Ok(Self {
            version,
            features: control[1],
            input_va,
            output_va,
            group_num,
            group_stride: control[5],
        })
    }

    pub const fn major(self) -> u8 {
        (self.version >> 24) as u8
    }

    pub const fn minor(self) -> u8 {
        (self.version >> 16) as u8
    }

    pub const fn patch(self) -> u16 {
        self.version as u16
    }
}

/// The address-space-0 fault currently latched in `MMU_INT_RAWSTAT`, if any (§4.5/§5.1).
pub fn latched_fault(io: &mut impl MmioIo) -> Option<Fault> {
    if io.read32(MMU_INT_RAWSTAT) & MMU_AS0_FAULT_BIT == 0 {
        return None;
    }
    let raw = io.read32(AS_FAULTSTATUS);
    let address = read64(io, AS_FAULTADDRESS, AS_FAULTADDRESS + 4);
    Some(decode_fault(raw, address))
}

/// The MMU block's raw status: bit n is a fault in address space n, bit 16+n a command
/// completion (§4.5).
pub fn mmu_rawstat(io: &mut impl MmioIo) -> u32 {
    io.read32(MMU_INT_RAWSTAT)
}

/// One address space's latched fault registers, whether or not its rawstat bit is set.
pub fn read_fault_as(io: &mut impl MmioIo, space: u32) -> Option<Fault> {
    let window = as_window(space)?;
    let raw = io.read32(window + as_offset(AS_FAULTSTATUS));
    let address_lo = window + as_offset(AS_FAULTADDRESS);
    let address = read64(io, address_lo, address_lo + 4);
    Some(decode_fault(raw, address))
}

// ---- G4a: the global interface (RESEARCH/009 §9.2-§9.4) ----

/// GLB input-block offsets (§9.3), relative to the control block's `input_va`.
pub const GLB_REQ: u32 = 0x00;
pub const GLB_ACK_IRQ_MASK: u32 = 0x04;
pub const GLB_PROGRESS_TIMER: u32 = 0x10;
pub const GLB_POWEROFF_TIMER: u32 = 0x14;
pub const GLB_CORE_EN_MASK: u32 = 0x18;
pub const GLB_IDLE_TIMER: u32 = 0x80;
/// GLB output-block offset of the acknowledge word (§9.3).
pub const GLB_ACK: u32 = 0x00;
/// Doorbell 0 rings the global interface; doorbells start at `0x8_0000` and sit 64 KiB apart
/// (RESEARCH/009 §4.10).
pub const GLB_DOORBELL: u32 = 0x0008_0000;

/// GLB request/ack bits (§9.3).
pub const GLB_REQ_CFG_PROGRESS_TIMER: u32 = 1 << 1;
pub const GLB_REQ_CFG_ALLOC_EN: u32 = 1 << 2;
pub const GLB_REQ_CFG_POWEROFF_TIMER: u32 = 1 << 3;
pub const GLB_REQ_COUNTER_EN: u32 = 1 << 7;
pub const GLB_REQ_PING: u32 = 1 << 8;
pub const GLB_REQ_IDLE_EN: u32 = 1 << 10;

/// panthor's initial acknowledge mask: the three config toggles, PING, IDLE_EN and the IDLE
/// event (§9.4).
pub const GLB_INIT_ACK_IRQ_MASK: u32 = GLB_REQ_CFG_PROGRESS_TIMER
    | GLB_REQ_CFG_ALLOC_EN
    | GLB_REQ_CFG_POWEROFF_TIMER
    | GLB_REQ_PING
    | GLB_REQ_IDLE_EN
    | (1 << 26);
/// GLB events (§9.3, event mask [26:20]): the firmware toggles ACK, the host copies it into REQ.
/// IDLE follows `idle_timer` (800 µs) with no work, and is in the init acknowledge mask, so it
/// raises JOB IRQ bit 31.
pub const GLB_IDLE: u32 = 1 << 26;
pub const GLB_EVENTS: u32 = 0x7F << 20;

/// RESEARCH/009 §11.5 step 2: the global interface's raised, unacknowledged events.
pub fn glb_pending_events(io: &mut impl GlbIo) -> u32 {
    (io.read_input(GLB_REQ) ^ io.read_output(GLB_ACK)) & GLB_EVENTS
}

/// Acknowledge global `events` by copying their ACK bits into REQ; request toggles in flight
/// (PING, the config bits) are left alone. Like panthor's IDLE handling, no doorbell follows.
pub fn ack_glb_events(io: &mut impl GlbIo, events: u32) {
    let mask = events & GLB_EVENTS;
    let req = io.read_input(GLB_REQ);
    let ack = io.read_output(GLB_ACK);
    io.write_input(GLB_REQ, (req & !mask) | (ack & mask));
}

/// panthor's progress timeout in the firmware's 1024-tick units (§9.4).
pub const GLB_PROGRESS_TIMEOUT: u32 = 2_560_000;
/// panthor gives a ping 100 ms to be acknowledged (§8.7).
pub const GLB_PING_TIMEOUT_NS: u64 = 100_000_000;

/// The driver's window on the firmware's shared section and the global doorbell. The input and
/// output blocks are Normal-NC memory the firmware also accesses; `ring_doorbell` must order the
/// input stores before its Device write.
pub trait GlbIo {
    fn read_input(&mut self, offset: u32) -> u32;
    fn write_input(&mut self, offset: u32, value: u32);
    fn read_output(&mut self, offset: u32) -> u32;
    fn ring_doorbell(&mut self);
}

/// RESEARCH/009 §9.3: a timer value is `ceil(us * rate / (10^6 * 1024))` in the low 30 bits. Bit
/// 31 would select the GPU cycle counter instead of the system timer; KUMO uses the latter.
pub fn glb_timer_ticks(us: u32, rate_hz: u32) -> u32 {
    if rate_hz == 0 {
        return 0;
    }
    let numerator = u64::from(us) * u64::from(rate_hz);
    let denominator = 1_000_000u64 * 1024;
    let ticks = (numerator + denominator - 1) / denominator;
    ticks.min(0x7fff_ffff) as u32
}

/// A toggle request is pending while its REQ bit differs from the ACK bit: set it to the opposite
/// of the firmware's current acknowledge (§9.2).
fn with_toggle(req: u32, ack: u32, bit: u32) -> u32 {
    let value = if ack & bit == 0 { bit } else { 0 };
    (req & !bit) | value
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GlbReport {
    pub req: u32,
    pub ack: u32,
    pub core_en_mask: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GlbInitError {
    /// The acknowledge never mirrored the PING request within the timeout.
    PingTimeout { req: u32, ack: u32 },
}

/// RESEARCH/009 §9.4 plus §9.2's liveness check: program the core mask, timers and acknowledge
/// mask, set the IDLE_EN/COUNTER_EN levels, toggle the three config requests and PING from the
/// firmware's current acknowledge, ring doorbell 0, and require PING to be mirrored within
/// `timeout_ns`. The final acknowledge is returned for the boot report. — TIMBERDOODLE
pub fn init_global_interface(
    io: &mut (impl GlbIo + ClockIo),
    shader_present: u64,
    timer_hz: u32,
    timeout_ns: u64,
) -> Result<GlbReport, GlbInitError> {
    io.write_input(GLB_CORE_EN_MASK, shader_present as u32);
    io.write_input(GLB_CORE_EN_MASK + 4, (shader_present >> 32) as u32);
    io.write_input(GLB_PROGRESS_TIMER, GLB_PROGRESS_TIMEOUT);
    io.write_input(GLB_POWEROFF_TIMER, glb_timer_ticks(10_000, timer_hz));
    io.write_input(GLB_IDLE_TIMER, glb_timer_ticks(800, timer_hz));
    io.write_input(GLB_ACK_IRQ_MASK, GLB_INIT_ACK_IRQ_MASK);
    let ack = io.read_output(GLB_ACK);
    let mut req = io.read_input(GLB_REQ) | GLB_REQ_IDLE_EN | GLB_REQ_COUNTER_EN;
    req = with_toggle(req, ack, GLB_REQ_CFG_ALLOC_EN);
    req = with_toggle(req, ack, GLB_REQ_CFG_PROGRESS_TIMER);
    req = with_toggle(req, ack, GLB_REQ_CFG_POWEROFF_TIMER);
    req = with_toggle(req, ack, GLB_REQ_PING);
    io.write_input(GLB_REQ, req);
    io.ring_doorbell();
    let deadline = io.now_ns().saturating_add(timeout_ns);
    loop {
        let ack = io.read_output(GLB_ACK);
        if (ack ^ req) & GLB_REQ_PING == 0 {
            return Ok(GlbReport {
                req,
                ack,
                core_en_mask: shader_present,
            });
        }
        if io.now_ns() >= deadline {
            return Err(GlbInitError::PingTimeout { req, ack });
        }
        io.delay_us(10);
    }
}

// ---- G4b: the first command-stream group (RESEARCH/009 §9.5-§9.7, §10, §11) ----

/// GLB `doorbell_req` (input) and `doorbell_ack` (output): one bit per CSG slot (§9.3).
pub const GLB_DOORBELL_REQ: u32 = 0x08;
/// A CSG's control block sits at `shared + 0x1000 + g*group_stride`; its first stream's control
/// block is `0x40` bytes further in (§9.1).
pub const CSG_CONTROL_BASE: u32 = 0x1000;
pub const CSG_STREAM_CONTROL_OFFSET: u32 = 0x40;

/// Doorbell *n* lives at `0x8_0000 + n*0x1_0000`; doorbell 0 is the global interface and a queue
/// in CSG slot g uses g + 1 (§4.10).
pub const fn csf_doorbell(index: u32) -> u32 {
    0x0008_0000 + index * 0x0001_0000
}

/// Firmware-VA access to the MCU's memory. The driver translates addresses through the windows it
/// mapped; host tests use a fake store. — TIMBERDOODLE
pub trait FirmwareMemory {
    fn read32(&mut self, mcu: u64) -> u32;
    fn write32(&mut self, mcu: u64, value: u32);
}

fn read_mcu64(mem: &mut impl FirmwareMemory, mcu: u64) -> u64 {
    u64::from(mem.read32(mcu)) | (u64::from(mem.read32(mcu + 4)) << 32)
}

fn write_mcu64(mem: &mut impl FirmwareMemory, mcu: u64, value: u64) {
    mem.write32(mcu, value as u32);
    mem.write32(mcu + 4, (value >> 32) as u32);
}

/// CS/CSG request and config fields (§9.5/§9.6).
pub const CS_STATE_MASK: u32 = 0b111;
pub const CS_STATE_START: u32 = 1;
pub const CS_EXTRACT_EVENT: u32 = 1 << 4;
pub const CS_IDLE_SYNC_WAIT: u32 = 1 << 8;
pub const CS_IDLE_EMPTY: u32 = 1 << 10;
pub const CSG_STATE_MASK: u32 = 0b111;
pub const CSG_STATE_START: u32 = 1;
/// Re-apply the endpoint configuration; a toggle, and part of every group start (§9.5).
pub const CSG_ENDPOINT_CONFIG: u32 = 1 << 4;
/// CSG events: the firmware toggles the ACK bit, the host acknowledges by copying it into REQ
/// (§9.5). SYNC_UPDATE follows a sync-object change such as the §10.6 wrapper's add.
pub const CSG_SYNC_UPDATE: u32 = 1 << 28;
pub const CSG_IDLE: u32 = 1 << 29;
pub const CSG_PROGRESS_TIMER_EVENT: u32 = 1 << 31;
/// The events §11.5 step 3 acknowledges by copy.
pub const CSG_EVENTS: u32 = CSG_SYNC_UPDATE | CSG_IDLE | CSG_PROGRESS_TIMER_EVENT;

/// A CSG slot's read-only control block (§9.5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CsgControl {
    pub features: u32,
    pub input_va: u64,
    pub output_va: u64,
    pub suspend_size: u32,
    pub protm_suspend_size: u32,
    pub stream_num: u32,
    pub stream_stride: u32,
}

/// A stream slot's read-only control block (§9.6).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CsControl {
    pub features: u32,
    pub input_va: u64,
    pub output_va: u64,
}

pub fn read_csg_control(mem: &mut impl FirmwareMemory, addr: u64) -> CsgControl {
    CsgControl {
        features: mem.read32(addr),
        input_va: u64::from(mem.read32(addr + 0x04)),
        output_va: u64::from(mem.read32(addr + 0x08)),
        suspend_size: mem.read32(addr + 0x0C),
        protm_suspend_size: mem.read32(addr + 0x10),
        stream_num: mem.read32(addr + 0x14),
        stream_stride: mem.read32(addr + 0x18),
    }
}

pub fn read_cs_control(mem: &mut impl FirmwareMemory, addr: u64) -> CsControl {
    CsControl {
        features: mem.read32(addr),
        input_va: u64::from(mem.read32(addr + 0x04)),
        output_va: u64::from(mem.read32(addr + 0x08)),
    }
}

/// What a queue slot needs to run (§9.6 including §11.2's initial values).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueueConfig {
    /// GPU virtual address of the ring in the group's address space.
    pub ring_va: u64,
    pub ring_size: u64,
    /// MCU addresses of the queue's input and output pages (§9.7).
    pub ring_input_va: u64,
    pub ring_output_va: u64,
    pub doorbell: u32,
    pub priority: u32,
}

pub fn program_queue(mem: &mut impl FirmwareMemory, input_va: u64, queue: QueueConfig) {
    mem.write32(
        input_va,
        CS_STATE_START | CS_EXTRACT_EVENT | CS_IDLE_SYNC_WAIT | CS_IDLE_EMPTY,
    );
    mem.write32(
        input_va + 0x04,
        (queue.priority & 0xF) | ((queue.doorbell << 8) & 0xFF00),
    );
    mem.write32(input_va + 0x0C, u32::MAX);
    write_mcu64(mem, input_va + 0x10, queue.ring_va);
    mem.write32(input_va + 0x18, queue.ring_size as u32);
    write_mcu64(mem, input_va + 0x30, queue.ring_input_va);
    write_mcu64(mem, input_va + 0x38, queue.ring_output_va);
}

/// What a CSG slot needs to run (§9.5 including §11.2's initial values).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupConfig {
    pub compute_mask: u64,
    pub fragment_mask: u64,
    pub tiler_mask: u64,
    pub max_compute: u32,
    pub max_fragment: u32,
    pub max_tiler: u32,
    pub priority: u32,
    /// The address space the group's GPU VAs live in (§11.2's `config`).
    pub address_space: u32,
    pub suspend_va: u64,
    pub protm_suspend_va: u64,
    /// One bit per programmed queue, toggled into `doorbell_req` (§9.2).
    pub queue_mask: u32,
}

pub fn program_group(
    mem: &mut impl FirmwareMemory,
    input_va: u64,
    output_va: u64,
    group: GroupConfig,
) {
    mem.write32(input_va + 0x04, u32::MAX);
    write_mcu64(mem, input_va + 0x20, group.compute_mask);
    write_mcu64(mem, input_va + 0x28, group.fragment_mask);
    mem.write32(input_va + 0x30, group.tiler_mask as u32);
    mem.write32(
        input_va + 0x34,
        (group.max_compute & 0xFF)
            | ((group.max_fragment & 0xFF) << 8)
            | ((group.max_tiler & 0xF) << 16)
            | ((group.priority & 0xF) << 28),
    );
    write_mcu64(mem, input_va + 0x40, group.suspend_va);
    write_mcu64(mem, input_va + 0x48, group.protm_suspend_va);
    mem.write32(input_va + 0x50, group.address_space);
    let req = mem.read32(input_va + 0x08);
    let ack = mem.read32(output_va + 0x08);
    mem.write32(
        input_va + 0x08,
        (req & !group.queue_mask) | (!ack & group.queue_mask),
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroupError {
    /// The CSG acknowledge never showed the start state.
    AckTimeout { req: u32, ack: u32 },
}

/// RESEARCH/009 §11.2 step 3: set the group's STATE to start, toggle its GLB doorbell-request
/// bit, ring doorbell 0, and require the CSG acknowledge to show the start state. — TIMBERDOODLE
pub fn start_group(
    io: &mut (impl FirmwareMemory + GlbIo + ClockIo),
    csg_input_va: u64,
    csg_output_va: u64,
    slot: u32,
    timeout_ns: u64,
) -> Result<u32, GroupError> {
    // panthor's start queues both the state change and an ENDPOINT_CONFIG toggle, then waits for
    // every queued bit to be acknowledged. Without the endpoint toggle the firmware never
    // acknowledges the start (metal, J582). — TIMBERDOODLE
    let ack = io.read32(csg_output_va);
    let mut req = (io.read32(csg_input_va) & !CSG_STATE_MASK) | CSG_STATE_START;
    req = (req & !CSG_ENDPOINT_CONFIG) | ((ack ^ CSG_ENDPOINT_CONFIG) & CSG_ENDPOINT_CONFIG);
    io.write32(csg_input_va, req);
    let bit = 1u32 << slot;
    let doorbell_req = io.read_input(GLB_DOORBELL_REQ);
    let doorbell_ack = io.read_output(GLB_DOORBELL_REQ);
    io.write_input(
        GLB_DOORBELL_REQ,
        (doorbell_req & !bit) | (!doorbell_ack & bit),
    );
    io.ring_doorbell();
    let mask = CSG_STATE_MASK | CSG_ENDPOINT_CONFIG;
    let deadline = io.now_ns().saturating_add(timeout_ns);
    loop {
        let ack = io.read32(csg_output_va);
        if ack & mask == req & mask {
            return Ok(ack);
        }
        if io.now_ns() >= deadline {
            return Err(GroupError::AckTimeout { req, ack });
        }
        io.delay_us(10);
    }
}

/// The queue's two interface pages (§9.7).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueueIface {
    pub input_va: u64,
    pub output_va: u64,
}

impl QueueIface {
    /// Both interface pages from one base, one page apart (§9.7). The firmware's ringbuf fields
    /// and the host's `insert` publication take this same value, so they cannot diverge. — TIMBERDOODLE
    pub const fn at(base: u64) -> Self {
        Self {
            input_va: base,
            output_va: base + PAGE_SIZE,
        }
    }
}

pub fn queue_insert(mem: &mut impl FirmwareMemory, iface: QueueIface) -> u64 {
    read_mcu64(mem, iface.input_va)
}

/// RESEARCH/009 §11.3: mirror the firmware's `extract` into the input page, then advance
/// `insert`. The caller writes the ring and issues its barrier before calling this.
pub fn publish_insert(mem: &mut impl FirmwareMemory, iface: QueueIface, insert: u64) {
    let extract = read_mcu64(mem, iface.output_va);
    write_mcu64(mem, iface.input_va + 0x08, extract);
    write_mcu64(mem, iface.input_va, insert);
}

/// RESEARCH/009 §9.2's stream-doorbell path: toggle the stream's `doorbell_req` bit in the
/// group, toggle the group's GLB bit, and ring doorbell 0. Panthor's fast path rings only the
/// queue's own doorbell (§11.3); KUMO rings both so the firmware cannot miss new work. — TIMBERDOODLE
pub fn wake_stream(
    io: &mut (impl FirmwareMemory + GlbIo),
    csg_input_va: u64,
    csg_output_va: u64,
    slot: u32,
    stream: u32,
) {
    let stream_bit = 1u32 << stream;
    let req = io.read32(csg_input_va + 0x08);
    let ack = io.read32(csg_output_va + 0x08);
    io.write32(
        csg_input_va + 0x08,
        (req & !stream_bit) | (!ack & stream_bit),
    );
    let slot_bit = 1u32 << slot;
    let glb_req = io.read_input(GLB_DOORBELL_REQ);
    let glb_ack = io.read_output(GLB_DOORBELL_REQ);
    io.write_input(
        GLB_DOORBELL_REQ,
        (glb_req & !slot_bit) | (!glb_ack & slot_bit),
    );
    io.ring_doorbell();
}

/// RESEARCH/009 §9.7: the firmware's consumption pointer from the queue's output page.
pub fn queue_extract(mem: &mut impl FirmwareMemory, iface: QueueIface) -> u64 {
    read_mcu64(mem, iface.output_va)
}

/// RESEARCH/009 §11.5: the group's per-stream event level, held until acknowledged.
pub fn cs_irq_req(mem: &mut impl FirmwareMemory, csg_output_va: u64) -> u32 {
    mem.read32(csg_output_va + 0x0C)
}

/// RESEARCH/009 §11.5: acknowledge every raised stream event the caller has seen. panthor copies
/// `cs_irq_req` into `cs_irq_ack` before reading the streams, so an event that arrives meanwhile
/// is not lost; this is that copy. — TIMBERDOODLE
pub fn ack_cs_irqs(mem: &mut impl FirmwareMemory, csg_input_va: u64, req: u32) {
    mem.write32(csg_input_va + 0x0C, req);
}

/// What one pass of §11.5's acknowledgements found pending.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EventsAcked {
    pub glb: u32,
    pub cs_irq_req: u32,
    pub cs_irq_ack: u32,
    pub csg: u32,
}

impl EventsAcked {
    pub const fn any(self) -> bool {
        self.glb != 0 || self.cs_irq_req != self.cs_irq_ack || self.csg != 0
    }
}

/// RESEARCH/009 §11.5 steps 2-3 for one group: acknowledge the global events, copy the stream
/// level, then acknowledge the group's events, and say what was pending. Afterwards the
/// interface holds no unacknowledged event. That is what lets the firmware raise the next JOB
/// interrupt: on metal (J589's first run) a group whose IDLE from an earlier stream was still
/// pending raised no JOB interrupt for a new SYNC_UPDATE. — TIMBERDOODLE
pub fn ack_group_events(
    io: &mut (impl FirmwareMemory + GlbIo),
    csg_input_va: u64,
    csg_output_va: u64,
) -> EventsAcked {
    let glb = glb_pending_events(io);
    ack_glb_events(io, glb);
    let cs_irq_req = cs_irq_req(io, csg_output_va);
    let cs_irq_ack = io.read32(csg_input_va + 0x0C);
    ack_cs_irqs(io, csg_input_va, cs_irq_req);
    let csg = csg_pending_events(io, csg_input_va, csg_output_va);
    ack_csg_events(io, csg_input_va, csg_output_va, csg);
    EventsAcked {
        glb,
        cs_irq_req,
        cs_irq_ack,
        csg,
    }
}

/// RESEARCH/009 §11.5 step 1: the group's raised, unacknowledged events, `(req ^ ack)` under
/// [`CSG_EVENTS`].
pub fn csg_pending_events(
    mem: &mut impl FirmwareMemory,
    csg_input_va: u64,
    csg_output_va: u64,
) -> u32 {
    (mem.read32(csg_input_va) ^ mem.read32(csg_output_va)) & CSG_EVENTS
}

/// RESEARCH/009 §11.5 step 3: acknowledge `events` by copying their ACK bits into REQ. The STATE
/// level and the ENDPOINT_CONFIG toggle outside the mask are left as the host last wrote them.
pub fn ack_csg_events(
    mem: &mut impl FirmwareMemory,
    csg_input_va: u64,
    csg_output_va: u64,
    events: u32,
) {
    let mask = events & CSG_EVENTS;
    let req = mem.read32(csg_input_va);
    let ack = mem.read32(csg_output_va);
    mem.write32(csg_input_va, (req & !mask) | (ack & mask));
}

/// The byte position inside the ring for a monotonically growing insert (§9.7).
pub fn ring_offset(insert: u64, size: u64) -> usize {
    (insert & (size - 1)) as usize
}

/// The command-stream words for RESEARCH/009 §10.7's first stream: write `target` with
/// 0x00C0FFEE, flush the caches, and wait for both completions. — TIMBERDOODLE
pub fn first_stream(target: u64) -> [u64; 6] {
    store_stream(target, 0x00C0_FFEE)
}

/// §10.7's stream for any value: store it at `target`, flush, wait. It touches only r0-r3, well
/// below the wrapper's scratch quad. J586-J589 named r0 (the target's low word) as the flush-ID
/// register; that is no flush ID, and J588's counter rose by one per job for two flushes. panvk
/// gives every in-stream flush the ID 0 (`cs_move32_to(b, flush_id, 0)`), and so does this.
pub fn store_stream(target: u64, value: u32) -> [u64; 6] {
    [
        move48(0, target),
        move32(2, value),
        sync_set32(0, 0, 2),
        move32(3, 0),
        flush_cache2(0, 3),
        wait_slot(1),
    ]
}

/// SYNC_SET32: store a register to an address pair with system scope; signal on completion
/// (§10.3; error propagation off).
pub fn sync_set32(signal_slot: u8, address_pair: u8, data_reg: u8) -> u64 {
    (38u64 << 56)
        | (u64::from(signal_slot) << 48)
        | (u64::from(address_pair) << 40)
        | (u64::from(data_reg) << 32)
}

/// FLUSH_CACHE2: L2 clean+invalidate, LSC clean+invalidate, other invalidate (§4.4's 0x233),
/// skipped by the hardware if a flush has completed since the ID in `flush_id_reg` (§4.9).
pub fn flush_cache2(signal_slot: u8, flush_id_reg: u8) -> u64 {
    flush_caches(signal_slot, flush_id_reg, 0x233)
}

/// STORE_MULTIPLE: store the registers `base_reg + i` for each bit i of `mask` at the address
/// pair plus a signed byte offset (Mesa `v10.xml`). The sources may be read after issue: Mesa's
/// load/store tracker forbids overwriting a store's registers before a WAIT on the load/store
/// scoreboard.
pub fn store_multiple(base_reg: u8, address_pair: u8, mask: u16, offset: i16) -> u64 {
    (21u64 << 56)
        | (u64::from(base_reg) << 48)
        | (u64::from(address_pair) << 40)
        | (u64::from(mask) << 16)
        | u64::from(offset as u16)
}

/// ADD_IMMEDIATE32: `destination = source + immediate` (signed), one register (§10.3).
pub fn add_imm32(destination: u8, source: u8, immediate: i32) -> u64 {
    (16u64 << 56)
        | (u64::from(destination) << 48)
        | (u64::from(source) << 40)
        | u64::from(immediate as u32)
}

/// ADD_IMMEDIATE64: the same on register pairs, the immediate sign-extended (§10.3).
pub fn add_imm64(destination_pair: u8, source_pair: u8, immediate: i32) -> u64 {
    (17u64 << 56)
        | (u64::from(destination_pair) << 48)
        | (u64::from(source_pair) << 40)
        | u64::from(immediate as u32)
}

/// BRANCH conditions (§10.4): the value register is tested against zero.
pub const CS_COND_NEQUAL: u8 = 3;
pub const CS_COND_ALWAYS: u8 = 6;

/// BRANCH: when the 32-bit `value_reg` meets `condition`, continue `offset` instructions after
/// the branch's successor. Mesa computes `offset = target - branch_position - 1`.
pub fn branch(condition: u8, value_reg: u8, offset: i16) -> u64 {
    (22u64 << 56)
        | (u64::from(value_reg) << 40)
        | (u64::from(condition & 0xF) << 28)
        | u64::from(offset as u16)
}

/// CALL: run `length_reg` bytes of stream at the address pair, then return (§10.3).
pub fn call_stream(address_pair: u8, length_reg: u8) -> u64 {
    (32u64 << 56) | (u64::from(address_pair) << 40) | (u64::from(length_reg) << 32)
}

/// SYNC_ADD64: add a register pair to the 64-bit sync object at an address pair, system scope,
/// propagating the stream's error state into it; signal on completion (§10.3).
pub fn sync_add64(signal_slot: u8, address_pair: u8, data_pair: u8) -> u64 {
    (51u64 << 56)
        | (u64::from(signal_slot) << 48)
        | (u64::from(address_pair) << 40)
        | (u64::from(data_pair) << 32)
        | 1
}

/// ERROR_BARRIER: a fault before this point is recoverable at the job boundary (§10.3).
pub fn error_barrier() -> u64 {
    47u64 << 56
}

/// The top four registers are the kernel's scratch (§10.2, panthor's
/// `CSF_UNPRESERVED_REG_COUNT`).
pub const CS_UNPRESERVED_REGS: u32 = 4;
/// The command-stream prefetcher reads whole 64-byte lines (§10.6).
pub const CS_LINE_BYTES: u64 = 64;
/// One wrapper: eleven instructions, NOP-padded to two whole lines (§10.6).
pub const JOB_WRAPPER_WORDS: usize = 16;
pub const JOB_WRAPPER_BYTES: u64 = JOB_WRAPPER_WORDS as u64 * 8;
/// A per-queue sync object: `seqno` (64), `status` (32; non-zero means the job failed), padding
/// (§11.4).
pub const SYNC_OBJECT_BYTES: u64 = 16;
pub const SYNC_STATUS_OFFSET: u64 = 8;

/// One job for the wrapper to run (§10.6).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobCall {
    /// LATEST_FLUSH_ID, sampled after the stream's inputs were written (§4.9).
    pub latest_flush: u32,
    /// GPU VA and length in bytes of the stream the wrapper CALLs.
    pub stream_va: u64,
    pub stream_len: u32,
    /// GPU VA of the queue's sync object.
    pub sync_va: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WrapperError {
    /// Too few registers for the scratch quad, or a quad that does not start on a pair.
    Registers(u32),
    /// No scoreboard to wait on, or more than WAIT's 16-bit mask can name.
    Scoreboards(u32),
    /// panthor's submit rule: a 64-byte-aligned stream of whole, non-zero instructions.
    Stream { va: u64, len: u32 },
    /// SYNC_ADD64 needs an 8-byte-aligned object.
    SyncObject(u64),
    /// panthor rejects a flush ID with bits 24-30 set.
    FlushId(u32),
}

/// RESEARCH/009 §10.6, panthor's `prepare_job_instrs` with profiling off: flush the caches the
/// job reads, CALL its stream, wait for every scoreboard, add 1 to the queue's sync object, and
/// end on an error barrier. The scratch quad rX..rX+3 is the top of the register file, so the
/// user stream's registers survive. — TIMBERDOODLE
pub fn job_wrapper(
    features: CsFeatures,
    call: JobCall,
) -> Result<[u64; JOB_WRAPPER_WORDS], WrapperError> {
    let regs = features.work_regs;
    if !(8..=256).contains(&regs) || regs % 2 != 0 {
        return Err(WrapperError::Registers(regs));
    }
    if !(1..=16).contains(&features.scoreboards) {
        return Err(WrapperError::Scoreboards(features.scoreboards));
    }
    let max_va = 0x0000_FFFF_FFFF_FFFF;
    if call.stream_va % CS_LINE_BYTES != 0
        || call.stream_len == 0
        || call.stream_len % 8 != 0
        || call.stream_va > max_va
    {
        return Err(WrapperError::Stream {
            va: call.stream_va,
            len: call.stream_len,
        });
    }
    if call.sync_va % 8 != 0 || call.sync_va > max_va {
        return Err(WrapperError::SyncObject(call.sync_va));
    }
    if call.latest_flush & 0x7F00_0000 != 0 {
        return Err(WrapperError::FlushId(call.latest_flush));
    }
    let addr = (regs - CS_UNPRESERVED_REGS) as u8;
    let val = addr + 2;
    let wait_all = ((1u32 << features.scoreboards) - 1) as u16;
    let instructions = [
        move32(val, call.latest_flush),
        flush_cache2(0, val),
        move48(addr, call.stream_va),
        move32(val, call.stream_len),
        wait_slot(1),
        call_stream(addr, val),
        move48(addr, call.sync_va),
        move48(val, 1),
        wait_slot(wait_all),
        sync_add64(0, addr, val),
        error_barrier(),
    ];
    let mut words = [0u64; JOB_WRAPPER_WORDS];
    words[..instructions.len()].copy_from_slice(&instructions);
    Ok(words)
}

/// G6b owns output visibility even when the client stream omits cache maintenance. The last
/// wrapper scratch register is zero (the high half of MOVE48 val=1): use it as the always-zero
/// flush ID, wait for the clean, then signal the queue seqno. — TIMBERDOODLE
pub fn client_job_wrapper(
    features: CsFeatures,
    call: JobCall,
) -> Result<[u64; JOB_WRAPPER_WORDS], WrapperError> {
    let mut words = job_wrapper(features, call)?;
    let sync = words[9];
    let barrier = words[10];
    let zero_reg = (features.work_regs - 1) as u8;
    words[9] = flush_caches(0, zero_reg, FLUSH_CLEAN);
    words[10] = wait_slot(1);
    words[11] = sync;
    words[12] = barrier;
    Ok(words)
}

/// Where one wrapper lands, as ring positions that only grow (§9.7).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobSlot {
    pub start: u64,
    pub end: u64,
}

/// The next wrapper starts on a whole line at or after `insert`, so the prefetcher never fetches
/// a line the host is still writing; a queue left mid-line (J586's 40-byte first stream) skips
/// ahead over NOPs. None when the job would overwrite words the firmware has not consumed.
pub fn job_slot(insert: u64, extract: u64, ring_size: u64) -> Option<JobSlot> {
    let start = insert.checked_add(CS_LINE_BYTES - 1)? & !(CS_LINE_BYTES - 1);
    let end = start + JOB_WRAPPER_BYTES;
    if extract > insert || end - extract > ring_size {
        return None;
    }
    Some(JobSlot { start, end })
}

/// Every ring word a job writes, as (byte offset in the ring, word): NOPs from `insert` to the
/// slot's start, then the wrapper, each wrapping round the ring's end the way panthor's
/// `copy_instrs_to_ringbuf` does.
pub fn write_job_words(
    insert: u64,
    slot: JobSlot,
    wrapper: &[u64; JOB_WRAPPER_WORDS],
    ring_size: u64,
    mut write: impl FnMut(usize, u64),
) {
    let mut position = insert & !7;
    while position < slot.start {
        write(ring_offset(position, ring_size), 0);
        position += 8;
    }
    for (index, word) in wrapper.iter().enumerate() {
        write(ring_offset(slot.start + index as u64 * 8, ring_size), *word);
    }
}

/// The stream's diagnostic words, read when it faults or stalls (§9.6).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CsStatus {
    pub cmd_ptr: u64,
    /// `status_wait`: bit 31 waiting on a sync object, [15:0] scoreboard mask.
    pub wait: u32,
    pub wait_sync_ptr: u64,
    pub wait_sync_value: u64,
    pub blocked_reason: u32,
    pub fault: u32,
    pub fatal: u32,
}

pub fn read_cs_status(mem: &mut impl FirmwareMemory, output_va: u64) -> CsStatus {
    CsStatus {
        cmd_ptr: read_mcu64(mem, output_va + 0x40),
        wait: mem.read32(output_va + 0x48),
        wait_sync_ptr: read_mcu64(mem, output_va + 0x50),
        wait_sync_value: u64::from(mem.read32(output_va + 0x58))
            | (u64::from(mem.read32(output_va + 0x64)) << 32),
        blocked_reason: mem.read32(output_va + 0x60),
        fault: mem.read32(output_va + 0x80),
        fatal: mem.read32(output_va + 0x84),
    }
}

// ---- G5a: a test pattern painted by the command stream (PLAN/013 G5) ----

const PAINT_BAR_WIDTH: u32 = PAINT_WIDTH / PAINT_BARS.len() as u32;
/// Sixteen 32-bit registers per STORE_MULTIPLE: 64 bytes, sixteen pixels.
const PAINT_CHUNK_PIXELS: u32 = 16;
const PAINT_CHUNKS_PER_ROW: u32 = PAINT_WIDTH / PAINT_CHUNK_PIXELS;
/// Registers: r0:r1 the address, r2-r65 the four bar colours (sixteen each, then r2-r17 the
/// ramp), r80/r81 the row and chunk counters, r82 the flush ID. All below the wrapper's r92-r95.
const PAINT_ADDR: u8 = 0;
const PAINT_COLOURS: u8 = 2;
const PAINT_ROWS: u8 = 80;
const PAINT_CHUNKS: u8 = 81;
const PAINT_FLUSH_ID: u8 = 82;
/// The stream is 176 instructions; the builder holds a little more.
pub const PAINT_STREAM_MAX: usize = 192;

/// A command stream with its length; built once, copied into GPU memory.
pub struct PaintStream {
    words: [u64; PAINT_STREAM_MAX],
    len: usize,
}

impl PaintStream {
    pub fn words(&self) -> &[u64] {
        &self.words[..self.len]
    }

    fn push(&mut self, word: u64) {
        self.words[self.len] = word;
        self.len += 1;
    }

    /// A conditional branch back to `target` (an instruction index), in Mesa's convention.
    fn branch_back(&mut self, condition: u8, value_reg: u8, target: usize) {
        let offset = target as i32 - self.len as i32 - 1;
        self.push(branch(condition, value_reg, offset as i16));
    }

    /// One 64-byte store, its WAIT, and the address step. The WAIT comes before the address
    /// (a store source) changes, per Mesa's load/store tracker rule.
    fn store_chunk(&mut self, base_reg: u8, wait_all: u16) {
        self.push(store_multiple(base_reg, PAINT_ADDR, 0xFFFF, 0));
        self.push(wait_slot(wait_all));
        self.push(add_imm64(PAINT_ADDR, PAINT_ADDR, 64));
    }
}

/// G5a's stream: paint [`paint_pixel`] into the 256x128 surface at `target` using only the
/// command-stream processor (STORE_MULTIPLE, ADD_IMMEDIATE and BRANCH loops, no shader), then
/// clean L2 and LSC with flush ID 0 as panvk does and wait for it. Every store is followed by a
/// WAIT on all scoreboards, so the stream needs no assumption about which slot the load/store
/// unit signals. — TIMBERDOODLE
pub fn paint_stream(target: u64, features: CsFeatures) -> PaintStream {
    let wait_all = match features.scoreboards {
        1..=15 => ((1u32 << features.scoreboards) - 1) as u16,
        _ => 0xFFFF,
    };
    let mut s = PaintStream {
        words: [0; PAINT_STREAM_MAX],
        len: 0,
    };
    s.push(move48(PAINT_ADDR, target));
    for (bar, colour) in PAINT_BARS.iter().enumerate() {
        for i in 0..PAINT_CHUNK_PIXELS as u8 {
            s.push(move32(
                PAINT_COLOURS + bar as u8 * PAINT_CHUNK_PIXELS as u8 + i,
                *colour,
            ));
        }
    }
    // Top half: each row is four bars of four chunks, each bar stored from its own registers.
    s.push(move32(PAINT_ROWS, PAINT_HEIGHT / 2));
    let bars = s.len;
    for bar in 0..PAINT_BARS.len() as u8 {
        for _ in 0..PAINT_BAR_WIDTH / PAINT_CHUNK_PIXELS {
            s.store_chunk(PAINT_COLOURS + bar * PAINT_CHUNK_PIXELS as u8, wait_all);
        }
    }
    s.push(add_imm32(PAINT_ROWS, PAINT_ROWS, -1));
    s.branch_back(CS_COND_NEQUAL, PAINT_ROWS, bars);
    // Bottom half: r(2+i) holds the pixel at x = 16c + i. Each chunk adds 16 to every register;
    // sixteen chunks add 0x100, which carries the blue byte back to i and green up by one, so
    // the row step adds another 0x100 for green = 2 * row.
    for i in 0..PAINT_CHUNK_PIXELS as u8 {
        s.push(move32(PAINT_COLOURS + i, u32::from(i)));
    }
    s.push(move32(PAINT_ROWS, PAINT_HEIGHT / 2));
    let rows = s.len;
    s.push(move32(PAINT_CHUNKS, PAINT_CHUNKS_PER_ROW));
    let chunks = s.len;
    s.store_chunk(PAINT_COLOURS, wait_all);
    for i in 0..PAINT_CHUNK_PIXELS as u8 {
        s.push(add_imm32(
            PAINT_COLOURS + i,
            PAINT_COLOURS + i,
            PAINT_CHUNK_PIXELS as i32,
        ));
    }
    s.push(add_imm32(PAINT_CHUNKS, PAINT_CHUNKS, -1));
    s.branch_back(CS_COND_NEQUAL, PAINT_CHUNKS, chunks);
    for i in 0..PAINT_CHUNK_PIXELS as u8 {
        s.push(add_imm32(PAINT_COLOURS + i, PAINT_COLOURS + i, 0x100));
    }
    s.push(add_imm32(PAINT_ROWS, PAINT_ROWS, -1));
    s.branch_back(CS_COND_NEQUAL, PAINT_ROWS, rows);
    s.push(move32(PAINT_FLUSH_ID, 0));
    s.push(flush_caches(0, PAINT_FLUSH_ID, FLUSH_CLEAN));
    s.push(wait_slot(1));
    s
}

// ---- G5b: presenting the painted view through drv-fb (DESIGN/019) ----

/// drv-gpu's bootstrap channel, its x1. Sora queues the firmware's initrd handle first. Once
/// drv-fb's views exist, on a board with the GPU power unit, it sends view [`GPU_VIEW`]'s
/// presentation channel and surface VMO, then closes its end; elsewhere it only closes.
pub const BOOTSTRAP_FIRMWARE: u8 = b'f';
pub const BOOTSTRAP_CLIENT: u8 = b'g';
pub const BOOTSTRAP_VIEW_CHANNEL: u8 = b'v';
pub const BOOTSTRAP_VIEW_SURFACE: u8 = b's';
/// The band's second well (DESIGN/019 F1a); Sora keeps the first for gfx-demo and gfx-client.
pub const GPU_VIEW: usize = 1;

/// The painted target as DESIGN/019 describes a surface: linear XRGB8888, no padding.
pub const PAINT_SURFACE: kumo_graphics::Surface = kumo_graphics::Surface {
    width: PAINT_WIDTH,
    height: PAINT_HEIGHT,
    stride: PAINT_WIDTH,
    format: kumo_graphics::XRGB8888,
};

/// The surface VMO is written in the VmoWrite syscall's 256-byte chunks (DESIGN/019 F0): 64
/// pixels of one row.
pub const SURFACE_CHUNK_BYTES: usize = 256;
const SURFACE_CHUNK_PIXELS: u32 = SURFACE_CHUNK_BYTES as u32 / 4;

/// Every chunk of the painted surface as (byte offset, first pixel's x, y), row-major. The same
/// offset addresses the GPU target and the surface VMO, since neither has padding.
pub fn surface_chunks() -> impl Iterator<Item = (u64, u32, u32)> {
    (0..PAINT_HEIGHT).flat_map(|y| {
        (0..PAINT_WIDTH / SURFACE_CHUNK_PIXELS).map(move |chunk| {
            let x = chunk * SURFACE_CHUNK_PIXELS;
            (u64::from(y * PAINT_WIDTH + x) * 4, x, y)
        })
    })
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::vec::Vec;

    struct Fake {
        power: Status,
        status: Result<DevicePowerStatus, Status>,
        mapping: Result<(), ProbeError>,
        gpu_id: u32,
        shaders: u64,
        calls: Vec<u32>,
    }

    impl Default for Fake {
        fn default() -> Self {
            Self {
                power: 0,
                status: Ok(DevicePowerStatus {
                    version: 1,
                    flags: 0x33,
                    ..DevicePowerStatus::default()
                }),
                mapping: Ok(()),
                gpu_id: 0xa867_0005,
                shaders: 0x50005,
                calls: Vec::new(),
            }
        }
    }

    impl ProbeIo for Fake {
        fn power_up(&mut self) -> Status {
            self.calls.push(1);
            self.power
        }
        fn power_status(&mut self) -> Result<DevicePowerStatus, Status> {
            self.calls.push(2);
            self.status
        }
        fn map_aperture(&mut self) -> Result<(), ProbeError> {
            self.calls.push(3);
            self.mapping
        }
        fn read32(&mut self, offset: u32) -> u32 {
            self.calls.push(0x1000 + offset);
            match offset {
                GPU_ID => self.gpu_id,
                SHADER_PRESENT_LO => self.shaders as u32,
                SHADER_PRESENT_HI => (self.shaders >> 32) as u32,
                _ => panic!("unexpected register"),
            }
        }
    }

    #[test]
    fn powers_checks_maps_then_reads_identity_and_both_shader_words() {
        let mut io = Fake {
            shaders: 0x1_0005_0001,
            ..Fake::default()
        };
        let identity = probe(&mut io).unwrap();
        assert_eq!(identity.gpu_id, 0xa867_0005);
        assert_eq!(identity.cores(), 4);
        assert_eq!(io.calls, [1, 2, 3, 0x1000, 0x1100, 0x1104]);
    }

    #[test]
    fn power_failure_never_requests_status_or_mmio() {
        for error in [Errno::NotSupported, Errno::TimedOut, Errno::AccessDenied] {
            let mut io = Fake {
                power: error.status(),
                ..Fake::default()
            };
            assert_eq!(probe(&mut io), Err(ProbeError::PowerUp(error.status())));
            assert_eq!(io.calls, [1]);
        }
    }

    #[test]
    fn incomplete_or_unknown_status_never_mints() {
        for flags in [0, 0x32, 0x31, 0x23, 0x13, 0x37, 0x3b] {
            let mut io = Fake::default();
            io.status.as_mut().unwrap().flags = flags;
            assert!(matches!(probe(&mut io), Err(ProbeError::NotReady(_))));
            assert_eq!(io.calls, [1, 2]);
        }
        let mut io = Fake::default();
        io.status.as_mut().unwrap().version = 2;
        assert!(matches!(probe(&mut io), Err(ProbeError::NotReady(_))));
        assert_eq!(io.calls, [1, 2]);
        io.status = Err(Errno::BadHandle.status());
        io.calls.clear();
        assert_eq!(probe(&mut io), Err(ProbeError::PowerStatus(-2)));
        assert_eq!(io.calls, [1, 2]);
    }

    #[test]
    fn mint_or_map_failure_never_reads() {
        for error in [ProbeError::Mint, ProbeError::Map(-7)] {
            let mut io = Fake {
                mapping: Err(error),
                ..Fake::default()
            };
            assert_eq!(probe(&mut io), Err(error));
            assert_eq!(io.calls, [1, 2, 3]);
        }
    }

    #[test]
    fn unexpected_identity_stops_at_id_and_zero_cores_is_not_online() {
        for gpu_id in [0, u32::MAX, 0xa862_0005] {
            let mut io = Fake {
                gpu_id,
                ..Fake::default()
            };
            assert!(matches!(probe(&mut io), Err(ProbeError::Identity { .. })));
            assert_eq!(io.calls, [1, 2, 3, 0x1000]);
        }
        let mut io = Fake {
            shaders: 0,
            ..Fake::default()
        };
        assert!(matches!(probe(&mut io), Err(ProbeError::Identity { .. })));
    }

    const ARM: u32 = 1;
    const WAIT: u32 = 2;
    const COMPLETE: u32 = 3;
    const WRITE: u32 = 0x1000;
    const READ: u32 = 0x2000;

    struct FakeGpu {
        reads: HashMap<u32, VecDeque<u32>>,
        read_log: Vec<u32>,
        writes: Vec<(u32, u32)>,
        calls: Vec<u32>,
        irqs: Vec<u32>,
        now: u64,
        delays: u64,
        completion: Completion,
        arm: Result<(), Status>,
    }

    impl FakeGpu {
        fn new(completion: Completion) -> Self {
            Self {
                reads: HashMap::new(),
                read_log: Vec::new(),
                writes: Vec::new(),
                calls: Vec::new(),
                irqs: Vec::new(),
                now: 0,
                delays: 0,
                completion,
                arm: Ok(()),
            }
        }

        /// The last scripted value repeats, so a one-value script answers every read.
        fn set_reads(&mut self, offset: u32, values: &[u32]) {
            self.reads
                .insert(offset, values.iter().copied().collect::<VecDeque<u32>>());
        }

        fn next_read(&mut self, offset: u32) -> u32 {
            match self.reads.get_mut(&offset) {
                Some(queue) if !queue.is_empty() => {
                    if queue.len() > 1 {
                        queue.pop_front().unwrap()
                    } else {
                        *queue.front().unwrap()
                    }
                }
                _ => 0,
            }
        }
    }

    impl MmioIo for FakeGpu {
        fn read32(&mut self, offset: u32) -> u32 {
            self.calls.push(READ + offset);
            self.read_log.push(offset);
            self.next_read(offset)
        }
        fn write32(&mut self, offset: u32, value: u32) {
            self.calls.push(WRITE + offset);
            self.writes.push((offset, value));
        }
    }

    impl ClockIo for FakeGpu {
        fn now_ns(&mut self) -> u64 {
            self.now
        }
        fn delay_us(&mut self, us: u64) {
            self.delays += 1;
            self.now += us * 1_000;
        }
    }

    impl CompletionIo for FakeGpu {
        fn arm_completion(&mut self, irq: u32, _timeout_ns: u64) -> Result<(), Status> {
            self.calls.push(ARM);
            self.irqs.push(irq);
            self.arm
        }
        fn wait_completion(&mut self) -> Completion {
            self.calls.push(WAIT);
            self.completion
        }
        fn complete(&mut self) {
            self.calls.push(COMPLETE);
        }
    }

    fn success_sequence() -> Vec<u32> {
        std::vec![
            ARM,
            WRITE + GPU_INT_CLEAR,
            WRITE + GPU_INT_MASK,
            WRITE + GPU_CMD,
            WAIT,
            READ + GPU_INT_RAWSTAT,
            WRITE + GPU_INT_CLEAR,
            WRITE + GPU_INT_MASK,
            COMPLETE,
        ]
    }

    fn quiet_mask_writes() -> Vec<(u32, u32)> {
        std::vec![
            (GPU_INT_CLEAR, GPU_IRQ_RESET_COMPLETED),
            (GPU_INT_MASK, GPU_IRQ_RESET_COMPLETED),
            (GPU_CMD, GPU_CMD_SOFT_RESET),
            (GPU_INT_CLEAR, GPU_IRQ_RESET_COMPLETED),
            (GPU_INT_MASK, 0),
        ]
    }

    #[test]
    fn soft_reset_arms_before_any_register_and_restores_a_quiet_mask() {
        let mut io = FakeGpu::new(Completion::Fired);
        io.set_reads(GPU_INT_RAWSTAT, &[GPU_IRQ_RESET_COMPLETED]);
        assert_eq!(soft_reset(&mut io), Ok(ResetPath::Interrupt));
        assert_eq!(io.calls, success_sequence());
        assert_eq!(io.writes, quiet_mask_writes());
        assert_eq!(io.irqs, std::vec![GPU_IRQ]);
    }

    #[test]
    fn timeout_rechecks_rawstat_like_panthor() {
        let mut io = FakeGpu::new(Completion::TimedOut);
        io.set_reads(GPU_INT_RAWSTAT, &[GPU_IRQ_RESET_COMPLETED]);
        assert_eq!(soft_reset(&mut io), Ok(ResetPath::PolledAfterTimeout));
        assert_eq!(io.calls, success_sequence());
        assert_eq!(io.writes, quiet_mask_writes());
    }

    #[test]
    fn a_timeout_without_completion_errors_but_still_quiets_the_line() {
        let mut io = FakeGpu::new(Completion::TimedOut);
        assert_eq!(soft_reset(&mut io), Err(ResetError::Timeout { rawstat: 0 }));
        assert_eq!(io.calls, success_sequence());
        assert_eq!(io.writes, quiet_mask_writes());
    }

    #[test]
    fn an_irq_without_the_reset_bit_is_unexpected() {
        let mut io = FakeGpu::new(Completion::Fired);
        assert_eq!(
            soft_reset(&mut io),
            Err(ResetError::UnexpectedCompletion { rawstat: 0 })
        );
        assert_eq!(io.calls, success_sequence());
        assert_eq!(io.writes, quiet_mask_writes());
    }

    #[test]
    fn an_unarmable_reset_wait_touches_no_register() {
        let mut io = FakeGpu::new(Completion::Fired);
        io.arm = Err(Errno::NotSupported.status());
        assert_eq!(
            soft_reset(&mut io),
            Err(ResetError::Arm(Errno::NotSupported.status()))
        );
        assert_eq!(io.calls, [ARM]);
        assert!(io.writes.is_empty());
    }

    #[test]
    fn l2_power_sets_coherency_first_and_requires_the_v610_ready_test() {
        let mut io = FakeGpu::new(Completion::Fired);
        io.set_reads(GPU_L2_PRESENT_LO, &[L2_POWER_MASK as u32]);
        io.set_reads(L2_READY_LO, &[0, L2_POWER_MASK as u32]);
        assert_eq!(
            power_l2(&mut io),
            Ok(L2Power {
                present: L2_POWER_MASK,
                ready: L2_POWER_MASK,
                coherency_features: 0,
            })
        );
        assert_eq!(
            io.writes,
            std::vec![
                (GPU_COHERENCY_PROTOCOL, GPU_COHERENCY_NONE),
                (L2_PWRON_LO, L2_POWER_MASK as u32),
                (L2_PWRON_HI, 0),
            ]
        );
        assert_eq!(io.delays, 1);
        assert_eq!(
            io.read_log,
            std::vec![
                GPU_L2_PRESENT_LO,
                GPU_L2_PRESENT_HI,
                GPU_COHERENCY_FEATURES,
                L2_PWRTRANS_LO,
                L2_PWRTRANS_HI,
                L2_READY_LO,
                L2_READY_HI,
                L2_READY_LO,
                L2_READY_HI,
            ]
        );
    }

    #[test]
    fn l2_power_reports_absence_without_writing_a_register() {
        let mut io = FakeGpu::new(Completion::Fired);
        assert_eq!(power_l2(&mut io), Err(L2Error::Absent));
        assert!(io.writes.is_empty());
        assert!(io.delays == 0);
        assert_eq!(io.read_log, std::vec![GPU_L2_PRESENT_LO, GPU_L2_PRESENT_HI]);
    }

    #[test]
    fn l2_power_times_out_when_ready_never_sets() {
        let mut io = FakeGpu::new(Completion::Fired);
        io.set_reads(GPU_L2_PRESENT_LO, &[L2_POWER_MASK as u32]);
        assert_eq!(power_l2(&mut io), Err(L2Error::Readiness { ready: 0 }));
        // 20 ms of READY polling at 100 µs per step.
        assert_eq!(io.delays, 200);
    }

    #[test]
    fn as_enable_locks_flushes_writes_and_updates_in_panthor_order() {
        let mut io = FakeGpu::new(Completion::Fired);
        assert_eq!(enable_as0(&mut io, 0x1234_5000, 48), Ok(()));
        assert_eq!(
            io.writes,
            std::vec![
                (AS_LOCKADDR, AS_LOCKADDR_WHOLE as u32),
                (AS_LOCKADDR + 4, 0),
                (AS_COMMAND, AS_COMMAND_LOCK),
                (AS_COMMAND, AS_COMMAND_FLUSH_MEM),
                (AS_TRANSTAB, 0x1234_5000),
                (AS_TRANSTAB + 4, 0),
                (AS_MEMATTR, 0x8c4c_8f4c),
                (AS_MEMATTR + 4, 0x4c4c_4c4c),
                (AS_TRANSCFG, 0x4200_01c6),
                (AS_TRANSCFG + 4, 0),
                (AS_COMMAND, AS_COMMAND_UPDATE),
            ]
        );
        assert_eq!(io.read_log, std::vec![AS_STATUS; 6]);
    }

    #[test]
    fn a_stuck_active_bit_stops_the_enable_before_any_config_write() {
        let mut io = FakeGpu::new(Completion::Fired);
        io.set_reads(AS_STATUS, &[AS_STATUS_AS_ACTIVE]);
        assert_eq!(
            enable_as0(&mut io, 0x1000, 48),
            Err(AsError::Busy { status: 1 })
        );
        assert_eq!(
            io.writes,
            std::vec![
                (AS_LOCKADDR, AS_LOCKADDR_WHOLE as u32),
                (AS_LOCKADDR + 4, 0)
            ]
        );
        assert_eq!(io.delays, 10_000);
    }

    #[test]
    fn as_disable_flushes_then_leaves_the_space_unmapped() {
        let mut io = FakeGpu::new(Completion::Fired);
        assert_eq!(disable_as0(&mut io), Ok(()));
        assert_eq!(
            io.writes,
            std::vec![
                (AS_LOCKADDR, AS_LOCKADDR_WHOLE as u32),
                (AS_LOCKADDR + 4, 0),
                (AS_COMMAND, AS_COMMAND_LOCK),
                (AS_COMMAND, AS_COMMAND_FLUSH_MEM),
                (AS_TRANSTAB, 0),
                (AS_TRANSTAB + 4, 0),
                (AS_MEMATTR, 0),
                (AS_MEMATTR + 4, 0),
                (AS_TRANSCFG, AS_TRANSCFG_ADRMODE_UNMAPPED as u32),
                (AS_TRANSCFG + 4, 0),
                (AS_COMMAND, AS_COMMAND_UPDATE),
            ]
        );
    }

    #[test]
    fn address_spaces_beyond_zero_use_their_own_windows() {
        let mut io = FakeGpu::new(Completion::Fired);
        assert_eq!(enable_as(&mut io, 1, 0x2000, 48), Ok(()));
        assert_eq!(
            io.writes,
            std::vec![
                (0x2450, 63),
                (0x2454, 0),
                (0x2458, AS_COMMAND_LOCK),
                (0x2458, AS_COMMAND_FLUSH_MEM),
                (0x2440, 0x2000),
                (0x2444, 0),
                (0x2448, 0x8c4c_8f4c),
                (0x244c, 0x4c4c_4c4c),
                (0x2470, 0x4200_01c6),
                (0x2474, 0),
                (0x2458, AS_COMMAND_UPDATE),
            ]
        );
        assert_eq!(io.read_log, std::vec![0x2468; 6]);
        assert_eq!(as_window(8), None);
        let mut io = FakeGpu::new(Completion::Fired);
        assert_eq!(
            enable_as(&mut io, 8, 0x2000, 48),
            Err(AsError::Space { space: 8 })
        );
        assert!(io.writes.is_empty());
    }

    #[test]
    fn flush_page_tables_locks_flushes_and_unlocks_the_space() {
        let mut io = FakeGpu::new(Completion::Fired);
        assert_eq!(flush_page_tables(&mut io, 1), Ok(()));
        assert_eq!(
            io.writes,
            std::vec![
                (0x2450, 63),
                (0x2454, 0),
                (0x2458, AS_COMMAND_LOCK),
                (0x2458, AS_COMMAND_FLUSH_PT),
                (0x2458, AS_COMMAND_UNLOCK),
            ]
        );
        assert_eq!(io.read_log, std::vec![0x2468; 6]);
        assert_eq!(
            flush_page_tables(&mut io, 8),
            Err(AsError::Space { space: 8 })
        );
    }

    #[test]
    fn shared_region_bounds_match_research_009() {
        assert!(shared_region_contains(CSF_SHARED_REGION_START));
        assert!(shared_region_contains(CSF_SHARED_REGION_START + 0x10_0000));
        assert!(!shared_region_contains(CSF_SHARED_REGION_END));
        assert!(!shared_region_contains(0x0900_0000));
        assert!(!shared_region_contains(0x03ff_ffff));
    }

    #[test]
    fn per_space_faults_read_their_own_window() {
        let mut io = FakeGpu::new(Completion::Fired);
        assert_eq!(mmu_rawstat(&mut io), 0);
        assert!(matches!(read_fault_as(&mut io, 8), None));
        io.set_reads(0x245c, &[0x7cd0_03cb]);
        io.set_reads(0x2460, &[0x0100_1000]);
        let fault = read_fault_as(&mut io, 1).unwrap();
        assert_eq!(fault.exception, 0xcb);
        assert_eq!(fault.address, 0x0100_1000);
    }

    #[test]
    fn quiet_mmu_clears_the_fault_then_masks_the_line() {
        let mut io = FakeGpu::new(Completion::Fired);
        quiet_mmu(&mut io);
        assert_eq!(
            io.writes,
            std::vec![(MMU_INT_CLEAR, MMU_AS0_FAULT_BIT), (MMU_INT_MASK, 0)]
        );
    }

    fn fault_wait_sequence() -> Vec<u32> {
        std::vec![
            ARM,
            WRITE + MMU_INT_CLEAR,
            WRITE + MMU_INT_MASK,
            WRITE + MCU_CONTROL,
            WAIT,
            READ + MMU_INT_RAWSTAT,
            READ + AS_FAULTSTATUS,
            READ + AS_FAULTADDRESS,
            READ + AS_FAULTADDRESS + 4,
            WRITE + MMU_INT_CLEAR,
            WRITE + MMU_INT_MASK,
            COMPLETE,
        ]
    }

    fn fault_quiet_writes() -> Vec<(u32, u32)> {
        std::vec![
            (MMU_INT_CLEAR, MMU_AS0_FAULT_BIT),
            (MMU_INT_MASK, MMU_AS0_FAULT_BIT),
            (MCU_CONTROL, MCU_CONTROL_AUTO),
            (MMU_INT_CLEAR, MMU_AS0_FAULT_BIT),
            (MMU_INT_MASK, 0),
        ]
    }

    #[test]
    fn mcu_fault_arms_before_the_start_and_quiets_the_line_after_the_report() {
        let mut io = FakeGpu::new(Completion::Fired);
        io.set_reads(MMU_INT_RAWSTAT, &[MMU_AS0_FAULT_BIT]);
        io.set_reads(AS_FAULTSTATUS, &[0x5c1]);
        io.set_reads(AS_FAULTADDRESS, &[0x0040_0000]);
        assert_eq!(
            mcu_fault(&mut io),
            Ok(FaultObserved {
                fault: Fault {
                    raw: 0x5c1,
                    address: 0x0040_0000,
                    exception: 0xc1,
                    access: FaultAccess::Execute,
                    decoder: true,
                    source: 0,
                },
                completion: Completion::Fired,
            })
        );
        assert_eq!(io.calls, fault_wait_sequence());
        assert_eq!(io.writes, fault_quiet_writes());
        assert_eq!(io.irqs, std::vec![MMU_IRQ]);
    }

    #[test]
    fn a_missing_fault_times_out_after_the_rawstat_recheck_and_still_quiets() {
        let mut io = FakeGpu::new(Completion::TimedOut);
        assert_eq!(mcu_fault(&mut io), Err(FaultWait::Timeout { rawstat: 0 }));
        assert_eq!(
            io.calls,
            std::vec![
                ARM,
                WRITE + MMU_INT_CLEAR,
                WRITE + MMU_INT_MASK,
                WRITE + MCU_CONTROL,
                WAIT,
                READ + MMU_INT_RAWSTAT,
                WRITE + MMU_INT_CLEAR,
                WRITE + MMU_INT_MASK,
                COMPLETE,
            ]
        );
        assert_eq!(io.writes, fault_quiet_writes());
    }

    #[test]
    fn a_missed_interrupt_leaves_the_fault_to_the_rawstat_recheck() {
        let mut io = FakeGpu::new(Completion::TimedOut);
        io.set_reads(MMU_INT_RAWSTAT, &[MMU_AS0_FAULT_BIT]);
        io.set_reads(AS_FAULTSTATUS, &[0x0000_0200]);
        io.set_reads(AS_FAULTADDRESS, &[0x1000]);
        let observed = mcu_fault(&mut io).unwrap();
        assert_eq!(observed.completion, Completion::TimedOut);
        assert_eq!(observed.fault.access, FaultAccess::Read);
        assert_eq!(observed.fault.exception, 0x00);
    }

    #[test]
    fn an_irq_without_the_fault_bit_is_unexpected() {
        let mut io = FakeGpu::new(Completion::Fired);
        assert_eq!(
            mcu_fault(&mut io),
            Err(FaultWait::UnexpectedCompletion { rawstat: 0 })
        );
        assert_eq!(io.writes, fault_quiet_writes());
    }

    #[test]
    fn an_unarmable_fault_wait_touches_no_register() {
        let mut io = FakeGpu::new(Completion::Fired);
        io.arm = Err(Errno::NotSupported.status());
        assert_eq!(
            mcu_fault(&mut io),
            Err(FaultWait::Arm(Errno::NotSupported.status()))
        );
        assert_eq!(io.calls, [ARM]);
        assert_eq!(io.irqs, std::vec![MMU_IRQ]);
        assert!(io.writes.is_empty());
    }

    #[test]
    fn stop_mcu_polls_until_disabled_and_names_a_stuck_status() {
        let mut io = FakeGpu::new(Completion::Fired);
        io.set_reads(MCU_STATUS, &[1, MCU_STATUS_DISABLED]);
        assert_eq!(stop_mcu(&mut io), Ok(MCU_STATUS_DISABLED));
        assert_eq!(io.writes, std::vec![(MCU_CONTROL, MCU_CONTROL_DISABLE)]);
        assert_eq!(io.delays, 1);

        let mut io = FakeGpu::new(Completion::Fired);
        io.set_reads(MCU_STATUS, &[1]);
        assert_eq!(stop_mcu(&mut io), Err(McuError::StopTimeout { status: 1 }));
        assert_eq!(io.delays, 10_000);
    }

    #[test]
    fn fault_decode_matches_research_009_and_panthor_field_layout() {
        let fault = decode_fault(0x5c1, 0x0040_0000);
        assert_eq!(fault.exception, 0xc1);
        assert_eq!(fault.access, FaultAccess::Execute);
        assert!(fault.decoder);
        assert_eq!(fault.source, 0);

        let fault = decode_fault((0x3321 << 16) | (3 << 8) | 0xc8, 0x10);
        assert_eq!(fault.exception, 0xc8);
        assert_eq!(fault.access, FaultAccess::Write);
        assert!(!fault.decoder);
        assert_eq!(fault.source, 0x3321);

        assert_eq!(decode_fault(0x200, 0).access, FaultAccess::Read);
        assert_eq!(decode_fault(0x000, 0).access, FaultAccess::Atomic);
    }

    #[test]
    fn exception_names_cover_the_research_009_table() {
        assert_eq!(exception_name(0x00), "OK");
        assert_eq!(exception_name(0x80), "GPU_BUS_FAULT");
        assert_eq!(exception_name(0x8a), "GPU_CACHEABILITY_FAULT");
        assert_eq!(exception_name(0xc1), "TRANSLATION_FAULT");
        assert_eq!(exception_name(0xc8), "PERMISSION_FAULT");
        assert_eq!(exception_name(0xd9), "ACCESS_FLAG_FAULT");
        assert_eq!(exception_name(0xe0), "ADDRESS_SIZE_FAULT_IN");
        assert_eq!(exception_name(0xe5), "ADDRESS_SIZE_FAULT_OUT");
        assert_eq!(exception_name(0xe8), "MEMORY_ATTRIBUTE_FAULT");
        assert_eq!(exception_name(0x42), "UNKNOWN");
    }

    #[test]
    fn table_indices_split_a_48_bit_address_into_four_nine_bit_fields() {
        assert_eq!(
            [0, 1, 2, 3].map(|level| table_index(0x0400_0000, level)),
            [0, 0, 0x20, 0]
        );
        assert_eq!(
            [0, 1, 2, 3].map(|level| table_index(0x0000_7fff_ffff_f000, level)),
            [0xff, 0x1ff, 0x1ff, 0x1ff]
        );
        assert_eq!(table_index(0xffff_ffff_ffff_f000, 0), 0x1ff);
    }

    #[test]
    fn descriptors_match_research_009_worked_values() {
        assert_eq!(
            page_descriptor(0x1234_5000, true, false, false),
            0x1234_5f47
        );
        assert_eq!(page_descriptor(0x1234_5000, true, true, false), 0x1234_5fc7);
        assert_eq!(
            page_descriptor(0x1234_5000, false, false, false),
            0x1234_5e43
        );
        assert_eq!(
            page_descriptor(0x1234_5000, true, false, true),
            0x1234_5f47 | NO_EXECUTE
        );
        assert_eq!(table_descriptor(0x1234_5000), 0x1234_5003);
        // The low 12 bits of the physical input are not part of the address.
        assert_eq!(
            page_descriptor(0x1234_5fff, false, false, false),
            0x1234_5e43
        );
    }

    #[derive(Default)]
    struct Recorder {
        entries: Vec<(u32, usize, u64)>,
    }

    impl TableWriter for Recorder {
        fn write_entry(&mut self, level: u32, index: usize, value: u64) {
            self.entries.push((level, index, value));
        }
    }

    #[test]
    fn install_mapping_links_all_four_levels_to_the_leaf() {
        let mut recorder = Recorder::default();
        install_mapping(
            &mut recorder,
            Mapping {
                va: 0x0400_0000,
                pa: 0x05_0000,
                cached: false,
                readonly: false,
                noexec: false,
            },
            [0x10_0000, 0x10_1000, 0x10_2000, 0x10_3000],
        );
        assert_eq!(
            recorder.entries,
            std::vec![
                (0, 0, 0x10_1003),
                (1, 0, 0x10_2003),
                (2, 0x20, 0x10_3003),
                (3, 0, 0x05_0e43),
            ]
        );
    }

    #[test]
    fn g2b_constants_match_the_references() {
        assert_eq!(GPU_MMU_FEATURES, 0x014);
        assert_eq!(GPU_AS_PRESENT, 0x018);
        assert_eq!(GPU_L2_PRESENT_LO, 0x120);
        assert_eq!(L2_READY_LO, 0x160);
        assert_eq!(L2_PWRON_LO, 0x1a0);
        assert_eq!(L2_PWRTRANS_LO, 0x220);
        assert_eq!(GPU_COHERENCY_FEATURES, 0x300);
        assert_eq!(GPU_COHERENCY_PROTOCOL, 0x304);
        assert_eq!(GPU_COHERENCY_NONE, 31);
        assert_eq!(MCU_CONTROL, 0x700);
        assert_eq!(MCU_STATUS, 0x704);
        assert_eq!(MCU_CONTROL_AUTO, 2);
        assert_eq!(MMU_INT_RAWSTAT, 0x2000);
        assert_eq!(MMU_INT_CLEAR, 0x2004);
        assert_eq!(MMU_INT_MASK, 0x2008);
        assert_eq!(MMU_AS0, 0x2400);
        assert_eq!(AS_TRANSTAB, 0x2400);
        assert_eq!(AS_MEMATTR, 0x2408);
        assert_eq!(AS_COMMAND, 0x2418);
        assert_eq!(AS_FAULTSTATUS, 0x241c);
        assert_eq!(AS_FAULTADDRESS, 0x2420);
        assert_eq!(AS_STATUS, 0x2428);
        assert_eq!(AS_TRANSCFG, 0x2430);
        assert_eq!(AS_MEMATTR_NON_COHERENT, 0x4c4c_4c4c_8c4c_8f4c);
        assert_eq!(as_transcfg(48), 0x0000_0000_4200_01c6);
        assert_eq!(AS_LOCKADDR_WHOLE, 63);
        assert_eq!(MMU_IRQ, 125);
        assert_eq!(MMU_AS0_FAULT_BIT, 1);
        assert_eq!(L2_POWER_TIMEOUT_NS, 20_000_000);
        assert_eq!(MMU_COMMAND_TIMEOUT_NS, 100_000_000);
        assert_eq!(MCU_STOP_TIMEOUT_NS, 100_000_000);
        assert_eq!(FAULT_TIMEOUT_NS, 100_000_000);
        assert_eq!(RESET_TIMEOUT_NS, 100_000_000);
        assert_eq!(PAGE_SIZE, 4096);
        assert_eq!(PAGE_TABLE_ENTRIES, 512);
        assert_eq!(PAGE_TABLE_LEVELS, 4);
    }
}

#[cfg(test)]
mod fw_tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const SHARED_VA: (u32, u32) = (0x0400_0000, 0x0400_1000);

    fn le(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn header(size: u32) -> Vec<u8> {
        let mut out = Vec::new();
        le(&mut out, FIRMWARE_MAGIC);
        out.push(3); // minor
        out.push(0); // major
        out.extend_from_slice(&[0, 0]); // padding1
        le(&mut out, 0x0101_0000); // version_hash
        le(&mut out, 0); // padding2
        le(&mut out, size);
        out
    }

    fn entry(kind: u32, bits: u32, body: &[u8]) -> Vec<u8> {
        assert_eq!(body.len() % 4, 0, "entries are 4-byte packed");
        let size = 4 + body.len();
        assert!(size < 256);
        let mut out = Vec::new();
        le(&mut out, kind | ((size as u32) << 8) | bits);
        out.extend_from_slice(body);
        out
    }

    fn section_entry(flags: u32, va: (u32, u32), data: (u32, u32), name: &[u8]) -> Vec<u8> {
        assert_eq!(name.len() % 4, 0);
        let mut body = Vec::new();
        le(&mut body, flags);
        le(&mut body, va.0);
        le(&mut body, va.1);
        le(&mut body, data.0);
        le(&mut body, data.1);
        body.extend_from_slice(name);
        entry(0, 0, &body)
    }

    fn image(entries: &[Vec<u8>]) -> Vec<u8> {
        let size = FIRMWARE_HEADER_LEN + entries.iter().map(|entry| entry.len()).sum::<usize>();
        let mut out = header(size as u32);
        for entry in entries {
            out.extend_from_slice(entry);
        }
        out
    }

    fn shared() -> Vec<u8> {
        section_entry(
            SectionFlags::READ.0 | SectionFlags::SHARED.0,
            SHARED_VA,
            (0, 4),
            b"iface\x00\x00\x00",
        )
    }

    fn set_size(image: &mut [u8], size: u32) {
        image[16..20].copy_from_slice(&size.to_le_bytes());
    }

    #[test]
    fn firmware_parse_reads_the_section_plan() {
        let bytes = image(&[shared()]);
        let firmware = Firmware::parse(&bytes).unwrap();
        assert_eq!(firmware.minor(), 3);
        assert_eq!(firmware.version_hash(), 0x0101_0000);
        assert_eq!(firmware.build_sha(), None);
        let sections: Vec<_> = firmware.sections().collect();
        assert_eq!(sections.len(), 1);
        let section = sections[0];
        assert_eq!(section.va, CSF_SHARED_REGION_START);
        assert_eq!(section.len, 0x1000);
        assert_eq!(section.data, &bytes[0..4]);
        assert!(section
            .flags
            .contains(SectionFlags::READ | SectionFlags::SHARED));
        assert!(!section.update);
        assert_eq!(section.name, b"iface\x00\x00\x00");
    }

    #[test]
    fn firmware_parse_accepts_ignored_optional_and_build_info_entries() {
        let shared = shared();
        let config = entry(1, FIRMWARE_ENTRY_OPTIONAL, &[0; 8]);
        let unknown = entry(9, FIRMWARE_ENTRY_OPTIONAL, &[0; 4]);
        let sha = b"git_sha: abc123\x00";
        let table = FIRMWARE_HEADER_LEN + shared.len() + config.len() + unknown.len() + 12;
        let mut body = Vec::new();
        le(&mut body, table as u32);
        le(&mut body, sha.len() as u32);
        let build = entry(6, FIRMWARE_ENTRY_OPTIONAL, &body);
        let mut bytes = header(table as u32);
        bytes.extend_from_slice(&shared);
        bytes.extend_from_slice(&config);
        bytes.extend_from_slice(&unknown);
        bytes.extend_from_slice(&build);
        bytes.extend_from_slice(sha);
        let firmware = Firmware::parse(&bytes).unwrap();
        assert_eq!(firmware.build_sha(), Some(&b"abc123"[..]));
        assert_eq!(firmware.sections().count(), 1);
    }

    #[test]
    fn firmware_parse_refuses_a_bad_header() {
        let base = image(&[shared()]);

        let mut bad = base.clone();
        bad[0] ^= 0xff;
        assert!(matches!(
            Firmware::parse(&bad),
            Err(FirmwareError::Magic { .. })
        ));

        let mut bad = base.clone();
        bad[5] = 1;
        assert!(matches!(
            Firmware::parse(&bad),
            Err(FirmwareError::Major { major: 1 })
        ));

        let mut bad = base.clone();
        bad[6] = 1;
        assert!(matches!(
            Firmware::parse(&bad),
            Err(FirmwareError::HeaderPadding)
        ));

        let mut bad = base.clone();
        bad[12] = 1;
        assert!(matches!(
            Firmware::parse(&bad),
            Err(FirmwareError::HeaderPadding)
        ));

        let mut bad = base.clone();
        let len = bad.len() as u32;
        set_size(&mut bad, len + 1);
        assert!(matches!(
            Firmware::parse(&bad),
            Err(FirmwareError::Size { .. })
        ));

        let mut bad = base.clone();
        set_size(&mut bad, 16);
        assert!(matches!(
            Firmware::parse(&bad),
            Err(FirmwareError::Size { .. })
        ));

        let short = [0u8; 12];
        assert!(matches!(
            Firmware::parse(&short),
            Err(FirmwareError::HeaderShort { len: 12 })
        ));
    }

    #[test]
    fn firmware_parse_refuses_a_bad_entry() {
        let zero_size = {
            let mut out = Vec::new();
            le(&mut out, 0); // kind 0, size 0
            out
        };
        let bytes = image(&[zero_size, shared()]);
        assert!(matches!(
            Firmware::parse(&bytes),
            Err(FirmwareError::EntrySize { size: 0, .. })
        ));

        let odd_size = {
            let mut out = Vec::new();
            le(&mut out, 5 << 8); // kind 0, size 5
            out.extend_from_slice(&[0; 4]);
            out
        };
        let bytes = image(&[odd_size, shared()]);
        assert!(matches!(
            Firmware::parse(&bytes),
            Err(FirmwareError::EntrySize { size: 5, .. })
        ));

        // Declared size stops short of the entry it contains.
        let mut bytes = image(&[shared()]);
        set_size(&mut bytes, (FIRMWARE_HEADER_LEN + 32 - 4) as u32);
        assert!(matches!(
            Firmware::parse(&bytes),
            Err(FirmwareError::EntryOverrun { .. })
        ));

        let unknown_required = entry(9, 0, &[0; 4]);
        let bytes = image(&[unknown_required, shared()]);
        assert!(matches!(
            Firmware::parse(&bytes),
            Err(FirmwareError::UnknownEntry { kind: 9, .. })
        ));
    }

    #[test]
    fn firmware_parse_refuses_a_bad_section() {
        let bytes = image(&[section_entry(
            SectionFlags::READ.0 | SectionFlags::SHARED.0,
            SHARED_VA,
            (8, 4),
            b"",
        )]);
        assert!(matches!(
            Firmware::parse(&bytes),
            Err(FirmwareError::SectionRange { .. })
        ));

        let bytes = image(&[section_entry(
            SectionFlags::READ.0 | SectionFlags::SHARED.0,
            (0x0400_1000, 0x0400_0000),
            (0, 4),
            b"",
        )]);
        assert!(matches!(
            Firmware::parse(&bytes),
            Err(FirmwareError::SectionRange { .. })
        ));

        let bytes = image(&[section_entry(
            SectionFlags::READ.0 | SectionFlags::SHARED.0,
            SHARED_VA,
            (0, 4000),
            b"",
        )]);
        assert!(matches!(
            Firmware::parse(&bytes),
            Err(FirmwareError::SectionData { .. })
        ));

        let bytes = image(&[section_entry(
            SectionFlags::READ.0 | SectionFlags::SHARED.0,
            (0x0400_0001, 0x0400_1000),
            (0, 4),
            b"",
        )]);
        assert!(matches!(
            Firmware::parse(&bytes),
            Err(FirmwareError::SectionAlignment { .. })
        ));

        let bytes = image(&[section_entry(
            SectionFlags::READ.0 | SectionFlags::SHARED.0,
            (0x0400_0000, 0x0400_0fff),
            (0, 4),
            b"",
        )]);
        assert!(matches!(
            Firmware::parse(&bytes),
            Err(FirmwareError::SectionAlignment { .. })
        ));

        let bytes = image(&[section_entry(0x4000_0041, SHARED_VA, (0, 4), b"")]);
        assert!(matches!(
            Firmware::parse(&bytes),
            Err(FirmwareError::SectionFlags { .. })
        ));
    }

    #[test]
    fn firmware_parse_requires_a_shared_interface() {
        let bytes = image(&[section_entry(
            SectionFlags::READ.0,
            (0, 0x1000),
            (0, 4),
            b"",
        )]);
        assert!(matches!(
            Firmware::parse(&bytes),
            Err(FirmwareError::SharedSectionMissing)
        ));

        let bytes = image(&[section_entry(SectionFlags::READ.0, SHARED_VA, (0, 4), b"")]);
        assert!(matches!(
            Firmware::parse(&bytes),
            Err(FirmwareError::SharedSectionNotShared {
                va: CSF_SHARED_REGION_START
            })
        ));

        let bytes = image(&[section_entry(
            SectionFlags::READ.0 | SectionFlags::PROT.0,
            SHARED_VA,
            (0, 4),
            b"",
        )]);
        assert!(matches!(
            Firmware::parse(&bytes),
            Err(FirmwareError::SharedSectionMissing)
        ));

        let prot = section_entry(
            SectionFlags::READ.0 | SectionFlags::PROT.0,
            (0x0300_0000, 0x0300_1000),
            (0, 4),
            b"",
        );
        let bytes = image(&[prot, shared()]);
        let firmware = Firmware::parse(&bytes).unwrap();
        assert_eq!(firmware.sections().count(), 2);
    }

    #[test]
    fn firmware_parse_refuses_data_larger_than_the_section() {
        let mut bytes = image(&[section_entry(
            SectionFlags::READ.0 | SectionFlags::SHARED.0,
            SHARED_VA,
            (0, 0x1004),
            b"",
        )]);
        bytes.resize(0x1004, 0);
        assert!(matches!(
            Firmware::parse(&bytes),
            Err(FirmwareError::SectionTooLarge { .. })
        ));
    }

    #[test]
    fn section_flags_decode_panthor_map_flags() {
        let flags = SectionFlags(0xd); // READ | EXECUTE | cache mode 1
        assert!(flags.contains(SectionFlags::READ));
        assert!(!flags.writable());
        assert!(flags.executable());
        assert!(flags.cached());
        assert_eq!(flags.cache_mode(), 1);

        let flags = SectionFlags(0x1b); // READ | WRITE | cache mode 3
        assert!(flags.writable());
        assert!(!flags.executable());
        assert!(!flags.cached());
        assert_eq!(flags.cache_mode(), 3);
    }

    #[test]
    fn section_names_stay_opaque_bytes() {
        let name = b"\xff\x00\x00\x40";
        let bytes = image(&[section_entry(
            SectionFlags::READ.0 | SectionFlags::SHARED.0,
            SHARED_VA,
            (0, 4),
            name,
        )]);
        let firmware = Firmware::parse(&bytes).unwrap();
        assert_eq!(firmware.sections().next().unwrap().name, name);
    }

    #[test]
    fn corrupt_build_info_is_ignored_like_panthor() {
        let shared = shared();
        let mut body = Vec::new();
        le(&mut body, u32::MAX);
        le(&mut body, 64);
        let build = entry(6, 0, &body);
        let size = FIRMWARE_HEADER_LEN + shared.len() + build.len();
        let mut bytes = header(size as u32);
        bytes.extend_from_slice(&shared);
        bytes.extend_from_slice(&build);
        let firmware = Firmware::parse(&bytes).unwrap();
        assert_eq!(firmware.build_sha(), None);
        assert_eq!(firmware.sections().count(), 1);
    }

    #[test]
    fn the_vendored_firmware_image_parses() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../firmware/mali_csffw.bin"
        );
        let bytes = std::fs::read(path).expect("vendored mali_csffw.bin");
        let firmware = Firmware::parse(&bytes).unwrap();
        assert_eq!(firmware.minor(), 3);
        assert_eq!(firmware.version_hash(), 0x0105_0000);
        assert_eq!(
            firmware.build_sha(),
            Some(&b"95a25d71030715381f33105394285e1dcc860a65 "[..])
        );
        let sections: Vec<_> = firmware.sections().collect();
        assert!(sections.iter().any(|section| section.va == 0));
        let shared: Vec<_> = sections
            .iter()
            .filter(|section| section.va == CSF_SHARED_REGION_START)
            .collect();
        assert_eq!(shared.len(), 1);
        assert!(shared[0]
            .flags
            .contains(SectionFlags::SHARED | SectionFlags::ZERO));
    }
}

#[cfg(test)]
mod g3b_tests {
    extern crate std;
    use super::*;
    use std::collections::HashMap;
    use std::vec::Vec;

    struct FakeMemory {
        words: HashMap<u64, u64>,
    }

    impl FakeMemory {
        fn new() -> Self {
            Self {
                words: HashMap::new(),
            }
        }

        fn get(&self, phys: u64) -> u64 {
            *self.words.get(&phys).unwrap_or(&0)
        }
    }

    impl TableMemory for FakeMemory {
        fn read64(&mut self, phys: u64) -> u64 {
            self.get(phys)
        }

        fn write64(&mut self, phys: u64, value: u64) {
            self.words.insert(phys, value);
        }
    }

    const ARENA: u64 = 0x10_0000;
    const ARENA_LEN: u64 = 0x1_0000;

    fn arena() -> (FakeMemory, TableArena, u64) {
        let mut memory = FakeMemory::new();
        let mut arena = TableArena::new(ARENA, ARENA_LEN).unwrap();
        let l0 = arena.alloc_table(&mut memory).unwrap();
        (memory, arena, l0)
    }

    fn attrs() -> PageAttrs {
        PageAttrs {
            cached: true,
            readonly: false,
            executable: true,
        }
    }

    #[test]
    fn map_range_links_levels_and_reuses_parent_tables() {
        let (mut memory, mut arena, l0) = arena();
        assert_eq!(l0, ARENA);
        assert_eq!(
            map_range(
                &mut memory,
                &mut arena,
                l0,
                0x0400_0000,
                0x0020_0000,
                0x2000,
                attrs()
            ),
            Ok(())
        );
        assert_eq!(memory.get(l0), 0x10_1003);
        assert_eq!(memory.get(0x10_1000), 0x10_2003);
        assert_eq!(memory.get(0x10_2000 + 0x20 * 8), 0x10_3003);
        assert_eq!(memory.get(0x10_3000), 0x0020_0f47);
        assert_eq!(memory.get(0x10_3008), 0x0020_1f47);

        // A second page in the same 2 MiB region reuses the L3 table.
        assert_eq!(
            map_range(
                &mut memory,
                &mut arena,
                l0,
                0x0400_2000,
                0x0030_0000,
                0x1000,
                attrs()
            ),
            Ok(())
        );
        assert_eq!(memory.get(0x10_3000 + 2 * 8), 0x0030_0f47);
        assert_eq!(memory.get(0x10_2000 + 0x20 * 8), 0x10_3003);

        // A page in the next 2 MiB region allocates one more L3 table.
        assert_eq!(
            map_range(
                &mut memory,
                &mut arena,
                l0,
                0x0420_0000,
                0x0040_0000,
                0x1000,
                attrs()
            ),
            Ok(())
        );
        assert_eq!(memory.get(0x10_2000 + 0x21 * 8), 0x10_4003);
        assert_eq!(memory.get(0x10_4000), 0x0040_0f47);
    }

    #[test]
    fn map_range_applies_read_only_and_no_execute_attributes() {
        let (mut memory, mut arena, l0) = arena();
        let attrs = PageAttrs {
            cached: false,
            readonly: true,
            executable: false,
        };
        assert_eq!(
            map_range(
                &mut memory,
                &mut arena,
                l0,
                0x0100_0000,
                0x0050_0000,
                0x1000,
                attrs
            ),
            Ok(())
        );
        assert_eq!(memory.get(0x10_2000 + 0x8 * 8), 0x10_3003);
        assert_eq!(memory.get(0x10_3000), 0x0060_0000_0050_0ec3);
    }

    #[test]
    fn map_range_refuses_bad_ranges() {
        let (mut memory, mut arena, l0) = arena();
        for (va, pa, len) in [
            (0x1000, 0x2000, 0),
            (0x1001, 0x2000, 0x1000),
            (0x1000, 0x2001, 0x1000),
            (0x1000, 0x2000, 0x800),
            (1u64 << 48, 0x2000, 0x1000),
        ] {
            assert!(matches!(
                map_range(&mut memory, &mut arena, l0, va, pa, len, attrs()),
                Err(TableError::Range { .. })
            ));
        }
    }

    #[test]
    fn map_range_reports_exhaustion_and_collisions() {
        let mut memory = FakeMemory::new();
        let mut small = TableArena::new(ARENA, PAGE_SIZE).unwrap();
        let l0 = small.alloc_table(&mut memory).unwrap();
        assert_eq!(
            map_range(&mut memory, &mut small, l0, 0x1000, 0x2000, 0x1000, attrs()),
            Err(TableError::OutOfTables)
        );

        let (mut memory, mut arena, l0) = arena();
        memory.write64(l0, 0x3001); // a block descriptor where a table must be
        assert_eq!(
            map_range(&mut memory, &mut arena, l0, 0x1000, 0x2000, 0x1000, attrs()),
            Err(TableError::Collision { level: 0 })
        );
    }

    const ARM: u32 = 1;
    const WAIT: u32 = 2;
    const COMPLETE: u32 = 3;
    const WRITE: u32 = 0x1000;
    const READ: u32 = 0x2000;

    struct FakeBoot {
        reads: HashMap<u32, u32>,
        calls: Vec<u32>,
        writes: Vec<(u32, u32)>,
        irqs: Vec<u32>,
        completion: Completion,
        arm: Result<(), Status>,
    }

    impl FakeBoot {
        fn new(completion: Completion) -> Self {
            Self {
                reads: HashMap::new(),
                calls: Vec::new(),
                writes: Vec::new(),
                irqs: Vec::new(),
                completion,
                arm: Ok(()),
            }
        }

        fn set(&mut self, offset: u32, value: u32) {
            self.reads.insert(offset, value);
        }
    }

    impl MmioIo for FakeBoot {
        fn read32(&mut self, offset: u32) -> u32 {
            self.calls.push(READ + offset);
            *self.reads.get(&offset).unwrap_or(&0)
        }

        fn write32(&mut self, offset: u32, value: u32) {
            self.calls.push(WRITE + offset);
            self.writes.push((offset, value));
        }
    }

    impl CompletionIo for FakeBoot {
        fn arm_completion(&mut self, irq: u32, _timeout_ns: u64) -> Result<(), Status> {
            self.calls.push(ARM);
            self.irqs.push(irq);
            self.arm
        }

        fn wait_completion(&mut self) -> Completion {
            self.calls.push(WAIT);
            self.completion
        }

        fn complete(&mut self) {
            self.calls.push(COMPLETE);
        }
    }

    #[test]
    fn mcu_boot_arms_before_the_start_and_quiets_the_job_line() {
        let mut io = FakeBoot::new(Completion::Fired);
        io.set(JOB_INT_RAWSTAT, JOB_IRQ_GLOBAL_IF);
        assert_eq!(
            mcu_boot(&mut io),
            Ok(BootObserved {
                completion: Completion::Fired,
                rawstat: JOB_IRQ_GLOBAL_IF
            })
        );
        assert_eq!(
            io.calls,
            std::vec![
                ARM,
                WRITE + JOB_INT_CLEAR,
                WRITE + JOB_INT_MASK,
                WRITE + MCU_CONTROL,
                WAIT,
                READ + JOB_INT_RAWSTAT,
                READ + JOB_INT_STAT,
                WRITE + JOB_INT_CLEAR,
                WRITE + JOB_INT_MASK,
                COMPLETE,
            ]
        );
        assert_eq!(io.irqs, std::vec![JOB_IRQ]);
        assert_eq!(
            io.writes,
            std::vec![
                (JOB_INT_CLEAR, u32::MAX),
                (JOB_INT_MASK, u32::MAX),
                (MCU_CONTROL, MCU_CONTROL_AUTO),
                (JOB_INT_CLEAR, JOB_IRQ_GLOBAL_IF),
                (JOB_INT_MASK, 0),
            ]
        );
    }

    #[test]
    fn mcu_boot_accepts_the_status_register_after_a_missed_interrupt() {
        let mut io = FakeBoot::new(Completion::TimedOut);
        io.set(JOB_INT_STAT, JOB_IRQ_GLOBAL_IF);
        let observed = mcu_boot(&mut io).unwrap();
        assert_eq!(observed.completion, Completion::TimedOut);
    }

    #[test]
    fn mcu_boot_reports_timeout_with_the_mcu_status() {
        let mut io = FakeBoot::new(Completion::TimedOut);
        io.set(MCU_STATUS, 1);
        assert_eq!(
            mcu_boot(&mut io),
            Err(BootError::Timeout {
                rawstat: 0,
                mcu_status: 1
            })
        );
        assert_eq!(io.calls.last(), Some(&(READ + MCU_STATUS)));
    }

    #[test]
    fn mcu_boot_an_irq_without_the_global_bit_is_unexpected() {
        let mut io = FakeBoot::new(Completion::Fired);
        assert_eq!(
            mcu_boot(&mut io),
            Err(BootError::UnexpectedCompletion {
                rawstat: 0,
                stat: 0
            })
        );
    }

    #[test]
    fn mcu_boot_an_unarmable_wait_touches_no_register() {
        let mut io = FakeBoot::new(Completion::Fired);
        io.arm = Err(Errno::NotSupported.status());
        assert_eq!(
            mcu_boot(&mut io),
            Err(BootError::Arm(Errno::NotSupported.status()))
        );
        assert_eq!(io.calls, [ARM]);
        assert!(io.writes.is_empty());
    }

    #[test]
    fn global_interface_validation_matches_research_009() {
        let control = [0x0101_0000, 0x2, 0x0400_1000, 0x0400_2000, 31, 0x40, 0, 0];
        let glb = GlobalInterface::read(control, 0x0400_0000, 0x10000).unwrap();
        assert_eq!(glb.version, 0x0101_0000);
        assert_eq!(glb.major(), 1);
        assert_eq!(glb.minor(), 1);
        assert_eq!(glb.patch(), 0);
        assert_eq!(glb.input_va, 0x0400_1000);
        assert_eq!(glb.output_va, 0x0400_2000);
        assert_eq!(glb.group_num, 31);
        assert_eq!(glb.group_stride, 0x40);

        assert_eq!(
            GlobalInterface::read([0; 8], 0x0400_0000, 0x10000),
            Err(GlbError::NotBooted)
        );
        let mut bad = control;
        bad[2] = 0x0401_0000;
        assert_eq!(
            GlobalInterface::read(bad, 0x0400_0000, 0x10000),
            Err(GlbError::InputOutside)
        );
        let mut bad = control;
        bad[3] = 0x03ff_f000;
        assert_eq!(
            GlobalInterface::read(bad, 0x0400_0000, 0x10000),
            Err(GlbError::OutputOutside)
        );
        let mut bad = control;
        bad[4] = 2;
        assert_eq!(
            GlobalInterface::read(bad, 0x0400_0000, 0x10000),
            Err(GlbError::GroupCount { group_num: 2 })
        );
    }

    #[test]
    fn latched_fault_reads_the_status_only_when_bit_zero_is_set() {
        let mut io = FakeBoot::new(Completion::Fired);
        assert_eq!(latched_fault(&mut io), None);
        assert_eq!(io.calls, std::vec![READ + MMU_INT_RAWSTAT]);

        let mut io = FakeBoot::new(Completion::Fired);
        io.set(MMU_INT_RAWSTAT, MMU_AS0_FAULT_BIT);
        io.set(AS_FAULTSTATUS, 0x5c1);
        io.set(AS_FAULTADDRESS, 0x0040_0000);
        let fault = latched_fault(&mut io).unwrap();
        assert_eq!(fault.exception, 0xc1);
        assert_eq!(fault.address, 0x0040_0000);
        assert_eq!(
            io.calls,
            std::vec![
                READ + MMU_INT_RAWSTAT,
                READ + AS_FAULTSTATUS,
                READ + AS_FAULTADDRESS,
                READ + AS_FAULTADDRESS + 4,
            ]
        );
    }

    #[test]
    fn the_vendored_image_loads_seven_sections_and_skips_prot() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../firmware/mali_csffw.bin"
        );
        let bytes = std::fs::read(path).expect("vendored mali_csffw.bin");
        let firmware = Firmware::parse(&bytes).unwrap();
        let loadable: Vec<_> = firmware.loadable().collect();
        assert_eq!(loadable.len(), 7);
        assert!(loadable.iter().any(|section| section.va == 0));
        assert!(loadable
            .iter()
            .any(|section| section.va == CSF_SHARED_REGION_START));
        assert!(loadable
            .iter()
            .all(|section| !section.flags.contains(SectionFlags::PROT)));
        assert!(firmware
            .sections()
            .any(|section| section.flags.contains(SectionFlags::PROT)));
    }

    const SUBMIT: u32 = 4;

    #[test]
    fn a_job_wait_arms_before_the_doorbell_and_quiets_the_line_before_completing() {
        let mut io = FakeBoot::new(Completion::Fired);
        io.set(JOB_INT_RAWSTAT, job_irq_csg(0) | JOB_IRQ_GLOBAL_IF);
        io.set(JOB_INT_STAT, job_irq_csg(0));
        let irq = await_job_irq(&mut io, job_irq_csg(0), 1_000_000, |io| {
            io.calls.push(SUBMIT)
        });
        assert_eq!(
            irq,
            Ok(JobIrq {
                completion: Completion::Fired,
                rawstat: 0x8000_0001,
                stat: 0x1,
            })
        );
        assert_eq!(io.irqs, [JOB_IRQ]);
        assert_eq!(
            io.calls,
            std::vec![
                ARM,
                WRITE + JOB_INT_MASK,
                SUBMIT,
                WAIT,
                READ + JOB_INT_RAWSTAT,
                READ + JOB_INT_STAT,
                WRITE + JOB_INT_CLEAR,
                WRITE + JOB_INT_MASK,
                COMPLETE,
            ]
        );
        // Only the group's bit is unmasked; everything raised is cleared, then the mask closes.
        assert_eq!(
            io.writes,
            std::vec![
                (JOB_INT_MASK, 0x1),
                (JOB_INT_CLEAR, 0x8000_0001),
                (JOB_INT_MASK, 0),
            ]
        );
    }

    #[test]
    fn a_job_wait_that_cannot_arm_never_submits() {
        let mut io = FakeBoot::new(Completion::Fired);
        io.arm = Err(-5);
        let irq = await_job_irq(&mut io, job_irq_csg(0), 1_000_000, |io| {
            io.calls.push(SUBMIT)
        });
        assert_eq!(irq, Err(-5));
        assert_eq!(io.calls, [ARM]);
        assert!(io.writes.is_empty());
    }

    #[test]
    fn a_timed_out_job_wait_still_reports_and_clears_the_raw_status() {
        let mut io = FakeBoot::new(Completion::TimedOut);
        io.set(JOB_INT_RAWSTAT, job_irq_csg(0));
        let irq = await_job_irq(&mut io, job_irq_csg(0), 1_000_000, |_| {}).unwrap();
        assert_eq!(irq.completion, Completion::TimedOut);
        assert_eq!(irq.rawstat, 0x1);
        assert_eq!(io.calls.last(), Some(&COMPLETE));
        assert!(io.writes.contains(&(JOB_INT_CLEAR, 0x1)));
    }
}

#[cfg(test)]
mod g4a_tests {
    extern crate std;
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::vec::Vec;

    struct FakeGlb {
        input: HashMap<u32, u32>,
        acks: VecDeque<u32>,
        writes: Vec<(u32, u32)>,
        doorbells: u32,
        now: u64,
        delays: u64,
    }

    impl FakeGlb {
        fn new(acks: &[u32]) -> Self {
            Self {
                input: HashMap::new(),
                acks: acks.iter().copied().collect(),
                writes: Vec::new(),
                doorbells: 0,
                now: 0,
                delays: 0,
            }
        }

        /// The last scripted acknowledge repeats.
        fn next_ack(&mut self) -> u32 {
            if self.acks.len() > 1 {
                self.acks.pop_front().unwrap()
            } else {
                *self.acks.front().unwrap_or(&0)
            }
        }
    }

    impl GlbIo for FakeGlb {
        fn read_input(&mut self, offset: u32) -> u32 {
            *self.input.get(&offset).unwrap_or(&0)
        }

        fn write_input(&mut self, offset: u32, value: u32) {
            self.writes.push((offset, value));
            self.input.insert(offset, value);
        }

        fn read_output(&mut self, offset: u32) -> u32 {
            assert_eq!(offset, GLB_ACK);
            self.next_ack()
        }

        fn ring_doorbell(&mut self) {
            self.doorbells += 1;
        }
    }

    impl ClockIo for FakeGlb {
        fn now_ns(&mut self) -> u64 {
            self.now
        }

        fn delay_us(&mut self, us: u64) {
            self.delays += 1;
            self.now += us * 1_000;
        }
    }

    #[test]
    fn global_init_programs_the_controls_and_waits_for_the_ping_ack() {
        let mut io = FakeGlb::new(&[0, 0x58e]);
        assert_eq!(
            init_global_interface(&mut io, 0x5_0005, 24_000_000, GLB_PING_TIMEOUT_NS),
            Ok(GlbReport {
                req: 0x58e,
                ack: 0x58e,
                core_en_mask: 0x5_0005
            })
        );
        assert_eq!(
            io.writes,
            std::vec![
                (GLB_CORE_EN_MASK, 0x5_0005),
                (GLB_CORE_EN_MASK + 4, 0),
                (GLB_PROGRESS_TIMER, GLB_PROGRESS_TIMEOUT),
                (GLB_POWEROFF_TIMER, 235),
                (GLB_IDLE_TIMER, 19),
                (GLB_ACK_IRQ_MASK, GLB_INIT_ACK_IRQ_MASK),
                (GLB_REQ, 0x58e),
            ]
        );
        assert_eq!(io.doorbells, 1);
        assert_eq!(io.delays, 0);
    }

    #[test]
    fn global_init_toggles_against_the_firmwares_existing_ack() {
        let mut io = FakeGlb::new(&[GLB_REQ_CFG_ALLOC_EN, 0x58a]);
        assert_eq!(
            init_global_interface(&mut io, 1, 24_000_000, GLB_PING_TIMEOUT_NS),
            Ok(GlbReport {
                req: 0x58a,
                ack: 0x58a,
                core_en_mask: 1
            })
        );
        assert_eq!(io.writes.last(), Some(&(GLB_REQ, 0x58a)));
    }

    #[test]
    fn global_init_times_out_without_the_ping_ack() {
        let mut io = FakeGlb::new(&[0]);
        assert_eq!(
            init_global_interface(&mut io, 1, 24_000_000, GLB_PING_TIMEOUT_NS),
            Err(GlbInitError::PingTimeout { req: 0x58e, ack: 0 })
        );
        assert_eq!(io.delays, 10_000);
    }

    #[test]
    fn glb_timer_ticks_match_the_worked_values() {
        assert_eq!(glb_timer_ticks(10_000, 24_000_000), 235);
        assert_eq!(glb_timer_ticks(800, 24_000_000), 19);
        assert_eq!(glb_timer_ticks(1_000, 1_000_000), 1);
        assert_eq!(glb_timer_ticks(0, 24_000_000), 0);
        assert_eq!(glb_timer_ticks(1_000, 0), 0);
    }

    #[test]
    fn g4a_constants_match_research_009() {
        assert_eq!(GLB_REQ, 0x00);
        assert_eq!(GLB_ACK_IRQ_MASK, 0x04);
        assert_eq!(GLB_PROGRESS_TIMER, 0x10);
        assert_eq!(GLB_POWEROFF_TIMER, 0x14);
        assert_eq!(GLB_CORE_EN_MASK, 0x18);
        assert_eq!(GLB_IDLE_TIMER, 0x80);
        assert_eq!(GLB_ACK, 0x00);
        assert_eq!(GLB_DOORBELL, 0x0008_0000);
        assert_eq!(GLB_INIT_ACK_IRQ_MASK, 0x0400_050e);
        assert_eq!(GLB_PROGRESS_TIMEOUT, 2_560_000);
        assert_eq!(GLB_PING_TIMEOUT_NS, 100_000_000);
    }
}

#[cfg(test)]
mod g4b_tests {
    extern crate std;
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::vec::Vec;

    struct FakeFirmware {
        words: HashMap<u64, u32>,
        script: HashMap<u64, VecDeque<u32>>,
        glb_input: HashMap<u32, u32>,
        glb_output: HashMap<u32, u32>,
        writes: Vec<(u64, u32)>,
        doorbells: u32,
        now: u64,
        delays: u64,
    }

    impl FakeFirmware {
        fn new() -> Self {
            Self {
                words: HashMap::new(),
                script: HashMap::new(),
                glb_input: HashMap::new(),
                glb_output: HashMap::new(),
                writes: Vec::new(),
                doorbells: 0,
                now: 0,
                delays: 0,
            }
        }

        fn set(&mut self, mcu: u64, value: u32) {
            self.words.insert(mcu, value);
        }

        /// Successive reads of `mcu` return `values` in order; the last value repeats.
        fn script_reads(&mut self, mcu: u64, values: &[u32]) {
            self.script.insert(mcu, values.iter().copied().collect());
        }

        fn get(&self, mcu: u64) -> u32 {
            *self.words.get(&mcu).unwrap_or(&0)
        }

        fn set_glb_output(&mut self, offset: u32, value: u32) {
            self.glb_output.insert(offset, value);
        }
    }

    impl FirmwareMemory for FakeFirmware {
        fn read32(&mut self, mcu: u64) -> u32 {
            if let Some(values) = self.script.get_mut(&mcu) {
                if values.len() > 1 {
                    return values.pop_front().unwrap();
                }
                if let Some(&value) = values.front() {
                    return value;
                }
            }
            self.get(mcu)
        }

        fn write32(&mut self, mcu: u64, value: u32) {
            self.writes.push((mcu, value));
            self.words.insert(mcu, value);
        }
    }

    impl GlbIo for FakeFirmware {
        fn read_input(&mut self, offset: u32) -> u32 {
            *self.glb_input.get(&offset).unwrap_or(&0)
        }

        fn write_input(&mut self, offset: u32, value: u32) {
            self.glb_input.insert(offset, value);
        }

        fn read_output(&mut self, offset: u32) -> u32 {
            *self.glb_output.get(&offset).unwrap_or(&0)
        }

        fn ring_doorbell(&mut self) {
            self.doorbells += 1;
        }
    }

    impl ClockIo for FakeFirmware {
        fn now_ns(&mut self) -> u64 {
            self.now
        }

        fn delay_us(&mut self, us: u64) {
            self.delays += 1;
            self.now += us * 1_000;
        }
    }

    #[test]
    fn wake_stream_toggles_both_doorbell_levels_and_rings_the_glb() {
        let mut io = FakeFirmware::new();
        wake_stream(&mut io, 0x6000, 0x7000, 0, 0);
        assert_eq!(io.get(0x6008), 1);
        assert_eq!(io.glb_input.get(&GLB_DOORBELL_REQ), Some(&1));
        assert_eq!(io.doorbells, 1);
        io.set(0x7008, 1);
        io.glb_output.insert(GLB_DOORBELL_REQ, 1);
        wake_stream(&mut io, 0x6000, 0x7000, 0, 0);
        assert_eq!(io.get(0x6008), 0);
        assert_eq!(io.glb_input.get(&GLB_DOORBELL_REQ), Some(&0));
        assert_eq!(io.doorbells, 2);
    }

    #[test]
    fn queue_iface_pages_are_one_apart() {
        let iface = QueueIface::at(0x0410_0000);
        assert_eq!(iface.input_va, 0x0410_0000);
        assert_eq!(iface.output_va, 0x0410_1000);
    }

    #[test]
    fn queue_extract_and_the_stream_irq_ack_follow_research_009() {
        let mut io = FakeFirmware::new();
        io.set(0x7000, 0x28);
        let iface = QueueIface {
            input_va: 0x6000,
            output_va: 0x7000,
        };
        assert_eq!(queue_extract(&mut io, iface), 0x28);
        io.set(0x670c, 0x3);
        assert_eq!(cs_irq_req(&mut io, 0x6700), 0x3);
        ack_cs_irqs(&mut io, 0x6500, 0x3);
        assert_eq!(io.get(0x650c), 0x3);
    }

    #[test]
    fn csg_and_cs_controls_decode_research_009() {
        let mut io = FakeFirmware::new();
        io.set(0x5000, 0x10);
        io.set(0x5004, 0x6000);
        io.set(0x5008, 0x7000);
        io.set(0x500c, 0x100);
        io.set(0x5010, 0x200);
        io.set(0x5014, 8);
        io.set(0x5018, 0x20);
        assert_eq!(
            read_csg_control(&mut io, 0x5000),
            CsgControl {
                features: 0x10,
                input_va: 0x6000,
                output_va: 0x7000,
                suspend_size: 0x100,
                protm_suspend_size: 0x200,
                stream_num: 8,
                stream_stride: 0x20,
            }
        );
        io.set(0x5040, 0x33);
        io.set(0x5044, 0x6100);
        io.set(0x5048, 0x7100);
        assert_eq!(
            read_cs_control(&mut io, 0x5040),
            CsControl {
                features: 0x33,
                input_va: 0x6100,
                output_va: 0x7100,
            }
        );
    }

    #[test]
    fn program_queue_writes_the_research_009_fields() {
        let mut io = FakeFirmware::new();
        program_queue(
            &mut io,
            0x6000,
            QueueConfig {
                ring_va: 0x1000_0000,
                ring_size: 0x1000,
                ring_input_va: 0x0900_0000,
                ring_output_va: 0x0900_1000,
                doorbell: 1,
                priority: 0,
            },
        );
        assert_eq!(
            io.writes,
            std::vec![
                (0x6000, 0x511),
                (0x6004, 0x100),
                (0x600c, 0xffff_ffff),
                (0x6010, 0x1000_0000),
                (0x6014, 0),
                (0x6018, 0x1000),
                (0x6030, 0x0900_0000),
                (0x6034, 0),
                (0x6038, 0x0900_1000),
                (0x603c, 0),
            ]
        );
    }

    #[test]
    fn program_group_writes_masks_endpoint_and_toggles_the_doorbell() {
        let mut io = FakeFirmware::new();
        program_group(
            &mut io,
            0x6500,
            0x6700,
            GroupConfig {
                compute_mask: 0x5_0005,
                fragment_mask: 0x5_0005,
                tiler_mask: 0x1,
                max_compute: 4,
                max_fragment: 4,
                max_tiler: 1,
                priority: 0,
                address_space: 1,
                suspend_va: 0x0900_2000,
                protm_suspend_va: 0,
                queue_mask: 1,
            },
        );
        assert_eq!(
            io.writes,
            std::vec![
                (0x6504, 0xffff_ffff),
                (0x6520, 0x5_0005),
                (0x6524, 0),
                (0x6528, 0x5_0005),
                (0x652c, 0),
                (0x6530, 0x1),
                (0x6534, 0x0001_0404),
                (0x6540, 0x0900_2000),
                (0x6544, 0),
                (0x6548, 0),
                (0x654c, 0),
                (0x6550, 1),
                (0x6508, 1),
            ]
        );
    }

    #[test]
    fn start_group_toggles_the_glb_bit_rings_and_waits_for_the_start_ack() {
        let mut io = FakeFirmware::new();
        io.script_reads(0x6700, &[0, CSG_STATE_START | CSG_ENDPOINT_CONFIG]);
        assert_eq!(
            start_group(&mut io, 0x6500, 0x6700, 0, 100_000_000),
            Ok(0x11)
        );
        assert_eq!(io.get(0x6500), 0x11);
        assert_eq!(io.glb_input.get(&GLB_DOORBELL_REQ), Some(&1));
        assert_eq!(io.doorbells, 1);
        assert_eq!(io.delays, 0);
    }

    #[test]
    fn start_group_toggles_the_endpoint_bit_from_the_ack() {
        // The endpoint bit is already configured (ack bit 4 set), so the toggle clears it in the
        // request while the state stays at start; the fake never acks the state.
        let mut io = FakeFirmware::new();
        io.set(0x6700, CSG_ENDPOINT_CONFIG);
        assert_eq!(
            start_group(&mut io, 0x6500, 0x6700, 0, 100_000_000),
            Err(GroupError::AckTimeout {
                req: 0x01,
                ack: CSG_ENDPOINT_CONFIG
            })
        );
        assert_eq!(io.get(0x6500), 0x01);
    }

    #[test]
    fn start_group_times_out_without_the_ack() {
        let mut io = FakeFirmware::new();
        assert_eq!(
            start_group(&mut io, 0x6500, 0x6700, 0, 100_000_000),
            Err(GroupError::AckTimeout { req: 0x11, ack: 0 })
        );
        assert_eq!(io.delays, 10_000);
    }

    #[test]
    fn stream_encodings_match_research_009() {
        assert_eq!(
            first_stream(0x1000_1000),
            [
                0x0100_0000_1000_1000,
                0x0202_0000_00c0_ffee,
                0x2600_0002_0000_0000,
                0x0203_0000_0000_0000,
                0x2400_0300_0000_0233,
                0x0300_0000_0001_0000,
            ]
        );
        assert_eq!(csf_doorbell(0), 0x0008_0000);
        assert_eq!(csf_doorbell(1), 0x0009_0000);
    }

    #[test]
    fn publishing_insert_mirrors_the_firmwares_extract() {
        let mut io = FakeFirmware::new();
        io.set(0x7100, 77);
        publish_insert(
            &mut io,
            QueueIface {
                input_va: 0x6000,
                output_va: 0x7100,
            },
            40,
        );
        assert_eq!(
            io.writes,
            std::vec![(0x6008, 77), (0x600c, 0), (0x6000, 40), (0x6004, 0)]
        );
        assert_eq!(ring_offset(0x1040, 0x1000), 0x40);
    }

    #[test]
    fn cs_status_reads_the_diagnostic_fields() {
        let mut io = FakeFirmware::new();
        io.set(0x7140, 0x1234);
        io.set(0x7148, 0xC000_00FF);
        io.set(0x7150, 0x1000_3000);
        io.set(0x7158, 2);
        io.set(0x7164, 1);
        io.set(0x7160, 5);
        io.set(0x7180, 0x41);
        assert_eq!(
            read_cs_status(&mut io, 0x7100),
            CsStatus {
                cmd_ptr: 0x1234,
                wait: 0xC000_00FF,
                wait_sync_ptr: 0x1000_3000,
                wait_sync_value: 0x1_0000_0002,
                blocked_reason: 5,
                fault: 0x41,
                fatal: 0,
            }
        );
    }

    #[test]
    fn csg_events_are_pending_until_their_ack_bits_are_copied() {
        let mut io = FakeFirmware::new();
        // The host started the group (STATE=1, endpoint toggle set); the firmware acknowledged it
        // and then toggled SYNC_UPDATE and IDLE.
        io.set(0x6500, CSG_STATE_START | CSG_ENDPOINT_CONFIG);
        io.set(
            0x6700,
            CSG_STATE_START | CSG_ENDPOINT_CONFIG | CSG_SYNC_UPDATE | CSG_IDLE,
        );
        assert_eq!(
            csg_pending_events(&mut io, 0x6500, 0x6700),
            CSG_SYNC_UPDATE | CSG_IDLE
        );
        ack_csg_events(&mut io, 0x6500, 0x6700, CSG_SYNC_UPDATE);
        assert_eq!(
            io.get(0x6500),
            CSG_STATE_START | CSG_ENDPOINT_CONFIG | CSG_SYNC_UPDATE
        );
        assert_eq!(csg_pending_events(&mut io, 0x6500, 0x6700), CSG_IDLE);
        // A mask outside the events never touches STATE or the endpoint toggle.
        ack_csg_events(
            &mut io,
            0x6500,
            0x6700,
            CSG_STATE_MASK | CSG_ENDPOINT_CONFIG,
        );
        assert_eq!(
            io.get(0x6500),
            CSG_STATE_START | CSG_ENDPOINT_CONFIG | CSG_SYNC_UPDATE
        );
        // The firmware toggles SYNC_UPDATE again: the copy now clears the REQ bit.
        io.set(0x6700, CSG_STATE_START | CSG_ENDPOINT_CONFIG | CSG_IDLE);
        assert_eq!(
            csg_pending_events(&mut io, 0x6500, 0x6700),
            CSG_SYNC_UPDATE | CSG_IDLE
        );
        ack_csg_events(&mut io, 0x6500, 0x6700, CSG_EVENTS);
        assert_eq!(
            io.get(0x6500),
            CSG_STATE_START | CSG_ENDPOINT_CONFIG | CSG_IDLE
        );
        assert_eq!(csg_pending_events(&mut io, 0x6500, 0x6700), 0);
    }

    #[test]
    fn glb_idle_is_acknowledged_by_copy_without_touching_a_ping_in_flight() {
        let mut io = FakeFirmware::new();
        // A ping is requested but not yet acknowledged, and the firmware raised IDLE.
        io.write_input(GLB_REQ, GLB_REQ_IDLE_EN | GLB_REQ_PING);
        io.set_glb_output(GLB_ACK, GLB_REQ_IDLE_EN | GLB_IDLE);
        assert_eq!(glb_pending_events(&mut io), GLB_IDLE);
        ack_glb_events(&mut io, GLB_IDLE | GLB_REQ_PING);
        assert_eq!(
            io.read_input(GLB_REQ),
            GLB_REQ_IDLE_EN | GLB_REQ_PING | GLB_IDLE
        );
        assert_eq!(glb_pending_events(&mut io), 0);
        assert_eq!(io.doorbells, 0);
    }

    #[test]
    fn one_ack_pass_leaves_no_event_pending_anywhere() {
        // J589's first metal run: the direct stream's group IDLE and a global IDLE were still
        // pending when the first wrapped job armed its wait.
        let mut io = FakeFirmware::new();
        io.set(0x6500, CSG_STATE_START | CSG_ENDPOINT_CONFIG);
        io.set(0x6700, CSG_STATE_START | CSG_ENDPOINT_CONFIG | CSG_IDLE);
        io.set(0x670c, 0x1);
        io.write_input(GLB_REQ, GLB_REQ_IDLE_EN);
        io.set_glb_output(GLB_ACK, GLB_REQ_IDLE_EN | GLB_IDLE);
        let acked = ack_group_events(&mut io, 0x6500, 0x6700);
        assert_eq!(
            acked,
            EventsAcked {
                glb: GLB_IDLE,
                cs_irq_req: 0x1,
                cs_irq_ack: 0,
                csg: CSG_IDLE,
            }
        );
        assert!(acked.any());
        let again = ack_group_events(&mut io, 0x6500, 0x6700);
        assert_eq!(
            again,
            EventsAcked {
                glb: 0,
                cs_irq_req: 0x1,
                cs_irq_ack: 0x1,
                csg: 0,
            }
        );
        assert!(!again.any());
        assert_eq!(
            io.get(0x6500),
            CSG_STATE_START | CSG_ENDPOINT_CONFIG | CSG_IDLE
        );
    }
}

#[cfg(test)]
mod g4c_tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const METAL: CsFeatures = CsFeatures::decode(0x0007_085f);

    fn call() -> JobCall {
        JobCall {
            latest_flush: 0x0000_0123,
            stream_va: 0x1000_2000,
            stream_len: 40,
            sync_va: 0x1000_3000,
        }
    }

    #[test]
    fn metal_cs_features_decode_to_96_registers_and_8_scoreboards() {
        assert_eq!(
            METAL,
            CsFeatures {
                work_regs: 96,
                scoreboards: 8,
            }
        );
    }

    #[test]
    fn the_wrapper_matches_research_009_section_10_6_word_for_word() {
        // RESEARCH/009 §10.6's table, with rX = r92 (0x5C) and rX+2 = r94 (0x5E).
        assert_eq!(
            job_wrapper(METAL, call()),
            Ok([
                0x025E_0000_0000_0123,
                0x2400_5E00_0000_0233,
                0x015C_0000_1000_2000,
                0x025E_0000_0000_0028,
                0x0300_0000_0001_0000,
                0x2000_5C5E_0000_0000,
                0x015C_0000_1000_3000,
                0x015E_0000_0000_0001,
                0x0300_0000_00FF_0000,
                0x3300_5C5E_0000_0001,
                0x2F00_0000_0000_0000,
                0,
                0,
                0,
                0,
                0,
            ])
        );
        assert_eq!(JOB_WRAPPER_BYTES, 128);
    }

    #[test]
    fn the_wrapper_follows_the_register_file_and_scoreboard_count() {
        let small = CsFeatures {
            work_regs: 32,
            scoreboards: 4,
        };
        let words = job_wrapper(small, call()).unwrap();
        assert_eq!(words[1], 0x2400_1E00_0000_0233);
        assert_eq!(words[5], 0x2000_1C1E_0000_0000);
        assert_eq!(words[8], 0x0300_0000_000F_0000);
        assert_eq!(words[9], 0x3300_1C1E_0000_0001);
        let full = CsFeatures {
            work_regs: 96,
            scoreboards: 16,
        };
        assert_eq!(job_wrapper(full, call()).unwrap()[8], 0x0300_0000_FFFF_0000);
    }

    #[test]
    fn the_wrapper_refuses_what_panthor_refuses() {
        let odd = CsFeatures {
            work_regs: 95,
            scoreboards: 8,
        };
        assert_eq!(job_wrapper(odd, call()), Err(WrapperError::Registers(95)));
        let tiny = CsFeatures {
            work_regs: 6,
            scoreboards: 8,
        };
        assert_eq!(job_wrapper(tiny, call()), Err(WrapperError::Registers(6)));
        let none = CsFeatures {
            work_regs: 96,
            scoreboards: 0,
        };
        assert_eq!(job_wrapper(none, call()), Err(WrapperError::Scoreboards(0)));
        let mut bad = call();
        bad.stream_va = 0x1000_2020;
        assert_eq!(
            job_wrapper(METAL, bad),
            Err(WrapperError::Stream {
                va: 0x1000_2020,
                len: 40
            })
        );
        let mut bad = call();
        bad.stream_len = 0;
        assert!(matches!(
            job_wrapper(METAL, bad),
            Err(WrapperError::Stream { .. })
        ));
        let mut bad = call();
        bad.stream_len = 44;
        assert!(matches!(
            job_wrapper(METAL, bad),
            Err(WrapperError::Stream { .. })
        ));
        let mut bad = call();
        bad.sync_va = 0x1000_3004;
        assert_eq!(
            job_wrapper(METAL, bad),
            Err(WrapperError::SyncObject(0x1000_3004))
        );
        let mut bad = call();
        bad.latest_flush = 0x0100_0000;
        assert_eq!(
            job_wrapper(METAL, bad),
            Err(WrapperError::FlushId(0x0100_0000))
        );
        // Bit 31 is outside panthor's rejected range.
        let mut ok = call();
        ok.latest_flush = 0x8000_0001;
        assert!(job_wrapper(METAL, ok).is_ok());
    }

    #[test]
    fn job_slots_start_on_a_line_and_respect_unconsumed_words() {
        // After J586's 40-byte first stream the first wrapper skips to the next line.
        assert_eq!(
            job_slot(0x28, 0x28, 0x1000),
            Some(JobSlot {
                start: 0x40,
                end: 0xC0
            })
        );
        assert_eq!(
            job_slot(0xC0, 0x28, 0x1000),
            Some(JobSlot {
                start: 0xC0,
                end: 0x140
            })
        );
        // Exactly a full ring of unconsumed words fits; one line more does not.
        assert_eq!(
            job_slot(0x1000, 0x80, 0x1000),
            Some(JobSlot {
                start: 0x1000,
                end: 0x1080
            })
        );
        assert_eq!(job_slot(0x1000, 0x40, 0x1000), None);
        // An extract past the insert is a corrupt queue, not free space.
        assert_eq!(job_slot(0x40, 0x80, 0x1000), None);
    }

    #[test]
    fn job_words_pad_to_the_line_and_wrap_round_the_ring() {
        let wrapper = job_wrapper(METAL, call()).unwrap();
        let mut writes = Vec::new();
        let slot = job_slot(0x28, 0x28, 0x1000).unwrap();
        write_job_words(0x28, slot, &wrapper, 0x1000, |offset, word| {
            writes.push((offset, word))
        });
        assert_eq!(writes.len(), 3 + JOB_WRAPPER_WORDS);
        assert_eq!(&writes[..3], &[(0x28, 0), (0x30, 0), (0x38, 0)]);
        assert_eq!(writes[3], (0x40, wrapper[0]));
        assert_eq!(writes[18], (0xB8, 0));

        // A wrapper that starts one line before the ring's end puts its second line at 0.
        let mut writes = Vec::new();
        let slot = job_slot(0x1FC0, 0x1F00, 0x1000).unwrap();
        write_job_words(0x1FC0, slot, &wrapper, 0x1000, |offset, word| {
            writes.push((offset, word))
        });
        assert_eq!(writes.len(), JOB_WRAPPER_WORDS);
        assert_eq!(writes[0], (0xFC0, wrapper[0]));
        assert_eq!(writes[7], (0xFF8, wrapper[7]));
        assert_eq!(writes[8], (0x000, wrapper[8]));
        assert_eq!(writes[15], (0x038, 0));
    }

    #[test]
    fn a_job_wait_is_classified_by_seqno_last_wait_and_fired_count() {
        use Completion::{Fired, TimedOut};
        assert_eq!(classify_job_wait(true, Fired, 1), JobWait::Interrupt);
        // A wake for a late IDLE, then the job's own SYNC_UPDATE.
        assert_eq!(classify_job_wait(true, Fired, 2), JobWait::Interrupt);
        assert_eq!(classify_job_wait(true, TimedOut, 1), JobWait::LateSeqno);
        assert_eq!(classify_job_wait(true, TimedOut, 0), JobWait::NoInterrupt);
        // The wait fired without the group's bit: not the job's interrupt.
        assert_eq!(classify_job_wait(true, Fired, 0), JobWait::NoInterrupt);
        assert_eq!(classify_job_wait(false, Fired, 3), JobWait::Stalled);
        assert_eq!(classify_job_wait(false, TimedOut, 0), JobWait::Stalled);
    }

    #[test]
    fn the_store_stream_flushes_with_panvks_flush_id_zero() {
        // J586-J589's words, except the flush now names r3 = 0 instead of r0 (the target's low
        // word), as panvk's in-stream flushes do.
        let stream = store_stream(0x1000_1040, 0xC0DE_0001);
        assert_eq!(
            stream,
            [
                0x0100_0000_1000_1040,
                0x0202_0000_C0DE_0001,
                0x2600_0002_0000_0000,
                0x0203_0000_0000_0000,
                0x2400_0300_0000_0233,
                0x0300_0000_0001_0000,
            ]
        );
        assert_eq!(first_stream(0x1000_1000)[1], 0x0202_0000_00c0_ffee);
    }
}

#[cfg(test)]
mod g5a_tests {
    extern crate std;
    use super::*;
    use std::collections::HashMap;
    use std::vec::Vec;

    const METAL: CsFeatures = CsFeatures::decode(0x0007_085f);
    const TARGET: u64 = 0x1010_0000;
    const STREAM: u64 = 0x1000_2400;
    const SYNC: u64 = 0x1000_3000;
    const POISON: u32 = 0x00FF_00FF;

    /// A model of the command-stream processor for the opcodes KUMO emits, with the semantics
    /// RESEARCH/009 §10 and Mesa's `v10.xml`/`cs_builder.h` give them: BRANCH lands `offset`
    /// instructions after its successor when the 32-bit value register meets the condition
    /// against zero; STORE_MULTIPLE stores register `base + i` at `address + offset + 4i` for each
    /// mask bit i (KUMO only uses full masks, where packed and positional layouts agree). WAIT,
    /// FLUSH_CACHE2 and ERROR_BARRIER only order work, which this sequential model already does.
    /// It checks a stream's logic, not the hardware.
    struct Csp {
        regs: [u32; 96],
        mem: HashMap<u64, u32>,
        stored: Vec<u64>,
        executed: u64,
    }

    impl Csp {
        fn new() -> Self {
            Self {
                regs: [0; 96],
                mem: HashMap::new(),
                stored: Vec::new(),
                executed: 0,
            }
        }

        fn load(&mut self, va: u64, words: &[u64]) {
            for (index, word) in words.iter().enumerate() {
                let at = va + index as u64 * 8;
                self.mem.insert(at, *word as u32);
                self.mem.insert(at + 4, (*word >> 32) as u32);
            }
        }

        fn read32(&self, va: u64) -> u32 {
            *self.mem.get(&va).unwrap_or(&0)
        }

        fn read64(&self, va: u64) -> u64 {
            u64::from(self.read32(va)) | (u64::from(self.read32(va + 4)) << 32)
        }

        fn write64(&mut self, va: u64, value: u64) {
            self.mem.insert(va, value as u32);
            self.mem.insert(va + 4, (value >> 32) as u32);
        }

        fn pair(&self, reg: u8) -> u64 {
            u64::from(self.regs[reg as usize]) | (u64::from(self.regs[reg as usize + 1]) << 32)
        }

        fn set_pair(&mut self, reg: u8, value: u64) {
            self.regs[reg as usize] = value as u32;
            self.regs[reg as usize + 1] = (value >> 32) as u32;
        }

        fn run(&mut self, start: u64, len: u64) {
            let count = (len / 8) as i64;
            let mut pc: i64 = 0;
            while pc < count {
                self.executed += 1;
                assert!(self.executed < 2_000_000, "runaway stream");
                let at = start + pc as u64 * 8;
                let w = self.read64(at);
                let op = (w >> 56) as u8;
                let r48 = ((w >> 48) & 0xFF) as u8;
                let r40 = ((w >> 40) & 0xFF) as u8;
                let r32 = ((w >> 32) & 0xFF) as u8;
                match op {
                    0 | 3 | 36 | 47 => {}
                    1 => self.set_pair(r48, w & 0x0000_FFFF_FFFF_FFFF),
                    2 => self.regs[r48 as usize] = w as u32,
                    16 => self.regs[r48 as usize] = self.regs[r40 as usize].wrapping_add(w as u32),
                    17 => {
                        let value = self.pair(r40).wrapping_add(w as u32 as i32 as i64 as u64);
                        self.set_pair(r48, value);
                    }
                    21 => {
                        let address = self.pair(r40).wrapping_add(w as u16 as i16 as i64 as u64);
                        let mask = (w >> 16) as u16;
                        for i in 0..16u64 {
                            if mask & (1 << i) != 0 {
                                let value = self.regs[r48 as usize + i as usize];
                                self.mem.insert(address + 4 * i, value);
                                self.stored.push(address + 4 * i);
                            }
                        }
                    }
                    22 => {
                        let value = self.regs[r40 as usize];
                        let taken = match (w >> 28) & 0xF {
                            2 => value == 0,
                            3 => value != 0,
                            6 => true,
                            other => panic!("condition {other} not modelled"),
                        };
                        if taken {
                            pc += 1 + i64::from(w as u16 as i16);
                            continue;
                        }
                    }
                    32 => {
                        let (address, length) =
                            (self.pair(r40), u64::from(self.regs[r32 as usize]));
                        self.run(address, length);
                    }
                    38 => {
                        let address = self.pair(r40);
                        self.mem.insert(address, self.regs[r32 as usize]);
                        self.stored.push(address);
                    }
                    51 => {
                        let address = self.pair(r40);
                        let sum = self.read64(address).wrapping_add(self.pair(r32));
                        self.write64(address, sum);
                    }
                    other => panic!("opcode {other} not modelled at 0x{at:x}"),
                }
                pc += 1;
            }
        }

        fn poison_target(&mut self) {
            for offset in (0..PAINT_BYTES).step_by(4) {
                self.mem.insert(TARGET + offset, POISON);
            }
        }

        fn pixel(&self, x: u32, y: u32) -> u32 {
            self.read32(TARGET + u64::from(y * PAINT_WIDTH + x) * 4)
        }
    }

    #[test]
    fn the_new_encoders_follow_mesas_v10_fields() {
        assert_eq!(store_multiple(2, 0, 0xFFFF, 0), 0x1502_0000_FFFF_0000);
        assert_eq!(store_multiple(50, 0, 0xFFFF, -64), 0x1532_0000_FFFF_FFC0);
        assert_eq!(add_imm64(0, 0, 64), 0x1100_0000_0000_0040);
        assert_eq!(add_imm32(81, 81, -1), 0x1051_5100_FFFF_FFFF);
        assert_eq!(branch(CS_COND_NEQUAL, 81, -21), 0x1600_5100_3000_FFEB);
        assert_eq!(flush_caches(0, 82, FLUSH_CLEAN), 0x2400_5200_0000_0011);
        assert_eq!(flush_cache2(0, 3), flush_caches(0, 3, 0x233));
    }

    #[test]
    fn the_paint_stream_fits_a_call_and_stays_below_the_wrapper_registers() {
        let stream = paint_stream(TARGET, METAL);
        let words = stream.words();
        assert_eq!(words.len(), 176);
        assert!(words.len() <= PAINT_STREAM_MAX);
        assert_eq!(*words.last().unwrap(), wait_slot(1));
        let mut csp = Csp::new();
        csp.load(STREAM, words);
        csp.regs[92..96].copy_from_slice(&[0x9292_9292, 0x9393_9393, 0x9494_9494, 0x9595_9595]);
        csp.run(STREAM, words.len() as u64 * 8);
        assert_eq!(
            csp.regs[92..96],
            [0x9292_9292, 0x9393_9393, 0x9494_9494, 0x9595_9595]
        );
    }

    #[test]
    fn the_paint_stream_paints_exactly_the_reference_and_nothing_else() {
        let stream = paint_stream(TARGET, METAL);
        let mut csp = Csp::new();
        csp.poison_target();
        csp.load(STREAM, stream.words());
        csp.run(STREAM, stream.words().len() as u64 * 8);
        assert_eq!(csp.stored.len() as u64, PAINT_BYTES / 4);
        assert!(csp
            .stored
            .iter()
            .all(|va| (TARGET..TARGET + PAINT_BYTES).contains(va)));
        let check = check_paint(|x, y| csp.pixel(x, y));
        assert_eq!(check.mismatches, 0, "first wrong pixel {:?}", check.first);
        assert_eq!(check.checksum, paint_reference_checksum());
        // 2048 stores, each with its WAIT and address step, plus the loop bookkeeping.
        assert!(
            csp.executed > 6_000 && csp.executed < 30_000,
            "{}",
            csp.executed
        );
    }

    #[test]
    fn the_wrapped_paint_job_signals_seqno_one_after_the_image_is_complete() {
        let stream = paint_stream(TARGET, METAL);
        let len = stream.words().len() as u32 * 8;
        let wrapper = job_wrapper(
            METAL,
            JobCall {
                latest_flush: 0x00FF_FFE0,
                stream_va: STREAM,
                stream_len: len,
                sync_va: SYNC,
            },
        )
        .unwrap();
        let mut csp = Csp::new();
        csp.poison_target();
        csp.load(STREAM, stream.words());
        csp.load(0x1000_0040, &wrapper);
        csp.run(0x1000_0040, JOB_WRAPPER_BYTES);
        assert_eq!(csp.read64(SYNC), 1);
        assert_eq!(check_paint(|x, y| csp.pixel(x, y)).mismatches, 0);
    }

    #[test]
    fn the_store_stream_still_lands_its_value_in_the_model() {
        let mut csp = Csp::new();
        let stream = store_stream(0x1000_1040, 0xC0DE_0001);
        csp.load(STREAM, &stream);
        csp.run(STREAM, stream.len() as u64 * 8);
        assert_eq!(csp.read32(0x1000_1040), 0xC0DE_0001);
        assert_eq!(csp.regs[3], 0);
    }

    #[test]
    fn the_reference_pattern_has_bars_on_top_and_a_ramp_below() {
        assert_eq!(paint_pixel(0, 0), 0x00FF_0000);
        assert_eq!(paint_pixel(63, 63), 0x00FF_0000);
        assert_eq!(paint_pixel(64, 0), 0x0000_FF00);
        assert_eq!(paint_pixel(128, 10), 0x0000_00FF);
        assert_eq!(paint_pixel(255, 63), 0x00FF_FFFF);
        assert_eq!(paint_pixel(0, 64), 0x0000_0000);
        assert_eq!(paint_pixel(255, 64), 0x0000_00FF);
        assert_eq!(paint_pixel(17, 65), 0x0000_0211);
        assert_eq!(paint_pixel(255, 127), 0x0000_7EFF);
    }

    #[test]
    fn g6b_actual_client_stream_and_visibility_wrapper_complete_twice() {
        let mut csp = Csp::new();
        let stream_va = protocol::VA_START;
        let target = stream_va + protocol::PAGE_BYTES;
        let sync_va = 0x1000_3000;
        for (index, value) in gpu_client::VALUES.into_iter().enumerate() {
            let stream = gpu_client::proof_stream(target, value);
            let wrapper = client_job_wrapper(
                METAL,
                JobCall {
                    latest_flush: 0xffffe5,
                    stream_va,
                    stream_len: (stream.len() * 8) as u32,
                    sync_va,
                },
            )
            .unwrap();
            // Driver-owned clean follows WAIT-all and precedes the completion signal.
            assert_eq!(wrapper[8], wait_slot(0xff));
            assert_eq!(wrapper[9], flush_caches(0, 95, FLUSH_CLEAN));
            assert_eq!(wrapper[10], wait_slot(1));
            assert_eq!(wrapper[11] >> 56, 51);
            csp.load(stream_va, &stream);
            csp.load(0x1000_0000, &wrapper);
            csp.run(0x1000_0000, (wrapper.len() * 8) as u64);
            assert_eq!(csp.read32(target), value);
            assert_eq!(csp.read64(sync_va), index as u64 + 1);
            assert_eq!(
                csp.regs[95], 0,
                "the post-job flush cannot be skipped by an old ID"
            );
        }
    }

    #[test]
    fn the_check_names_the_first_wrong_pixel_and_counts_poison() {
        let good = check_paint(paint_pixel);
        assert_eq!(good.mismatches, 0);
        assert_eq!(good.first, None);
        let one = check_paint(|x, y| {
            if (x, y) == (5, 70) {
                0
            } else {
                paint_pixel(x, y)
            }
        });
        assert_eq!(one.mismatches, 1);
        assert_eq!(one.first, Some((5, 70, 0, paint_pixel(5, 70))));
        assert_ne!(one.checksum, good.checksum);
        let poison = check_paint(|_, _| POISON);
        assert_eq!(poison.mismatches, PAINT_WIDTH * PAINT_HEIGHT);
        assert_eq!(poison.first, Some((0, 0, POISON, 0x00FF_0000)));
    }

    #[test]
    fn g6a_client_submission_round_trips_through_the_real_wrapper_and_host_csp() {
        use protocol::{
            BindFlags, Binding, Command, Reply, Request, RequestIds, Response, Submission,
            SyncState, Timeline, PAGE_BYTES, VA_START,
        };
        let binding = Binding {
            vm: 1,
            buffer: 2,
            va: VA_START,
            offset: 0,
            bytes: PAGE_BYTES,
            flags: BindFlags(BindFlags::READ_ONLY | BindFlags::NO_EXECUTE),
        };
        binding.validate(PAGE_BYTES).unwrap();
        let target = VA_START + PAGE_BYTES;
        let value = 0x00c0_ffee;
        let stream = store_stream(target, value);
        let request = Request {
            id: 5,
            command: Command::Submit(Submission {
                group: 3,
                stream_va: VA_START,
                stream_bytes: (stream.len() * 8) as u32,
                signal_point: 1,
            }),
        };
        let decoded = Request::decode(&request.encode()).unwrap();
        let mut ids = RequestIds::default();
        ids.accept(decoded.id).unwrap();
        let Command::Submit(job) = decoded.command else {
            panic!("submit expected")
        };
        job.validate_for(binding.vm, binding).unwrap();
        let mut timeline = Timeline::default();
        timeline.reserve(job.signal_point).unwrap();
        let accepted = Reply {
            id: request.id,
            result: Ok(Response::Submitted { point: 1 }),
        };
        assert_eq!(Reply::decode(&accepted.encode()), Some(accepted));
        assert_eq!(timeline.query(1), Ok(SyncState::Pending));

        let sync_va = 0x1000_3000;
        let wrapper = job_wrapper(
            METAL,
            JobCall {
                latest_flush: 0xffffe5,
                stream_va: job.stream_va,
                stream_len: job.stream_bytes,
                sync_va,
            },
        )
        .unwrap();
        let mut csp = Csp::new();
        csp.load(job.stream_va, &stream);
        csp.load(0x1000_0000, &wrapper);
        csp.run(0x1000_0000, (wrapper.len() * 8) as u64);
        assert_eq!(csp.read32(target), value);
        assert_eq!(csp.read64(sync_va), 1);
        // Only this simulated completion observation advances the protocol timeline.
        // This is a host model, not a channel round-trip or GPU execution proof.
        timeline
            .complete(
                csp.read64(sync_va),
                csp.read32(sync_va + SYNC_STATUS_OFFSET),
            )
            .unwrap();
        let complete = Reply {
            id: 6,
            result: Ok(Response::Sync {
                point: 1,
                state: timeline.query(1).unwrap(),
            }),
        };
        assert_eq!(Reply::decode(&complete.encode()), Some(complete));
        assert_eq!(
            complete.result,
            Ok(Response::Sync {
                point: 1,
                state: SyncState::Complete { gpu_status: 0 }
            })
        );
    }
}

#[cfg(test)]
mod g5b_tests {
    use super::*;
    use kumo_graphics::{Present, VIEW_COUNT, VIEW_HEIGHT, VIEW_WIDTH};

    #[test]
    fn the_painted_surface_is_exactly_one_view_and_fits_the_grant() {
        assert_eq!(
            (PAINT_SURFACE.width, PAINT_SURFACE.height),
            (VIEW_WIDTH, VIEW_HEIGHT)
        );
        assert_eq!(PAINT_SURFACE.byte_len(), Some(PAINT_BYTES as usize));
        assert!(PAINT_SURFACE.fits(kumo_graphics::SURFACE_BYTES));
        assert!(GPU_VIEW < VIEW_COUNT);
    }

    #[test]
    fn the_chunks_tile_the_surface_in_order_without_gaps() {
        let mut expected = 0u64;
        let mut count = 0;
        for (offset, x, y) in surface_chunks() {
            assert_eq!(offset, expected);
            assert_eq!(offset, u64::from(y * PAINT_WIDTH + x) * 4);
            assert_eq!(x % 64, 0);
            expected += SURFACE_CHUNK_BYTES as u64;
            count += 1;
        }
        assert_eq!(count, 512);
        assert_eq!(expected, PAINT_BYTES);
    }

    #[test]
    fn the_present_request_round_trips_through_the_contract() {
        let frame = Present {
            sequence: 1,
            surface: PAINT_SURFACE,
            damage: kumo_graphics::DamageSet::FULL,
        };
        assert_eq!(Present::decode(&frame.encode()), Some(frame));
        let tags = [
            BOOTSTRAP_FIRMWARE,
            BOOTSTRAP_CLIENT,
            BOOTSTRAP_VIEW_CHANNEL,
            BOOTSTRAP_VIEW_SURFACE,
        ];
        assert!(tags
            .iter()
            .enumerate()
            .all(|(i, a)| tags[i + 1..].iter().all(|b| a != b)));
    }
}

#[cfg(test)]
mod g5c_tests {
    extern crate std;
    use super::*;
    use std::collections::HashMap;
    use std::string::{String, ToString};
    use std::vec::Vec;

    const MANIFEST: &str = include_str!("../../../../shaders/mali/kumo_paint.v10.manifest");

    fn manifest() -> HashMap<String, String> {
        MANIFEST
            .lines()
            .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
            .map(|line| {
                let (key, value) = line.split_once(" = ").expect("key = value");
                (key.trim().to_string(), value.trim().to_string())
            })
            .collect()
    }

    fn number(m: &HashMap<String, String>, key: &str) -> u64 {
        let value = &m[key];
        match value.strip_prefix("0x") {
            Some(hex) => u64::from_str_radix(hex, 16).unwrap(),
            None => value.parse().unwrap(),
        }
    }

    fn hex_words(m: &HashMap<String, String>, key: &str) -> Vec<u32> {
        let hex = &m[key];
        (0..hex.len())
            .step_by(8)
            .map(|i| {
                let bytes = u32::from_str_radix(&hex[i..i + 8], 16).unwrap();
                bytes.swap_bytes()
            })
            .collect()
    }

    #[test]
    fn the_kernel_constants_match_the_vendored_manifest() {
        let m = manifest();
        let k = KUMO_PAINT_V10;
        assert_eq!(m["kernel"], "kumo_paint");
        assert_eq!(number(&m, "arch"), 10);
        assert_eq!(number(&m, "binary_bytes"), k.binary.len() as u64);
        assert_eq!(m["local_size"], "16 16 1");
        assert_eq!(k.local_size, [16, 16, 1]);
        assert_eq!(number(&m, "stage_compute"), 1);
        assert_eq!(number(&m, "work_reg_count"), u64::from(k.work_reg_count));
        assert_eq!(number(&m, "preload"), k.preload);
        assert_eq!(number(&m, "tls_size"), u64::from(k.tls_size));
        assert_eq!(number(&m, "wls_size"), u64::from(k.wls_size));
        assert_eq!(
            number(&m, "allow_merging_workgroups"),
            u64::from(k.allow_merging_workgroups)
        );
        assert_eq!(number(&m, "ftz_fp16"), u64::from(k.ftz_fp16));
        assert_eq!(number(&m, "ftz_fp32"), u64::from(k.ftz_fp32));
        for key in ["ubo_count", "texture_count", "sampler_count"] {
            assert_eq!(number(&m, key), 0, "{key}");
        }
        assert_eq!(number(&m, "sysvals_bytes"), u64::from(KERNEL_SYSVALS_BYTES));
        assert_eq!(number(&m, "args_offset"), u64::from(k.args_offset));
        assert_eq!(number(&m, "args_bytes"), u64::from(k.args_bytes));
        assert_eq!(m["arg_out"], "0 8");
        assert_eq!(m["arg_stride"], "8 4");
        // Mesa reserves the sysvals and the arguments at the start of the FAU, in 32-bit words.
        assert_eq!(
            number(&m, "fau_reserved_words"),
            u64::from(k.args_offset + k.args_bytes) / 4
        );
    }

    #[test]
    fn the_descriptors_match_mesas_pan_pack_byte_for_byte() {
        let m = manifest();
        let va = number(&m, "reference_binary_va");
        assert_eq!(
            shader_program_descriptor(&KUMO_PAINT_V10, va)
                .unwrap()
                .to_vec(),
            hex_words(&m, "spd_v10")
        );
        assert_eq!(
            local_storage_descriptor(&KUMO_PAINT_V10).unwrap().to_vec(),
            hex_words(&m, "tsd_v10")
        );
        assert_eq!(
            [workgroup_size_word(&KUMO_PAINT_V10).unwrap()].to_vec(),
            hex_words(&m, "wg_size_v10")
        );
    }

    #[test]
    fn the_descriptors_decode_as_research_009_section_12_says() {
        let spd = shader_program_descriptor(&KUMO_PAINT_V10, 0x1004_0000).unwrap();
        assert_eq!(spd[0] & 0xF, 8, "type: shader");
        assert_eq!((spd[0] >> 4) & 0xF, 1, "stage: compute");
        assert_eq!(spd[0] >> 30, 2, "32 registers per thread");
        assert_eq!(spd[1], 0x3000, "r60/r61 preloaded");
        assert_eq!((spd[2], spd[3]), (0x1004_0000, 0));
        assert_eq!(workgroup_size_word(&KUMO_PAINT_V10), Ok(0x3C0F));
    }

    #[test]
    fn the_packers_refuse_what_has_not_been_pinned() {
        let mut k = KUMO_PAINT_V10;
        assert_eq!(
            shader_program_descriptor(&k, 0x1004_0040),
            Err(KernelError::BinaryAddress(0x1004_0040))
        );
        k.ftz_fp32 = true;
        assert_eq!(
            shader_program_descriptor(&k, 0x1004_0000),
            Err(KernelError::FlushToZero)
        );
        let mut k = KUMO_PAINT_V10;
        k.tls_size = 16;
        assert!(matches!(
            local_storage_descriptor(&k),
            Err(KernelError::LocalStorage { .. })
        ));
        k.local_size = [2048, 1, 1];
        assert!(matches!(
            workgroup_size_word(&k),
            Err(KernelError::WorkgroupSize(_))
        ));
        let wide = KUMO_PAINT_V10;
        assert_eq!(
            shader_program_descriptor(&wide, 0x1004_0000).unwrap()[0] >> 30,
            2
        );
        let mut wide = KUMO_PAINT_V10;
        wide.work_reg_count = 64;
        assert_eq!(
            shader_program_descriptor(&wide, 0x1004_0000).unwrap()[0] >> 30,
            0
        );
    }

    #[test]
    fn the_push_data_follows_mesas_precompiled_kernel_layout() {
        let push = kumo_paint_push([16, 8, 1], 0x1010_0000, 256);
        assert_eq!(&push[0..12], &[16, 0, 0, 0, 8, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(&push[12..24], &[0u8; 12], "padding and no printf buffer");
        assert_eq!(&push[24..32], &0x1010_0000u64.to_le_bytes());
        assert_eq!(&push[32..36], &256u32.to_le_bytes());
        assert_eq!(fau_pointer(0x1000_3100, 36), Ok(0x0500_0000_1000_3100));
        assert_eq!(
            fau_pointer(0x1000_3100, push.len() as u32),
            Ok(0x0500_0000_1000_3100)
        );
        assert!(fau_pointer(0x1000_3104, 36).is_err());
        assert!(fau_pointer(0x1000_3100, 0).is_err());
    }

    #[test]
    fn compute_upload_matches_mesa_and_covers_every_pixel_once() {
        let m = manifest();
        assert_eq!(KUMO_PAINT_BINARY_VA, number(&m, "reference_binary_va"));
        let data = kumo_paint_data(0x1010_0000).unwrap();
        for (bytes, key) in [(&data[..32], "spd_v10"), (&data[32..64], "tsd_v10")] {
            let expected: Vec<u8> = hex_words(&m, key)
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect();
            assert_eq!(bytes, expected);
        }
        assert_eq!(KUMO_PAINT_TSD_VA, KUMO_PAINT_DATA_VA + 32);
        assert_eq!(KUMO_PAINT_PUSH_VA, KUMO_PAINT_DATA_VA + 64);
        assert_eq!(
            &data[64..],
            &kumo_paint_push(KUMO_PAINT_WORKGROUPS, 0x1010_0000, PAINT_WIDTH)
        );
        let [gx, gy, gz] = KUMO_PAINT_WORKGROUPS;
        let [lx, ly, lz] = KUMO_PAINT_V10.local_size;
        assert_eq!([gx * lx, gy * ly, gz * lz], [PAINT_WIDTH, PAINT_HEIGHT, 1]);
    }

    #[test]
    fn compute_stream_keeps_the_fau_count_and_waits_before_cleaning_and_signalling() {
        let features = CsFeatures::decode(0x0007_085f);
        let stream = kumo_paint_stream(features).unwrap();
        assert_eq!(stream.len() * 8 % CS_LINE_BYTES as usize, 0);
        let mut regs = [u32::MAX; 96];
        for word in &stream[..15] {
            let dst = ((word >> 48) & 0xFF) as usize;
            match word >> 56 {
                1 => {
                    regs[dst] = *word as u32;
                    regs[dst + 1] = ((word >> 32) & 0xFFFF) as u32;
                }
                2 => regs[dst] = *word as u32,
                23 | 34 => (),
                opcode => panic!("unexpected setup opcode {opcode}"),
            }
        }
        let pair = |r: usize| u64::from(regs[r]) | (u64::from(regs[r + 1]) << 32);
        assert_eq!(pair(0), 0);
        assert_eq!(pair(8), 0x0500_0000_1004_1040);
        assert_eq!(pair(16), KUMO_PAINT_DATA_VA);
        assert_eq!(pair(24), KUMO_PAINT_TSD_VA);
        assert_eq!(&regs[32..40], &[0, 0x3c0f, 0, 0, 0, 16, 8, 1]);
        // Independent v10.xml decode: resources requested, endpoint=1, then RUN_COMPUTE
        // (increment 1, X, selects 0); both scoreboards finish before L2/LSC clean.
        assert_eq!(stream[0], 0x2200_0000_0000_0001);
        assert_eq!(stream[1], 0x1700_0000_0000_0001);
        assert_eq!(stream[15], 0x0400_0000_0000_0001);
        assert_eq!(stream[16], 0x0300_0000_00ff_0000);
        assert_eq!(stream[17], 0x0228_0000_0000_0000);
        assert_eq!(stream[18], 0x2400_2800_0000_0011);
        assert_eq!(stream[19], 0x0300_0000_0001_0000);
        assert_eq!(stream[20], 0x2200_0000_0000_0000);
        assert_eq!(&stream[21..], &[0; 3]);
        assert!(
            !stream.iter().any(|w| matches!(w >> 56, 21 | 38)),
            "the stream never paints pixels itself"
        );
    }

    #[test]
    fn compute_dispatch_refuses_missing_registers_and_scoreboards() {
        for features in [
            CsFeatures {
                work_regs: 40,
                scoreboards: 8,
            },
            CsFeatures {
                work_regs: 96,
                scoreboards: 1,
            },
            CsFeatures {
                work_regs: 96,
                scoreboards: 17,
            },
        ] {
            assert_eq!(
                kumo_paint_stream(features),
                Err(KernelError::DispatchFeatures(features))
            );
        }
    }

    #[test]
    fn the_vendored_binary_is_the_one_the_manifest_describes() {
        let m = manifest();
        let on_disk = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../shaders/mali/kumo_paint.v10.bin"
        ))
        .unwrap();
        assert_eq!(on_disk, KUMO_PAINT_V10.binary);
        assert_eq!(on_disk.len() as u64, number(&m, "binary_bytes"));
        assert_eq!(on_disk.len() % 8, 0);
        assert_eq!(m["mesa"], "26.2.5-1");
    }
}
