#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
//j584
//j585
//j586
//j587
//j588

//! G1b–G4c: power, identity, reset, the first MMU state and the CSF firmware image for the
//! RK3588 Mali-G610. Recovery class: soft-state; all state is rediscovered on launch and
//! DevicePowerUp is idempotent. The driver probes identity, soft-resets the GPU, powers the L2,
//! builds address space 0 in contiguous DMA memory. G3a parses the vendored CSF image; G3b maps
//! its sections into address space 0, boots the MCU, and validates the global interface the
//! firmware publishes when it comes up. G4a initializes that interface and proves a ping
//! round-trip. G4b starts a command-stream group and has the GPU write a value. G4c wraps each
//! job the way panthor does (§10.6) and counts completions in the queue's sync object (§11.4).

use kumo_abi::{DevicePowerStatus, Errno, Status};

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
const JOB_IRQ_GLOBAL_IF: u32 = 1 << 31;
pub const JOB_IRQ: u32 = IRQ_BASE;

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
pub fn first_stream(target: u64) -> [u64; 5] {
    store_stream(target, 0x00C0_FFEE)
}

/// §10.7's stream for any value: store it at `target`, flush, wait. It touches only r0-r2, well
/// below the wrapper's scratch quad. The flush names r0 as its flush-ID register, exactly the
/// encoding metal proved at J586.
pub fn store_stream(target: u64, value: u32) -> [u64; 5] {
    [
        move48(0, target),
        move32(2, value),
        sync_set32(0, 0, 2),
        flush_cache2(0, 0),
        wait_slot(1),
    ]
}

/// MOVE48: a 48-bit immediate into a register pair (§10.3).
pub fn move48(destination_pair: u8, address: u64) -> u64 {
    (1u64 << 56) | (u64::from(destination_pair) << 48) | (address & 0x0000_FFFF_FFFF_FFFF)
}

/// MOVE32: a 32-bit immediate into one register (§10.3).
pub fn move32(destination: u8, value: u32) -> u64 {
    (2u64 << 56) | (u64::from(destination) << 48) | u64::from(value)
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
    (36u64 << 56) | (u64::from(signal_slot) << 48) | (u64::from(flush_id_reg) << 40) | 0x233
}

/// WAIT: block until every scoreboard in the mask has signalled (§10.3).
pub fn wait_slot(mask: u16) -> u64 {
    (3u64 << 56) | (u64::from(mask) << 16)
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

/// CS_FEATURES (§9.6): what a wrapper is built against. Metal reports `0x0007085f`: 96
/// registers, 8 scoreboard slots.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CsFeatures {
    pub work_regs: u32,
    pub scoreboards: u32,
}

impl CsFeatures {
    pub const fn decode(raw: u32) -> Self {
        Self {
            work_regs: (raw & 0xFF) + 1,
            scoreboards: (raw >> 8) & 0xFF,
        }
    }
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
                0x2400_0000_0000_0233,
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
    fn the_store_stream_keeps_the_proven_first_stream_encoding() {
        let stream = store_stream(0x1000_1040, 0xC0DE_0001);
        assert_eq!(
            stream,
            [
                0x0100_0000_1000_1040,
                0x0202_0000_C0DE_0001,
                0x2600_0002_0000_0000,
                0x2400_0000_0000_0233,
                0x0300_0000_0001_0000,
            ]
        );
        assert_eq!(first_stream(0x1000_1000)[1], 0x0202_0000_00c0_ffee);
    }
}
