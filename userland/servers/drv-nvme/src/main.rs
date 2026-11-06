#![no_std]
#![no_main]
//j801
//j802
//j803
//j805
//j806
//! One owner per process: polled Identify, then a read-only I/O pair whose completions
//! arrive by INTx (NV6). A proof owner runs the NV6 sequence and exits; a serve owner
//! (NV7) offers the namespace's FAT32 volume to Sora and serves it read-only until Sora
//! closes the control channel. Sora retains a DMA VMO alias before any controller
//! write; failures park with memory quarantined. Reads LBA 0, LBA 1 and the last LBA
//! single, batched and with vector 0 masked; NSZE is refused before any doorbell. No
//! root/client/config or SMMU writes. NV8 adds an explicit one-sector Write/Flush
//! proof mode outside the FAT volume. D5 cooperative raw DMA.
//! — TIMBERDOODLE

extern crate alloc;
use drv_blk::{
    decode_connect, volume_request, BlockAccess, MediaReady, VolumeRequest, CONTROL_CONNECT_LEN,
    REQUEST_LEN, STATUS_IO_ERROR, STATUS_OK, STATUS_READ_ONLY,
};
use drv_nvme::{
    exercise_with, Error, Memory, Proof, RegisterIo, Startup, ACK, DMA_BYTES, MMIO_BYTES,
    MODE_SERVE, MODE_WRITE_PROOF, PAGE, PROOF_LEN,
};
use kumo_abi::{DevicePowerStatus, Handle, Rights, VmarFlags};
use kumo_fatfs::{FatVolume, PartitionReader, SECTOR_SIZE};
use kumo_rt::{
    channel_read, channel_read_with_handle, channel_write, channel_write_with_handle, handle_close,
    handle_duplicate, handle_koid, interrupt_complete, interrupt_create, interrupt_wait, port_bind,
    port_create, port_unbind, port_wait, resource_mint_mmio, syscall_handle, timer_create,
    vmar_map, vmar_unmap, vmo_create_contiguous,
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
            0x0c | 0x10
                | 0x14
                | 0x24
                | 0x28
                | 0x2c
                | 0x30
                | 0x34
                | 0x1000
                | 0x1004
                | 0x1008
                | 0x100c
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
) -> Result<(drv_nvme::admin::Controller, drv_nvme::admin::Namespace), Error> {
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
    Ok((controller, ns))
}

/// The NVMe INTx line: one interrupt object and a private port. Each wait binds a
/// fresh one-shot timer to the same port and tells the wake sources apart by koid.
struct Irq {
    port: Handle,
    interrupt: Handle,
    koid: u64,
    waits: drv_nvme::intx::WaitSources,
}
impl Irq {
    fn bind(resource: Handle, irq: u32) -> Result<Self, Error> {
        let interrupt = syscall_handle(interrupt_create(resource, irq)).ok_or(Error::Interrupt)?;
        let Some(port) = syscall_handle(port_create()) else {
            let _ = handle_close(interrupt);
            return Err(Error::Interrupt);
        };
        let line = Self {
            port,
            interrupt,
            koid: handle_koid(interrupt),
            waits: drv_nvme::intx::WaitSources::new(handle_koid(interrupt)),
        };
        if port_bind(port, interrupt) != 0 {
            return Err(Error::Interrupt);
        }
        // Release any kernel mask an early fire applied before the port existed
        // (the drv-xhci arming lesson). Vector 0 is still masked by INTMS here.
        if interrupt_complete(interrupt) != 0 {
            return Err(Error::Interrupt);
        }
        Ok(line)
    }
}
impl Drop for Irq {
    fn drop(&mut self) {
        let _ = port_unbind(self.port, self.interrupt);
        let _ = handle_close(self.interrupt);
        let _ = handle_close(self.port);
    }
}
impl drv_nvme::intx::Line for Irq {
    fn wait(&mut self, ns: u64) -> Result<drv_nvme::intx::Wake, Error> {
        let timer = syscall_handle(timer_create(ns.max(1))).ok_or(Error::Sleep)?;
        if port_bind(self.port, timer) != 0 {
            let _ = handle_close(timer);
            return Err(Error::Sleep);
        }
        let timer_koid = handle_koid(timer);
        let mut last = 0;
        let port = self.port;
        let result = self.waits.wait(timer_koid, || {
            last = port_wait(port);
            last
        });
        let _ = port_unbind(self.port, timer);
        let _ = handle_close(timer);
        self.waits.retire(timer_koid);
        let (wake, skipped) = match result {
            Ok(value) => value,
            Err(e) => {
                log(
                    kumo_abi::sys::log_level::ERR,
                    &alloc::format!(
                        "NV8 IRQ unexpected source={} irq={} timer={}\n",
                        last,
                        self.koid,
                        timer_koid
                    ),
                );
                return Err(e);
            }
        };
        if skipped > 0 {
            log(
                kumo_abi::sys::log_level::NOTICE,
                &alloc::format!(
                    "NV8 IRQ drained {} retired timer packet(s); active deadline retained\n",
                    skipped
                ),
            );
        }
        if wake == drv_nvme::intx::Wake::Interrupt {
            let count = interrupt_wait(self.interrupt);
            if syscall_handle(count).is_none() {
                log(
                    kumo_abi::sys::log_level::ERR,
                    &alloc::format!("NV8 IRQ consume failed status={:#x}\n", count),
                );
                return Err(Error::Interrupt);
            }
        }
        Ok(wake)
    }

    fn complete(&mut self) -> Result<(), Error> {
        if interrupt_complete(self.interrupt) == 0 {
            Ok(())
        } else {
            Err(Error::Interrupt)
        }
    }
}

fn log_block(s: Startup, nsid: u32, lba: u64, mode: &str, block: &[u8], elapsed_ns: u64) {
    log(
        kumo_abi::sys::log_level::NOTICE,
        &alloc::format!(
            "NV6 READ session={} nsid={} lba={} mode={} bytes={} elapsed={}us nonzero={} sha256={}\n",
            s.session,
            nsid,
            lba,
            mode,
            block.len(),
            elapsed_ns / 1000,
            block.iter().filter(|&&b| b != 0).count(),
            kumo_sha256::Hex(kumo_sha256::digest(block))
        ),
    );
}

/// NV6: Identify (polled, vector masked), bind INTx and watch a quiet window, open
/// the pair with IEN=1, unmask vector 0, then read LBA 0 alone, LBA 1 and the last
/// LBA under one doorbell, refuse NSZE before any doorbell, watch for late deliveries,
/// re-read LBA 0 with vector 0 masked, mask, delete the pair. — TIMBERDOODLE
fn write_proof(
    io: &mut Registers,
    s: Startup,
    memory: Memory,
    resource: Handle,
) -> Result<(), Error> {
    use drv_nvme::{
        admin::Queue,
        intx::{mask, quiet, unmask, Stats, QUIET_NS},
        io::IoPair,
        volume::discover,
        write::{pattern, TestExtent, TEST_LBA},
    };
    let mut admin = Queue::new();
    let (controller, ns) = identify(io, s, memory, &mut admin)?;
    let mut line = Irq::bind(resource, s.irq)?;
    let mut stats = Stats::default();
    quiet(&mut admin, io, &mut line, QUIET_NS, &mut stats)?;
    let mut pair = IoPair::open(&mut admin, io, s, memory, ns, true)?;
    unmask(io)?;
    let found = {
        let mut b = NvmeBlocks {
            io,
            line: &mut line,
            pair: &mut pair,
            s,
            memory,
            stats: &mut stats,
        };
        let mut entries = alloc::vec![0;drv_nvme::volume::MAX_ENTRY_BYTES];
        discover(&mut b, ns.size, &mut entries)?
    };
    let (part, _) = found.chosen.ok_or(Error::Data)?;
    let gate = TestExtent::new(
        &controller,
        &ns,
        found.entries,
        part.first_lba,
        part.block_count(),
    )?;
    let mut before = [0; 1536];
    let lbas = [TEST_LBA - 1, TEST_LBA, TEST_LBA + 1];
    pair.read_irq(io, &mut line, s, memory, &lbas, &mut before, &mut stats)?;
    for (i, lba) in lbas.into_iter().enumerate() {
        log_block(
            s,
            ns.id,
            lba,
            "nv8-before",
            &before[i * 512..(i + 1) * 512],
            0,
        );
    }
    let mut metadata = [0; 1536];
    pair.read_irq(
        io,
        &mut line,
        s,
        memory,
        &[0, 1, ns.size - 1],
        &mut metadata,
        &mut stats,
    )?;
    let payload = pattern();
    if before[512..1024] == payload {
        log(kumo_abi::sys::log_level::NOTICE,&alloc::format!("NV8 WRITE session={} lba={} persistent pattern already present; zero writes/flushes\n",s.session,TEST_LBA));
    } else {
        gate.authorize(TEST_LBA, payload.len())?;
        let t = pair.write_irq(io, &mut line, s, memory, TEST_LBA, &payload, &mut stats)?;
        log(
            kumo_abi::sys::log_level::NOTICE,
            &alloc::format!(
                "NV8 WRITE session={} nsid={} lba={} bytes=512 elapsed={}us completed by INTx\n",
                s.session,
                ns.id,
                TEST_LBA,
                t / 1000
            ),
        );
        let t = pair.flush_irq(io, &mut line, s, memory, &mut stats)?;
        log(
            kumo_abi::sys::log_level::NOTICE,
            &alloc::format!(
                "NV8 FLUSH session={} nsid={} vwc={} elapsed={}us completed by INTx\n",
                s.session,
                ns.id,
                controller.vwc,
                t / 1000
            ),
        );
    }
    let mut after = [0; 1536];
    pair.read_irq(io, &mut line, s, memory, &lbas, &mut after, &mut stats)?;
    if after[..512] != before[..512]
        || after[1024..] != before[1024..]
        || after[512..1024] != payload
    {
        return Err(Error::Data);
    }
    for (i, lba) in lbas.into_iter().enumerate() {
        log_block(
            s,
            ns.id,
            lba,
            "nv8-after",
            &after[i * 512..(i + 1) * 512],
            0,
        );
    }
    pair.read_irq(
        io,
        &mut line,
        s,
        memory,
        &[0, 1, ns.size - 1],
        &mut after,
        &mut stats,
    )?;
    if after != metadata {
        return Err(Error::Data);
    }
    log(kumo_abi::sys::log_level::NOTICE,&alloc::format!("NV8 GUARDS session={} both adjacent sectors and MBR/primary/backup GPT headers unchanged; target pattern exact; entries={} deliveries={}\n",s.session,stats.entries,stats.deliveries));
    mask(io)?;
    pair.close(&mut admin, io, s, memory)?;
    Ok(())
}

fn media(io: &mut Registers, s: Startup, memory: Memory, resource: Handle) -> Result<(), Error> {
    use drv_nvme::intx::{mask, quiet, unmask, Stats, QUIET_NS};
    use drv_nvme::io::{IoPair, IO_CQ, IO_SQ};
    let mut admin = drv_nvme::admin::Queue::new();
    let (_, ns) = identify(io, s, memory, &mut admin)?;
    let size = ns.lba_bytes as usize;
    let mut line = Irq::bind(resource, s.irq)?;
    let mut stats = Stats::default();
    let stray = quiet(&mut admin, io, &mut line, QUIET_NS, &mut stats)?;
    log(
        kumo_abi::sys::log_level::NOTICE,
        &alloc::format!(
            "NV6 IRQ session={} intid={} pin=INTA client-mask={:#x} bound; vector 0 masked; pre-arm deliveries={}\n",
            s.session, s.irq, s.legacy_mask, stray
        ),
    );
    let mut pair = IoPair::open(&mut admin, io, s, memory, ns, true)?;
    log(
        kumo_abi::sys::log_level::NOTICE,
        &alloc::format!(
            "NV6 QUEUES session={} created qid=1 depth={} sq={:#x} cq={:#x} ien=1 iv=0 allocated-sq={} allocated-cq={}\n",
            s.session,
            drv_nvme::DEPTH,
            memory.physical + IO_SQ,
            memory.physical + IO_CQ,
            pair.allocated.submission,
            pair.allocated.completion
        ),
    );
    unmask(io)?;
    let mut data = [0u8; PAGE as usize];
    let before = stats;
    let t = pair.read_irq(
        io,
        &mut line,
        s,
        memory,
        &[0],
        &mut data[..size],
        &mut stats,
    )?;
    log_block(s, ns.id, 0, "single", &data[..size], t);
    let single = stats.deliveries - before.deliveries;
    let before = stats;
    let lbas = [1, ns.size - 1];
    let t = pair.read_irq(
        io,
        &mut line,
        s,
        memory,
        &lbas,
        &mut data[..2 * size],
        &mut stats,
    )?;
    for (i, &lba) in lbas.iter().enumerate() {
        log_block(s, ns.id, lba, "batch", &data[i * size..(i + 1) * size], t);
    }
    log(
        kumo_abi::sys::log_level::NOTICE,
        &alloc::format!(
            "NV6 BATCH session={} reads=2 doorbell=1 deliveries={} entries={} max-per-delivery={} single-deliveries={}\n",
            s.session,
            stats.deliveries - before.deliveries,
            stats.entries - before.entries,
            stats.max_batch,
            single
        ),
    );
    let rung = io.doorbells;
    match pair.read_irq(io, &mut line, s, memory, &[ns.size], &mut data[..size], &mut stats) {
        Err(Error::Range) if io.doorbells == rung => log(
            kumo_abi::sys::log_level::NOTICE,
            &alloc::format!(
                "NV6 RANGE session={} nsid={} lba={} (NSZE) refused before SQ write/doorbell; doorbells={} unchanged\n",
                s.session, ns.id, ns.size, rung
            ),
        ),
        _ => return Err(Error::Range),
    }
    let late = pair.quiet(io, &mut line, QUIET_NS, &mut stats)?;
    let m = pair.read_masked(io, &mut line, s, memory, 0, &mut data[..size], &mut stats)?;
    log_block(
        s,
        ns.id,
        0,
        "masked",
        &data[..size],
        m.posted_ns + m.unmask_ns,
    );
    log(
        kumo_abi::sys::log_level::NOTICE,
        &alloc::format!(
            "NV6 MASKED session={} lba=0 posted-without-delivery={}us masked-deliveries={} unmask-to-reap={}us\n",
            s.session,
            m.posted_ns / 1000,
            m.deliveries,
            m.unmask_ns / 1000
        ),
    );
    let late = late + pair.quiet(io, &mut line, QUIET_NS, &mut stats)?;
    mask(io)?;
    pair.close(&mut admin, io, s, memory)?;
    drop(line);
    log(
        kumo_abi::sys::log_level::NOTICE,
        &alloc::format!(
            "NV6 IRQ session={} deliveries={} entries={} empty={} late={} pre-arm={} masked={} max-per-delivery={} storm-limit={}; vector masked, sq=1/cq=1 deleted, line released; doorbells={}\n",
            s.session,
            stats.deliveries,
            stats.entries,
            stats.empty,
            late,
            stray,
            m.deliveries,
            stats.max_batch,
            drv_nvme::intx::STORM_LIMIT,
            io.doorbells
        ),
    );
    Ok(())
}

/// Namespace blocks for discovery and the FAT label, read through the INTx path in
/// batches of up to four.
struct NvmeBlocks<'a> {
    io: &'a mut Registers,
    line: &'a mut Irq,
    pair: &'a mut drv_nvme::io::IoPair,
    s: Startup,
    memory: Memory,
    stats: &'a mut drv_nvme::intx::Stats,
}
impl drv_nvme::volume::Blocks for NvmeBlocks<'_> {
    fn read(&mut self, lba: u64, out: &mut [u8]) -> Result<(), Error> {
        use drv_nvme::intx::BATCH;
        if out.len() % SECTOR_SIZE != 0 {
            return Err(Error::Range);
        }
        for (i, chunk) in out.chunks_mut(BATCH * SECTOR_SIZE).enumerate() {
            let base = lba.checked_add((i * BATCH) as u64).ok_or(Error::Range)?;
            let mut lbas = [0u64; BATCH];
            let n = chunk.len() / SECTOR_SIZE;
            for (j, slot) in lbas.iter_mut().take(n).enumerate() {
                *slot = base + j as u64;
            }
            self.pair.read_irq(
                self.io,
                self.line,
                self.s,
                self.memory,
                &lbas[..n],
                chunk,
                self.stats,
            )?;
        }
        Ok(())
    }
}
impl kumo_fatfs::BlockReader for NvmeBlocks<'_> {
    fn read_block(&mut self, lba: u64, buf: &mut [u8; SECTOR_SIZE]) -> bool {
        drv_nvme::volume::Blocks::read(self, lba, buf).is_ok()
    }
}

/// A GPT GUID in its canonical text form (first three fields little-endian on disk).
fn guid_text(g: kumo_gpt::Guid) -> alloc::string::String {
    let b = g.0;
    alloc::format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

#[derive(Clone, Copy, Default)]
struct Client {
    handle: u32,
    koid: u64,
    served: u32,
    refused: u32,
}
const MAX_CLIENTS: usize = 4;

/// NV7: bring the pair up as NV6 does, choose the FAT32 volume, offer it to Sora on the
/// bootstrap channel and serve block clients read-only. Returns Ok when Sora closes the
/// control channel (or no volume exists), after masking vector 0 and deleting the pair,
/// so the caller's proved disable follows. — TIMBERDOODLE
fn serve(
    io: &mut Registers,
    s: Startup,
    memory: Memory,
    resource: Handle,
    control: Handle,
    port: Handle,
    quarantine: Handle,
) -> Result<(), Error> {
    use drv_nvme::intx::{mask, unmask, Stats};
    use drv_nvme::io::IoPair;
    use drv_nvme::volume::{discover, Verdict, MAX_ENTRY_BYTES};
    let mut admin = drv_nvme::admin::Queue::new();
    let (controller, ns) = identify(io, s, memory, &mut admin)?;
    if ns.lba_bytes as usize != SECTOR_SIZE {
        return Err(Error::Data);
    }
    let mut line = Irq::bind(resource, s.irq)?;
    let mut stats = Stats::default();
    let mut pair = IoPair::open(&mut admin, io, s, memory, ns, true)?;
    unmask(io)?;
    let mut entries = alloc::vec![0u8; MAX_ENTRY_BYTES];
    let (found, label) = {
        let mut blocks = NvmeBlocks {
            io: &mut *io,
            line: &mut line,
            pair: &mut pair,
            s,
            memory,
            stats: &mut stats,
        };
        let found = discover(&mut blocks, ns.size, &mut entries)?;
        let label = match found.chosen {
            Some((part, _)) => {
                let mut reader =
                    PartitionReader::new(&mut blocks, part.first_lba, part.block_count());
                let fat = FatVolume::mount(&mut reader).map_err(|_| Error::Data)?;
                fat.volume_label(&mut reader)
            }
            None => None,
        };
        (found, label)
    };
    log(
        kumo_abi::sys::log_level::NOTICE,
        &alloc::format!(
            "NV7 GPT session={} protective-mbr=ok header=ok entries-crc=ok partitions={} reads={}\n",
            s.session, found.entries, stats.entries
        ),
    );
    for entry in found.reported.iter().flatten() {
        let p = entry.partition;
        log(
            kumo_abi::sys::log_level::NOTICE,
            &alloc::format!(
                "NV7 PART session={} index={} type={} first-lba={} blocks={} verdict={} oem={}\n",
                s.session,
                p.index,
                guid_text(p.type_guid),
                p.first_lba,
                p.block_count(),
                match entry.verdict {
                    Verdict::Skipped => "not-fat-type",
                    Verdict::NotFat32 => "not-fat32",
                    Verdict::Chosen => "chosen",
                    Verdict::Unprobed => "unprobed",
                },
                match entry.oem {
                    Some(oem) => oem
                        .iter()
                        .map(|&b| if (0x21..0x7f).contains(&b) {
                            b as char
                        } else {
                            '.'
                        })
                        .collect::<alloc::string::String>(),
                    None => alloc::string::String::from("-"),
                }
            ),
        );
    }
    let Some((part, bpb)) = found.chosen else {
        log(
            kumo_abi::sys::log_level::NOTICE,
            &alloc::format!(
                "NV7 VOLUME session={} none: no FAT32 Basic Data or ESP partition; nothing offered\n",
                s.session
            ),
        );
        mask(io)?;
        pair.close(&mut admin, io, s, memory)?;
        return Ok(());
    };
    let ready = MediaReady {
        block_count: part.block_count(),
        serial: bpb.volume_serial,
        label,
        read_only: true,
    };
    let label_text = label
        .map(|l| alloc::string::String::from_utf8_lossy(l.trim_ascii_end()).into_owned())
        .unwrap_or_else(|| alloc::string::String::from("(none)"));
    log(
        kumo_abi::sys::log_level::NOTICE,
        &alloc::format!(
            "NV7 VOLUME session={} index={} first-lba={} blocks={} fat32 serial={:04X}-{:04X} label={} read-only; offered\n",
            s.session,
            part.index,
            part.first_lba,
            part.block_count(),
            bpb.volume_serial >> 16,
            bpb.volume_serial & 0xffff,
            label_text
        ),
    );
    let offer = ready.encode();
    if channel_write(control, offer.as_ptr(), offer.len()) != 0 {
        return Err(Error::Bootstrap);
    }
    let control_koid = handle_koid(control);
    let mut clients = [Client::default(); MAX_CLIENTS];
    let mut shutdown = false;
    loop {
        let source = port_wait(port);
        if source == control_koid {
            let mut tag = [0u8; CONTROL_CONNECT_LEN + 1];
            let (len, raw) = channel_read_with_handle(control, tag.as_mut_ptr(), tag.len());
            if len == 0 {
                log(
                    kumo_abi::sys::log_level::NOTICE,
                    &alloc::format!(
                        "NV7 SERVICE session={} control closed; stopping after {} reads\n",
                        s.session,
                        stats.entries
                    ),
                );
                break;
            }
            if len > tag.len() {
                return Err(Error::Bootstrap);
            }
            if len == 1
                && matches!(tag[0], drv_nvme::CONTROL_STOP | drv_nvme::CONTROL_SHUTDOWN)
                && raw == 0
            {
                shutdown = tag[0] == drv_nvme::CONTROL_SHUTDOWN;
                log(
                    kumo_abi::sys::log_level::NOTICE,
                    &alloc::format!(
                        "NV8 STOP session={} supervisor requested quiescent teardown\n",
                        s.session
                    ),
                );
                break;
            }
            let Some(client) = syscall_handle(raw) else {
                continue;
            };
            let access = decode_connect(&tag[..len]);
            let slot = clients.iter().position(|c| c.handle == 0);
            let koid = handle_koid(client);
            match (access, slot) {
                (Some(access), Some(slot)) if port_bind(port, client) == 0 => {
                    clients[slot] = Client {
                        handle: client.0,
                        koid,
                        ..Client::default()
                    };
                    log(
                        kumo_abi::sys::log_level::INFO,
                        &alloc::format!(
                            "NV7 CLIENT session={} slot={} connected asked={} served read-only\n",
                            s.session,
                            slot,
                            if access == BlockAccess::ReadOnly {
                                "ro"
                            } else {
                                "rw"
                            }
                        ),
                    );
                }
                _ => {
                    let _ = handle_close(client);
                }
            }
            continue;
        }
        let Some(i) = clients
            .iter()
            .position(|c| c.handle != 0 && c.koid == source)
        else {
            continue;
        };
        let client = Handle(clients[i].handle);
        let mut frame = [0u8; REQUEST_LEN + SECTOR_SIZE + 1];
        let n = channel_read(client, frame.as_mut_ptr(), frame.len());
        if n == 0 || n > frame.len() as u64 {
            let _ = port_unbind(port, client);
            let _ = handle_close(client);
            log(
                kumo_abi::sys::log_level::INFO,
                &alloc::format!(
                    "NV7 CLIENT session={} slot={} closed served={} refused={}\n",
                    s.session,
                    i,
                    clients[i].served,
                    clients[i].refused
                ),
            );
            clients[i] = Client::default();
            continue;
        }
        let rung = io.doorbells;
        let mut reply = [0u8; 1 + SECTOR_SIZE];
        let mut reply_len = 1;
        reply[0] = match volume_request(
            &frame[..n as usize],
            part.first_lba,
            part.block_count(),
            true,
        ) {
            Ok(VolumeRequest::Read(lba)) => {
                match pair.read_irq(
                    io,
                    &mut line,
                    s,
                    memory,
                    &[lba],
                    &mut reply[1..],
                    &mut stats,
                ) {
                    Ok(_) => {
                        reply_len = reply.len();
                        clients[i].served += 1;
                        STATUS_OK
                    }
                    Err(e) => {
                        // The queue is poisoned: answer, then stop with everything retained.
                        let failed = [STATUS_IO_ERROR];
                        let _ = channel_write(client, failed.as_ptr(), failed.len());
                        return Err(e);
                    }
                }
            }
            // Nothing is ever buffered for a read-only volume.
            Ok(VolumeRequest::Flush) => STATUS_OK,
            Ok(VolumeRequest::Write(..)) => STATUS_READ_ONLY,
            Err(status) => {
                clients[i].refused += 1;
                status
            }
        };
        if reply[0] == STATUS_READ_ONLY {
            if io.doorbells != rung {
                return Err(Error::State);
            }
            log(
                kumo_abi::sys::log_level::NOTICE,
                &alloc::format!(
                    "NV8 RO session={} slot={} write refused before I/O; doorbells={} unchanged\n",
                    s.session,
                    i,
                    rung
                ),
            );
        }
        let _ = channel_write(client, reply.as_ptr(), reply_len);
    }
    for c in clients.iter().filter(|c| c.handle != 0) {
        let _ = port_unbind(port, Handle(c.handle));
        let _ = handle_close(Handle(c.handle));
    }
    if shutdown {
        let elapsed = pair.flush_irq(io, &mut line, s, memory, &mut stats)?;
        log(
            kumo_abi::sys::log_level::NOTICE,
            &alloc::format!(
                "NV8 SHUTDOWN session={} Flush completed vwc={} elapsed={}us\n",
                s.session,
                controller.vwc,
                elapsed / 1000
            ),
        );
    }
    mask(io)?;
    pair.close(&mut admin, io, s, memory)?;
    if shutdown {
        let report = drv_nvme::shutdown::normal(io)?;
        let ready = drv_nvme::shutdown::Ready {
            session: s.session,
            cc: report.cc,
            csts: report.csts,
        };
        log(kumo_abi::sys::log_level::NOTICE,&alloc::format!("NV8 SHUTDOWN session={} cc={:#x} SHN=1 csts={:#x} SHST=2 elapsed={}us; IO queues deleted; DMA retained until reset\n",s.session,report.cc,report.csts,report.elapsed_ns/1000));
        let frame = ready.encode();
        if channel_write(control, frame.as_ptr(), frame.len()) != 0 {
            return Err(Error::Bootstrap);
        }
        // Do not return into exercise_with's Controller Reset: preserve SHST2
        // and every DMA allocation until actual power transition. — TIMBERDOODLE
        loop {
            let _ = port_wait(quarantine);
        }
    }
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
    let p = exercise_with(&mut io, s, memory, |io| {
        if s.mode == MODE_SERVE {
            serve(io, s, memory, resource, control, port, quarantine)
        } else if s.mode == MODE_WRITE_PROOF {
            write_proof(io, s, memory, resource)
        } else {
            media(io, s, memory, resource)
        }
    });
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
