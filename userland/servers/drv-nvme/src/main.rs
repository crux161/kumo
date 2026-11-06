#![no_std]
#![no_main]
//j798
//j799
//j801
//! One polled Identify + read-only I/O pair ownership proof per process. Sora retains
//! a DMA VMO alias before any controller write; failures park with memory quarantined.
//! NV5 reads LBA 0, LBA 1 and the last LBA, and proves NSZE is refused before any
//! doorbell. No write/flush, root/config or SMMU writes. D5 cooperative raw DMA.
//! — TIMBERDOODLE

extern crate alloc;
use drv_nvme::{
    exercise_with, Error, Memory, Proof, RegisterIo, Startup, ACK, DMA_BYTES, MMIO_BYTES, PAGE,
    PROOF_LEN,
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
struct Registers {
    doorbells: u32,
}
impl RegisterIo for Registers {
    fn read32(&mut self, o: u16) -> u32 {
        assert!(o & 3 == 0 && u64::from(o) < PAGE);
        unsafe { core::ptr::read_volatile((MMIO + u64::from(o)) as *const u32) }
    }
    fn write32(&mut self, o: u16, v: u32) {
        assert!(matches!(
            o,
            0x0c | 0x14 | 0x24 | 0x28 | 0x2c | 0x30 | 0x34 | 0x1000 | 0x1004 | 0x1008 | 0x100c
        ));
        self.doorbells += u32::from(o >= 0x1000);
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

impl drv_nvme::admin::AdminIo for Registers {
    fn dma_read32(&mut self, off: u64) -> u32 {
        assert!(off & 3 == 0 && off < DMA_BYTES);
        unsafe { core::ptr::read_volatile((DMA + off) as *const u32) }
    }
    fn dma_write32(&mut self, off: u64, value: u32) {
        assert!(off & 3 == 0 && off < DMA_BYTES);
        unsafe { core::ptr::write_volatile((DMA + off) as *mut u32, value) }
    }
    fn dma_acquire(&mut self) {
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!("dmb oshld", options(nostack, preserves_flags));
        }
    }
}
fn identify(
    io: &mut Registers,
    s: Startup,
    memory: Memory,
    queue: &mut drv_nvme::admin::Queue,
) -> Result<drv_nvme::admin::Namespace, Error> {
    use drv_nvme::admin::{first_namespace, Controller, Identify, Namespace};
    let mut data = [0u8; 4096];
    let c = queue.identify(io, s, memory, Identify::Controller, &mut data)?;
    log(
        kumo_abi::sys::log_level::NOTICE,
        &alloc::format!(
            "NV4 IDENTIFY session={} cns=1 nsid=0 cid={} status={:#x} elapsed={}us OK\n",
            s.session,
            c.cid,
            c.status,
            c.elapsed_ns / 1000
        ),
    );
    let controller = Controller::decode(&data)?;
    if controller.vid != s.id as u16 || (s.vs >= 0x10200 && controller.version != s.vs) {
        return Err(Error::Identity);
    }
    log(
        kumo_abi::sys::log_level::NOTICE,
        &alloc::format!("{controller}\n"),
    );
    let c = queue.identify(io, s, memory, Identify::ActiveNamespaces, &mut data)?;
    let nsid = first_namespace(&data)?;
    log(
        kumo_abi::sys::log_level::NOTICE,
        &alloc::format!(
        "NV4 IDENTIFY session={} cns=2 nsid=0 cid={} status={:#x} first-nsid={} elapsed={}us OK\n",
        s.session,c.cid,c.status,nsid,c.elapsed_ns/1000),
    );
    let c = queue.identify(io, s, memory, Identify::Namespace(nsid), &mut data)?;
    let ns = Namespace::decode(nsid, &data)?;
    log(
        kumo_abi::sys::log_level::NOTICE,
        &alloc::format!(
            "NV4 IDENTIFY session={} cns=0 nsid={} cid={} status={:#x} elapsed={}us OK\n",
            s.session,
            nsid,
            c.cid,
            c.status,
            c.elapsed_ns / 1000
        ),
    );
    log(kumo_abi::sys::log_level::NOTICE, &alloc::format!("{ns}\n"));
    Ok(ns)
}

fn read_block(
    io: &mut Registers,
    s: Startup,
    memory: Memory,
    pair: &mut drv_nvme::io::IoPair,
    ns: &drv_nvme::admin::Namespace,
    lba: u64,
) -> Result<(), Error> {
    let mut block = [0u8; PAGE as usize];
    let out = &mut block[..ns.lba_bytes as usize];
    let c = pair.read(io, s, memory, lba, 1, out)?;
    log(
        kumo_abi::sys::log_level::NOTICE,
        &alloc::format!(
            "NV5 READ session={} nsid={} lba={} blocks=1 bytes={} cid={} status={:#x} elapsed={}us nonzero={} sha256={}\n",
            s.session,
            ns.id,
            lba,
            out.len(),
            c.cid,
            c.status,
            c.elapsed_ns / 1000,
            out.iter().filter(|&&b| b != 0).count(),
            kumo_sha256::Hex(kumo_sha256::digest(out))
        ),
    );
    Ok(())
}

/// NV5: Identify, then one polled I/O pair; read LBA 0 and 1, prove NSZE is
/// refused with no doorbell, read the last LBA (the queue survived the refusal),
/// then delete SQ 1 and CQ 1 before the caller's proved disable. — TIMBERDOODLE
fn media(io: &mut Registers, s: Startup, memory: Memory) -> Result<(), Error> {
    use drv_nvme::io::{IoPair, IO_CQ, IO_SQ};
    let mut admin = drv_nvme::admin::Queue::new();
    let ns = identify(io, s, memory, &mut admin)?;
    let mut pair = IoPair::open(&mut admin, io, s, memory, ns)?;
    log(
        kumo_abi::sys::log_level::NOTICE,
        &alloc::format!(
            "NV5 QUEUES session={} created qid=1 depth={} sq={:#x} cq={:#x} polled ien=0 allocated-sq={} allocated-cq={}\n",
            s.session,
            drv_nvme::DEPTH,
            memory.physical + IO_SQ,
            memory.physical + IO_CQ,
            pair.allocated.submission,
            pair.allocated.completion
        ),
    );
    read_block(io, s, memory, &mut pair, &ns, 0)?;
    read_block(io, s, memory, &mut pair, &ns, 1)?;
    let before = io.doorbells;
    let mut block = [0u8; PAGE as usize];
    match pair.read(io, s, memory, ns.size, 1, &mut block[..ns.lba_bytes as usize]) {
        Err(Error::Range) if io.doorbells == before => log(
            kumo_abi::sys::log_level::NOTICE,
            &alloc::format!(
                "NV5 RANGE session={} nsid={} lba={} (NSZE) blocks=1 refused before SQ write/doorbell; doorbells={} unchanged\n",
                s.session, ns.id, ns.size, before
            ),
        ),
        _ => return Err(Error::Range),
    }
    read_block(io, s, memory, &mut pair, &ns, ns.size - 1)?;
    pair.close(&mut admin, io, s, memory)?;
    log(
        kumo_abi::sys::log_level::NOTICE,
        &alloc::format!(
            "NV5 QUEUES session={} deleted sq=1 then cq=1; doorbells={}\n",
            s.session,
            io.doorbells
        ),
    );
    Ok(())
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
    let mmio = syscall_handle(resource_mint_mmio(resource, s.bar, MMIO_BYTES))
        .unwrap_or_else(|| fail(control, s.session, Error::Mapping));
    if vmar_map(
        Handle(0),
        mmio,
        0,
        MMIO,
        MMIO_BYTES,
        (VmarFlags::READ | VmarFlags::WRITE).0,
    ) != 0
    {
        fail(control, s.session, Error::Mapping);
    }
    let mut io = Registers { doorbells: 0 };
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
    let p = exercise_with(&mut io, s, memory, |io| media(io, s, memory));
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
    let _ = vmar_unmap(MMIO, MMIO_BYTES);
    for h in [dma, mmio, resource, port, quarantine, control] {
        let _ = handle_close(h);
    }
    kumo_rt::process_exit(0)
}
