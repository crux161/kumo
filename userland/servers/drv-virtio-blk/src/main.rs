#![no_std]
#![no_main]
//j930
//! `drv-virtio-blk`: a virtio-blk disk over modern virtio-mmio (SH8-d, PLAN/008 S3), served
//! like any offered volume.
//!
//! Bootstrap: on the bootstrap channel, `[b'R'][page: u64][offset: u32][irq: u32]` (LE) carries
//! a Resource bounded to the transport's MMIO page and its one interrupt line. The driver mints
//! both itself (the slot sits at `offset` inside the page). The channel then becomes the
//! storage control channel. The driver announces the disk
//! (`MediaReady`, label `DISK`) and serves every client Sora connects, with the access Sora
//! fixed for it.
//!
//! One request is in flight at a time, completed by the interrupt; nothing polls, because a
//! polling child starves Sora. A disk without `VIRTIO_BLK_F_FLUSH` cannot keep a write barrier,
//! so it is offered read-only. — TEAL

use kumo_abi::{Handle, VmarFlags};
use kumo_rt::{
    channel_read_with_handle, channel_write, handle_close, handle_koid, interrupt_complete,
    interrupt_create, interrupt_wait, port_bind, port_create, port_unbind, port_wait,
    resource_mint_mmio, vmar_map, vmo_create_contiguous,
};
use kumo_virtio::blk::{
    acknowledge_interrupt, read_capacity, BlockQueue, DmaRegion, Op, Outcome, RegionLayout,
    QUEUE_SIZE,
};
use kumo_virtio::{activate_block_queue, negotiate_writable_block, RegisterIo, SplitQueueMemory};

kumo_rt::entry!(main);

fn log_info(message: &[u8]) {
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::INFO, message);
}

fn fail(message: &[u8]) -> ! {
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::ERR, message);
    kumo_rt::process_exit(1)
}

const MMIO_VA: u64 = kumo_rt::USER_WINDOW_BASE + 0x0000_0000_1100_0000;
const DMA_VA: u64 = kumo_rt::USER_WINDOW_BASE + 0x0000_0000_1200_0000;
const MAX_CLIENTS: usize = 4;
/// Announced serial (the name of last resort): "VIRT".
const SERIAL: u32 = u32::from_le_bytes(*b"VIRT");
const LABEL: [u8; 11] = *b"DISK       ";

struct Mmio(u64);

impl RegisterIo for Mmio {
    fn read32(&mut self, offset: u32) -> u32 {
        // SAFETY: `self.0` maps the transport's registers (Device memory) for this process.
        unsafe { ((self.0 + u64::from(offset)) as *const u32).read_volatile() }
    }

    fn write32(&mut self, offset: u32, value: u32) {
        // SAFETY: as `read32`.
        unsafe { ((self.0 + u64::from(offset)) as *mut u32).write_volatile(value) }
    }
}

struct Dma {
    va: u64,
    phys: u64,
    len: usize,
}

impl DmaRegion for Dma {
    fn device_addr(&self) -> u64 {
        self.phys
    }

    fn write(&mut self, offset: usize, bytes: &[u8]) {
        assert!(offset + bytes.len() <= self.len);
        // SAFETY: inside this process's private DMA mapping; volatile so the device sees each
        // byte (the J539/J540 noalias lesson).
        for (i, b) in bytes.iter().enumerate() {
            unsafe { ((self.va + (offset + i) as u64) as *mut u8).write_volatile(*b) }
        }
    }

    fn read(&mut self, offset: usize, out: &mut [u8]) {
        assert!(offset + out.len() <= self.len);
        for (i, b) in out.iter_mut().enumerate() {
            // SAFETY: as `write`.
            *b = unsafe { ((self.va + (offset + i) as u64) as *const u8).read_volatile() };
        }
    }

    fn barrier(&mut self) {
        // Order DMA-visible accesses against each other and the doorbell (outer shareable).
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!("dmb osh", options(nostack, preserves_flags));
        }
    }
}

struct Disk {
    io: Mmio,
    mem: Dma,
    queue: BlockQueue,
    irq: Handle,
    irq_port: Handle,
    irq_koid: u64,
}

impl Disk {
    /// Run one request to completion: submit, then sleep on the interrupt until the used ring
    /// moves. Every wake acknowledges the device and unmasks the line. A 3 s watchdog recovers
    /// a request whose interrupt never arrives.
    fn run(&mut self, op: Op) -> Outcome {
        if self.queue.submit(&mut self.mem, &mut self.io, op).is_err() {
            return Outcome::IoError;
        }
        let mut watchdog: Option<(Handle, u64)> = None;
        loop {
            if let Some(outcome) = self.queue.complete(&mut self.mem) {
                if let Some((timer, _)) = watchdog {
                    let _ = port_unbind(self.irq_port, timer);
                    let _ = handle_close(timer);
                }
                return outcome;
            }
            if watchdog.is_none() {
                if let Some(timer) = valid(kumo_rt::timer_create(3_000_000_000)) {
                    let koid = handle_koid(timer);
                    if port_bind(self.irq_port, timer) == 0 {
                        watchdog = Some((timer, koid));
                    }
                }
            }
            let source = port_wait(self.irq_port);
            if watchdog.is_some_and(|(_, koid)| koid == source) {
                // A request that has not completed in 3 s: say so, then recover from a lost or
                // masked interrupt by acknowledging the device and unmasking the line. The
                // used ring is checked again at the top of the loop. — TEAL (j930)
                let status = acknowledge_interrupt(&mut self.io);
                interrupt_complete(self.irq);
                let _ = kumo_rt::log_write(
                    kumo_abi::sys::log_level::NOTICE,
                    &hex_line(
                        b"drv-virtio-blk: request stalled 3 s; interrupt re-armed, status=",
                        status,
                    ),
                );
                if let Some((timer, _)) = watchdog.take() {
                    let _ = port_unbind(self.irq_port, timer);
                    let _ = handle_close(timer);
                }
                continue;
            }
            if source != self.irq_koid {
                continue;
            }
            interrupt_wait(self.irq);
            acknowledge_interrupt(&mut self.io);
            interrupt_complete(self.irq);
        }
    }
}

/// `prefix`, a hex word and a newline, without an allocator.
fn hex_line(prefix: &[u8], value: u32) -> heapless_line::Line {
    heapless_line::Line::new(prefix, value)
}

mod heapless_line {
    /// One short log line on the stack.
    pub struct Line {
        bytes: [u8; 96],
        len: usize,
    }

    impl Line {
        pub fn new(prefix: &[u8], value: u32) -> Self {
            let mut bytes = [0u8; 96];
            let n = prefix.len().min(80);
            bytes[..n].copy_from_slice(&prefix[..n]);
            for i in 0..8 {
                bytes[n + i] = b"0123456789abcdef"[((value >> (28 - 4 * i)) & 0xF) as usize];
            }
            bytes[n + 8] = b'\n';
            Self { bytes, len: n + 9 }
        }
    }

    impl core::ops::Deref for Line {
        type Target = [u8];
        fn deref(&self) -> &[u8] {
            &self.bytes[..self.len]
        }
    }
}

fn status(outcome: Outcome) -> u8 {
    match outcome {
        Outcome::Ok => drv_blk::STATUS_OK,
        _ => drv_blk::STATUS_IO_ERROR,
    }
}

/// Where a bulk request's window of the client's VMO is mapped.
const CLIENT_VA: u64 = kumo_rt::USER_WINDOW_BASE + 0x0000_0000_1300_0000;

/// j930: map `len` bytes of the client's `vmo` from `offset` (page-aligned window), hand them to
/// `f`, unmap. `None` when the window cannot be mapped.
fn with_window<R>(
    vmo: Handle,
    offset: u64,
    len: usize,
    f: impl FnOnce(&mut [u8]) -> R,
) -> Option<R> {
    let page = offset & !0xFFF;
    let delta = (offset - page) as usize;
    let map_len = ((delta + len) as u64).next_multiple_of(0x1000);
    if map_len > 0x2_0000 {
        return None;
    }
    if vmar_map(
        Handle(0),
        vmo,
        page,
        CLIENT_VA,
        map_len,
        (VmarFlags::READ | VmarFlags::WRITE).0,
    ) != 0
    {
        return None;
    }
    // SAFETY: the kernel just mapped `map_len >= delta + len` bytes at CLIENT_VA for us alone.
    let window =
        unsafe { core::slice::from_raw_parts_mut((CLIENT_VA as usize + delta) as *mut u8, len) };
    let result = f(window);
    let _ = kumo_rt::vmar_unmap(CLIENT_VA, map_len);
    Some(result)
}

/// Serve one volume frame from a client with `read_only` access. `vmo` is the handle a bulk
/// request carried (the caller closes it).
fn serve(
    disk: &mut Disk,
    flush: bool,
    frame: &[u8],
    vmo: Option<Handle>,
    read_only: bool,
    reply: &mut [u8],
) -> usize {
    let blocks = disk.queue.capacity_sectors;
    let code = match drv_blk::volume_request(frame, 0, blocks, read_only || !flush) {
        Ok(drv_blk::VolumeRequest::Read(lba)) => {
            let outcome = disk.run(Op::Read {
                sector: lba,
                count: 1,
            });
            if outcome == Outcome::Ok {
                disk.queue.data(&mut disk.mem, &mut reply[1..513]);
                reply[0] = drv_blk::STATUS_OK;
                return 513;
            }
            status(outcome)
        }
        Ok(drv_blk::VolumeRequest::Write(lba, data)) => {
            disk.queue.set_data(&mut disk.mem, data);
            status(disk.run(Op::Write {
                sector: lba,
                count: 1,
            }))
        }
        // The barrier RedoxFS's commit order rests on: the host's flush, not just the queue's.
        Ok(drv_blk::VolumeRequest::Flush) if flush => status(disk.run(Op::Flush)),
        Ok(drv_blk::VolumeRequest::Flush) => drv_blk::STATUS_OK,
        // j930: one virtio request per bulk command (up to 64 KiB), through the driver's own DMA
        // buffer: the client's pages never reach the device.
        Ok(drv_blk::VolumeRequest::ReadVmo {
            lba,
            count,
            vmo_offset,
        }) => match vmo {
            None => drv_blk::STATUS_BAD_LEN,
            Some(vmo) => {
                let outcome = disk.run(Op::Read {
                    sector: lba,
                    count: u32::from(count),
                });
                if outcome == Outcome::Ok {
                    let len = usize::from(count) * drv_blk::BLOCK_SIZE as usize;
                    let Disk { queue, mem, .. } = &mut *disk;
                    match with_window(vmo, vmo_offset, len, |window| queue.data(mem, window)) {
                        Some(()) => drv_blk::STATUS_OK,
                        None => drv_blk::STATUS_BAD_LEN,
                    }
                } else {
                    status(outcome)
                }
            }
        },
        Ok(drv_blk::VolumeRequest::WriteVmo {
            lba,
            count,
            vmo_offset,
        }) => match vmo {
            None => drv_blk::STATUS_BAD_LEN,
            Some(vmo) => {
                let len = usize::from(count) * drv_blk::BLOCK_SIZE as usize;
                let Disk { queue, mem, .. } = &mut *disk;
                match with_window(vmo, vmo_offset, len, |window| queue.set_data(mem, window)) {
                    Some(()) => status(disk.run(Op::Write {
                        sector: lba,
                        count: u32::from(count),
                    })),
                    None => drv_blk::STATUS_BAD_LEN,
                }
            }
        },
        Err(code) => code,
    };
    reply[0] = code;
    1
}

fn valid(raw: u64) -> Option<Handle> {
    (raw != 0 && raw != u64::MAX).then_some(Handle(raw as u32))
}

#[no_mangle]
extern "C" fn main(
    _arg0: u64,
    bootstrap: u64,
    _arg2: u64,
    _arg3: u64,
    _arg4: u64,
    _arg5: u64,
    _arg6: u64,
    _arg7: u64,
) -> ! {
    log_info(b"drv-virtio-blk starting\n");
    let control = Handle(bootstrap as u32);
    let mut msg = [0u8; 17];
    let (n, raw) = channel_read_with_handle(control, msg.as_mut_ptr(), msg.len());
    let (Some(resource), true) = (valid(raw), n == 17 && msg[0] == b'R') else {
        fail(b"drv-virtio-blk: bootstrap carried no transport resource\n");
    };
    let page = u64::from_le_bytes(msg[1..9].try_into().unwrap());
    let offset = u32::from_le_bytes(msg[9..13].try_into().unwrap());
    let irq_line = u32::from_le_bytes(msg[13..17].try_into().unwrap());
    let Some(mmio) = valid(resource_mint_mmio(resource, page, 0x1000)) else {
        fail(b"drv-virtio-blk: MMIO mint failed\n");
    };
    let Some(irq) = valid(interrupt_create(resource, irq_line)) else {
        fail(b"drv-virtio-blk: interrupt create failed\n");
    };
    if offset >= 0x1000
        || vmar_map(
            Handle(0),
            mmio,
            0,
            MMIO_VA,
            0x1000,
            (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::DEVICE).0,
        ) != 0
    {
        fail(b"drv-virtio-blk: MMIO map failed\n");
    }
    let mut io = Mmio(MMIO_VA + u64::from(offset));
    let negotiated = match negotiate_writable_block(&mut io) {
        Ok(negotiated) => negotiated,
        Err(_) => fail(b"drv-virtio-blk: not a writable modern virtio-blk device\n"),
    };
    let Ok(layout) = RegionLayout::new() else {
        fail(b"drv-virtio-blk: bad region layout\n");
    };
    let (dma_raw, phys) = vmo_create_contiguous(layout.total_len as u64);
    let Some(dma) = valid(dma_raw) else {
        fail(b"drv-virtio-blk: no contiguous DMA memory\n");
    };
    if vmar_map(
        Handle(0),
        dma,
        0,
        DMA_VA,
        layout.total_len as u64,
        (VmarFlags::READ | VmarFlags::WRITE).0,
    ) != 0
    {
        fail(b"drv-virtio-blk: DMA map failed\n");
    }
    let mem = Dma {
        va: DMA_VA,
        phys,
        len: layout.total_len,
    };
    let active = match activate_block_queue(
        &mut io,
        negotiated,
        0,
        SplitQueueMemory {
            device_addr: phys,
            len: layout.header_offset as u64,
            preferred_size: QUEUE_SIZE,
        },
    ) {
        Ok(active) => active,
        Err(_) => fail(b"drv-virtio-blk: queue activation failed\n"),
    };
    let capacity = read_capacity(&mut io);
    let flush = negotiated.supports_flush();
    let Some(irq_port) = valid(port_create()) else {
        fail(b"drv-virtio-blk: irq port failed\n");
    };
    let irq_koid = handle_koid(irq);
    if irq_koid == u64::MAX || port_bind(irq_port, irq) != 0 {
        fail(b"drv-virtio-blk: irq bind failed\n");
    }
    // Release anything a fire before the bind left masked or latched.
    acknowledge_interrupt(&mut io);
    interrupt_complete(irq);
    let mut disk = Disk {
        io,
        mem,
        queue: BlockQueue::new(active, layout, capacity),
        irq,
        irq_port,
        irq_koid,
    };

    let Some(port) = valid(port_create()) else {
        fail(b"drv-virtio-blk: port failed\n");
    };
    let control_koid = handle_koid(control);
    if control_koid == u64::MAX || port_bind(port, control) != 0 {
        fail(b"drv-virtio-blk: control bind failed\n");
    }
    let ready = drv_blk::MediaReady {
        block_count: capacity,
        serial: SERIAL,
        label: Some(LABEL),
        read_only: !flush,
    }
    .encode();
    if channel_write(control, ready.as_ptr(), ready.len()) != 0 {
        fail(b"drv-virtio-blk: announce failed\n");
    }
    log_info(if flush {
        b"drv-virtio-blk: disk offered (read-write, flush barrier)\n" as &[u8]
    } else {
        b"drv-virtio-blk: disk offered read-only (the device has no flush)\n"
    });

    let mut clients: [Option<(Handle, u64, bool)>; MAX_CLIENTS] = [None; MAX_CLIENTS];
    let mut frame = [0u8; drv_blk::REQUEST_LEN + drv_blk::BLOCK_SIZE as usize + 16];
    let mut reply = [0u8; 1 + drv_blk::BLOCK_SIZE as usize];
    loop {
        let source = port_wait(port);
        if source == control_koid {
            let mut connect = [0u8; drv_blk::CONTROL_CONNECT_LEN];
            let (n, raw) = channel_read_with_handle(control, connect.as_mut_ptr(), connect.len());
            let carried = valid(raw);
            if n == 0 {
                let _ = port_unbind(port, control);
                continue;
            }
            let (Some(access), Some(channel)) = (
                drv_blk::decode_connect(&connect[..n.min(connect.len())]),
                carried,
            ) else {
                if let Some(extra) = carried {
                    let _ = handle_close(extra);
                }
                continue;
            };
            let koid = handle_koid(channel);
            match clients.iter().position(Option::is_none) {
                Some(slot) if koid != u64::MAX && port_bind(port, channel) == 0 => {
                    clients[slot] = Some((channel, koid, access == drv_blk::BlockAccess::ReadOnly));
                }
                _ => {
                    log_info(b"drv-virtio-blk: client refused\n");
                    let _ = handle_close(channel);
                }
            }
        } else if let Some(slot) = clients
            .iter()
            .position(|c| c.is_some_and(|(_, koid, _)| koid == source))
        {
            let Some((channel, _, read_only)) = clients[slot] else {
                continue;
            };
            let (n, raw) = channel_read_with_handle(channel, frame.as_mut_ptr(), frame.len());
            let carried = valid(raw);
            if n == 0 || n > frame.len() {
                if let Some(extra) = carried {
                    let _ = handle_close(extra);
                }
                let _ = port_unbind(port, channel);
                let _ = handle_close(channel);
                clients[slot] = None;
                continue;
            }
            let len = serve(
                &mut disk,
                flush,
                &frame[..n],
                carried,
                read_only,
                &mut reply,
            );
            if let Some(vmo) = carried {
                let _ = handle_close(vmo);
            }
            let _ = channel_write(channel, reply.as_ptr(), len);
        }
    }
}
