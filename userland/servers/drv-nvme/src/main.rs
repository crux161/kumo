#![no_std]
#![no_main]
//j798
//! One NV3 admin-queue ownership proof per process. Sora retains a DMA VMO alias
//! before any controller write; failures park with memory quarantined. No commands,
//! doorbells, media I/O, root/config or SMMU writes. — TIMBERDOODLE

extern crate alloc;
use drv_nvme::{
    exercise, Error, Memory, Proof, RegisterIo, Startup, ACK, DMA_BYTES, PAGE, PROOF_LEN,
};
use kumo_abi::{DevicePowerStatus, Handle, Rights, VmarFlags};
use kumo_rt::{
    channel_read, channel_read_with_handle, channel_write, channel_write_with_handle, handle_close,
    handle_duplicate, handle_koid, port_bind, port_create, port_wait, resource_mint_mmio,
    syscall_handle, vmar_map, vmar_unmap, vmo_create_contiguous,
};

kumo_rt::entry!(main);
const MMIO: u64 = 0x1100_0000;
const DMA: u64 = 0x1200_0000;

fn log(level: u8, s: &str) {
    let _ = kumo_rt::log_write(level, s.as_bytes());
}
fn send(control: Handle, p: Proof) {
    let b = p.encode();
    let _ = channel_write(control, b.as_ptr(), b.len());
}
fn fail(control: Handle, session: u32, e: Error) -> ! {
    log(
        kumo_abi::sys::log_level::ERR,
        &alloc::format!("drv-nvme: before ownership: {e}\n"),
    );
    send(
        control,
        Proof {
            session,
            status: e as u32,
            ..Proof::default()
        },
    );
    kumo_rt::process_exit(1)
}
fn ready_read(port: Handle, control: Handle, b: &mut [u8]) -> Option<(usize, u64)> {
    if port_wait(port) != handle_koid(control) {
        return None;
    }
    let (n, h) = channel_read_with_handle(control, b.as_mut_ptr(), b.len());
    (n > 0 && n <= b.len()).then_some((n, h))
}
struct Registers;
impl RegisterIo for Registers {
    fn read32(&mut self, o: u16) -> u32 {
        assert!(o & 3 == 0 && u64::from(o) < PAGE);
        unsafe { core::ptr::read_volatile((MMIO + u64::from(o)) as *const u32) }
    }
    fn write32(&mut self, o: u16, v: u32) {
        assert!(matches!(o, 0x0c | 0x14 | 0x24 | 0x28 | 0x2c | 0x30 | 0x34));
        unsafe { core::ptr::write_volatile((MMIO + u64::from(o)) as *mut u32, v) }
    }
    fn barrier(&mut self) {
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!("dsb sy", options(nostack, preserves_flags));
        }
    }
    fn now_ns(&mut self) -> u64 {
        kumo_rt::clock_get()
    }
    fn sleep_ns(&mut self, n: u64) -> bool {
        kumo_rt::sleep_ns(n)
    }
}

#[no_mangle]
extern "C" fn main(
    bootstrap: u64,
    _a1: u64,
    _a2: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
    _a7: u64,
) -> ! {
    let control = Handle(bootstrap as u32);
    let port = syscall_handle(port_create()).unwrap_or_else(|| fail(control, 0, Error::Bootstrap));
    // Create the empty quarantine port before acquiring DMA or writing hardware.
    let quarantine =
        syscall_handle(port_create()).unwrap_or_else(|| fail(control, 0, Error::Bootstrap));
    if port_bind(port, control) != 0 {
        fail(control, 0, Error::Bootstrap);
    }
    let mut frame = [0u8; PROOF_LEN];
    let (n, resource) =
        ready_read(port, control, &mut frame).unwrap_or_else(|| fail(control, 0, Error::Bootstrap));
    let s = Startup::decode(&frame[..n])
        .filter(|_| resource != 0)
        .unwrap_or_else(|| fail(control, 0, Error::Bootstrap));
    let resource = Handle(resource as u32);
    let mut power = DevicePowerStatus::default();
    if kumo_rt::device_power_status(resource, s.bar, &mut power) != 0
        || power.version != DevicePowerStatus::VERSION
        || !power.contains(DevicePowerStatus::POWERED | DevicePowerStatus::APERTURE_MINTABLE)
    {
        fail(control, s.session, Error::Power);
    }
    let mmio = syscall_handle(resource_mint_mmio(resource, s.bar, PAGE))
        .unwrap_or_else(|| fail(control, s.session, Error::Mapping));
    if vmar_map(
        Handle(0),
        mmio,
        0,
        MMIO,
        PAGE,
        (VmarFlags::READ | VmarFlags::WRITE).0,
    ) != 0
    {
        fail(control, s.session, Error::Mapping);
    }
    let mut io = Registers;
    let cap = u64::from(io.read32(0)) | (u64::from(io.read32(4)) << 32);
    if cap != s.cap || io.read32(8) != s.vs {
        fail(control, s.session, Error::Identity);
    }
    let (raw, physical) = vmo_create_contiguous(DMA_BYTES);
    let dma = syscall_handle(raw).unwrap_or_else(|| fail(control, s.session, Error::Memory));
    let memory = Memory {
        session: s.session,
        physical,
    };
    if !memory.valid() {
        let _ = handle_close(dma);
        fail(control, s.session, Error::Memory);
    }
    if vmar_map(
        Handle(0),
        dma,
        0,
        DMA,
        DMA_BYTES,
        (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
    ) != 0
    {
        let _ = handle_close(dma);
        fail(control, s.session, Error::Mapping);
    }
    // DMA allocation zeroes/cleans its former WB alias in the kernel. All CPU queue
    // access here is volatile through Normal-NC, never a Rust reference. — TIMBERDOODLE
    for off in (0..DMA_BYTES).step_by(8) {
        unsafe { core::ptr::write_volatile((DMA + off) as *mut u64, 0) };
    }
    io.barrier();
    let alias = syscall_handle(handle_duplicate(dma, Rights::READ | Rights::TRANSFER))
        .unwrap_or_else(|| fail(control, s.session, Error::Memory));
    let b = memory.encode();
    if channel_write_with_handle(control, b.as_ptr(), b.len(), alias) != 0 {
        fail(control, s.session, Error::Ack);
    }
    if port_wait(port) != handle_koid(control) {
        fail(control, s.session, Error::Ack);
    }
    let mut ack = [0u8; 2];
    if channel_read(control, ack.as_mut_ptr(), ack.len()) != 1 || ack[0] != ACK {
        fail(control, s.session, Error::Ack);
    }
    log(kumo_abi::sys::log_level::NOTICE,"drv-nvme: cooperative raw-physical DMA; no SMMUv3 confinement; parent custody acknowledged\n");
    let p = exercise(&mut io, s, memory);
    log(
        if p.complete() {
            kumo_abi::sys::log_level::NOTICE
        } else {
            kumo_abi::sys::log_level::ERR
        },
        &alloc::format!("{p}\n"),
    );
    send(control, p);
    if !p.complete() {
        log(
            kumo_abi::sys::log_level::ERR,
            "drv-nvme: ownership unproven; DMA retained; parked until reset/recovery\n",
        );
        loop {
            let _ = port_wait(quarantine);
        }
    }
    // RDY=0 and EN=0 were read back before this release; Sora still owns its alias.
    let _ = vmar_unmap(DMA, DMA_BYTES);
    let _ = vmar_unmap(MMIO, PAGE);
    for h in [dma, mmio, resource, port, quarantine, control] {
        let _ = handle_close(h);
    }
    kumo_rt::process_exit(0)
}
