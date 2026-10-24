#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
//j572
//j574
//j575
//j576
//j577

//! G1b–G3a: power, identity, reset, the first MMU state and the CSF firmware image for the
//! RK3588 Mali-G610. Recovery class: soft-state; all state is rediscovered on launch and
//! DevicePowerUp is idempotent. The driver probes identity, soft-resets the GPU, powers the L2,
//! builds address space 0 in contiguous DMA memory, and runs the MCU against those tables so a
//! deliberate fault and its containment are observed. G3a adds a parse-only CSF image parser;
//! loading its sections and booting the MCU wait on CRUX's firmware decision (PLAN/013 #1).

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
}

/// RESEARCH/009 §7.4: AArch64 4 KiB stage-1 walks, write-back and read-allocate, with the
/// input-address width the hardware reports (55 − VA bits). 48 VA bits give 0x4200_01C6.
pub fn as_transcfg(va_bits: u32) -> u64 {
    AS_TRANSCFG_ADRMODE_AARCH64_4K
        | (u64::from(55 - va_bits) << AS_TRANSCFG_INA_BITS_SHIFT)
        | AS_TRANSCFG_PTW_MEMATTR_WB
        | AS_TRANSCFG_PTW_RA
}

fn wait_as_idle(io: &mut (impl MmioIo + ClockIo)) -> Result<(), AsError> {
    match poll32(
        io,
        AS_STATUS,
        MMU_COMMAND_TIMEOUT_NS,
        MMU_POLL_INTERVAL_US,
        |value| value & AS_STATUS_AS_ACTIVE == 0,
    ) {
        Ok(_) => Ok(()),
        Err(status) => Err(AsError::Busy { status }),
    }
}

fn as_command(io: &mut (impl MmioIo + ClockIo), command: u32) -> Result<(), AsError> {
    wait_as_idle(io)?;
    io.write32(AS_COMMAND, command);
    wait_as_idle(io)
}

/// panthor's opening move on a space: lock the whole range and flush its memory
/// (RESEARCH/009 §7.5; `panthor_mmu_as_enable`/`_disable`).
fn lock_and_flush_mem(io: &mut (impl MmioIo + ClockIo)) -> Result<(), AsError> {
    write64(io, AS_LOCKADDR, AS_LOCKADDR + 4, AS_LOCKADDR_WHOLE);
    as_command(io, AS_COMMAND_LOCK)?;
    as_command(io, AS_COMMAND_FLUSH_MEM)
}

/// RESEARCH/009 §7.5: publish `transtab`, the memory-attribute table and the translation
/// config, then `UPDATE` so the MMU loads them.
pub fn enable_as0(
    io: &mut (impl MmioIo + ClockIo),
    transtab: u64,
    va_bits: u32,
) -> Result<(), AsError> {
    lock_and_flush_mem(io)?;
    write64(io, AS_TRANSTAB, AS_TRANSTAB + 4, transtab);
    write64(io, AS_MEMATTR, AS_MEMATTR + 4, AS_MEMATTR_NON_COHERENT);
    write64(io, AS_TRANSCFG, AS_TRANSCFG + 4, as_transcfg(va_bits));
    as_command(io, AS_COMMAND_UPDATE)
}

/// RESEARCH/009 §7.5: flush, then leave the space unmapped.
pub fn disable_as0(io: &mut (impl MmioIo + ClockIo)) -> Result<(), AsError> {
    lock_and_flush_mem(io)?;
    write64(io, AS_TRANSTAB, AS_TRANSTAB + 4, 0);
    write64(io, AS_MEMATTR, AS_MEMATTR + 4, 0);
    write64(
        io,
        AS_TRANSCFG,
        AS_TRANSCFG + 4,
        AS_TRANSCFG_ADRMODE_UNMAPPED,
    );
    as_command(io, AS_COMMAND_UPDATE)
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
    #[ignore = "set KUMO_MALI_CSFFW to a mali_csffw.bin to parse a real image"]
    fn a_real_image_parses_when_pointed_at() {
        let Ok(path) = std::env::var("KUMO_MALI_CSFFW") else {
            return;
        };
        let bytes = std::fs::read(path).unwrap();
        let firmware = Firmware::parse(&bytes).unwrap();
        assert!(firmware.build_sha().is_some());
        let sections: Vec<_> = firmware.sections().collect();
        assert!(!sections.is_empty());
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
