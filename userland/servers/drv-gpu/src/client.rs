//j595
//j596
//j636
//j644

//! One brokered G6b session leases the prepared AS1/CSG0 after the boot proof. No second
//! grant or automatic restart exists in this lane. The resident driver retains every DMA
//! mapping even after a reset failure, so losing the client cannot free live GPU backing.

use super::*;
use drv_gpu::{client_job_wrapper, protocol, JobSlot, JOB_WRAPPER_WORDS};
use protocol::{
    session::{Backend, Session, SubmitError, MAX_BUFFERS},
    Binding, Reply as GpuReply, Request, Response, Status as GpuStatus, Submission, REQUEST_LEN,
};

struct Pending {
    point: u64,
    seqno: u64,
    insert: u64,
    slot: JobSlot,
    words: [u64; JOB_WRAPPER_WORDS],
}

struct Hardware<'a> {
    device: &'a mut Device,
    shader_present: u64,
    tables: &'a mut TableDma,
    arena: &'a mut TableArena,
    root: u64,
    work: Mapping,
    input: u64,
    output: u64,
    windows: [McuWindow; 3],
    csg: CsgControl,
    cs: CsControl,
    buffers: [Option<Mapping>; MAX_BUFFERS],
    reply_grant: Option<Handle>,
    pending: Option<Pending>,
}

impl Backend for Hardware<'_> {
    fn info(&mut self) -> Result<protocol::DeviceInfo, GpuStatus> {
        // GPU_ID at aperture +0, already checked by probe before this server starts.
        Ok(protocol::DeviceInfo {
            gpu_id: read_aperture(0),
            cs_features: self.cs.features,
        })
    }
    fn topology(&mut self) -> Result<protocol::DeviceTopology, GpuStatus> {
        // The exact probe-attested mask also configured this session's CSG. No client
        // field selects a register, core mask or hardware slot. — KESTREL
        Ok(protocol::DeviceTopology {
            shader_present: self.shader_present,
        })
    }
    fn create_vm(&mut self) -> Result<(), GpuStatus> {
        log(
            log_level::NOTICE,
            format_args!("drv-gpu: client VM 1 leases AS1\n"),
        );
        Ok(())
    }

    fn create_buffer(&mut self, id: u32, bytes: u64) -> Result<(), GpuStatus> {
        let mapping = map_contiguous(bytes).ok_or(GpuStatus::NoMemory)?;
        self.buffers[id as usize - 1] = Some(mapping);
        zero_mapping(&mapping, bytes);
        dsb();
        self.reply_grant = valid_handle(kumo_rt::handle_duplicate(
            mapping.handle,
            kumo_abi::Rights::READ
                | kumo_abi::Rights::WRITE
                | kumo_abi::Rights::DUPLICATE
                | kumo_abi::Rights::TRANSFER,
        ));
        if self.reply_grant.is_none() {
            return Err(GpuStatus::DeviceLost);
        }
        log(
            log_level::INFO,
            format_args!("drv-gpu: client buffer {} shared, {} bytes\n", id, bytes),
        );
        Ok(())
    }

    fn bind(&mut self, b: Binding) -> Result<(), GpuStatus> {
        let mapping = self.buffers[b.buffer as usize - 1].ok_or(GpuStatus::NotFound)?;
        let attrs = PageAttrs {
            cached: !b.flags.contains(protocol::BindFlags::UNCACHED),
            readonly: b.flags.contains(protocol::BindFlags::READ_ONLY),
            executable: !b.flags.contains(protocol::BindFlags::NO_EXECUTE),
        };
        // A partial table update or failed live flush retires this session; its ranges are
        // never reused. Reset containment runs before any handle is closed. — TIMBERDOODLE
        if let Err(error) = map_range(
            self.tables,
            self.arena,
            self.root,
            b.va,
            mapping.phys + b.offset,
            b.bytes,
            attrs,
        ) {
            log(
                log_level::ERR,
                format_args!("drv-gpu: client map failed: {:?}\n", error),
            );
            return Err(GpuStatus::DeviceLost);
        }
        dsb();
        flush_page_tables(self.device, GROUP_AS).map_err(|_| GpuStatus::DeviceLost)?;
        Ok(())
    }

    fn create_group(&mut self) -> Result<(), GpuStatus> {
        log(
            log_level::NOTICE,
            format_args!(
                "drv-gpu: client group 1 leases CSG0 queue0, hardware seqno base={}\n",
                PAINT_JOB
            ),
        );
        Ok(())
    }

    fn submit(&mut self, job: Submission) -> Result<(), SubmitError> {
        if self.pending.is_some() {
            return Err(SubmitError::Unpublished(GpuStatus::Busy));
        }
        let seqno = PAINT_JOB
            .checked_add(job.signal_point)
            .ok_or(SubmitError::Unpublished(GpuStatus::BadRequest))?;
        dsb();
        let words = client_job_wrapper(
            CsFeatures::decode(self.cs.features),
            JobCall {
                latest_flush: latest_flush_id(self.device),
                stream_va: job.stream_va,
                stream_len: job.stream_bytes,
                sync_va: AS1_SYNC_VA,
            },
        )
        .map_err(|_| SubmitError::Unpublished(GpuStatus::BadRequest))?;
        let iface = QueueIface::at(MCU_IO_PAGES_VA);
        let mut io = SharedGlb::new(self.device, self.input, self.output, self.windows);
        let insert = queue_insert(&mut io, iface);
        let extract = queue_extract(&mut io, iface);
        let slot = job_slot(insert, extract, RING_SIZE)
            .ok_or(SubmitError::Unpublished(GpuStatus::Busy))?;
        // Prepare only. Publish after the acceptance reply, with the interrupt already armed.
        self.pending = Some(Pending {
            point: job.signal_point,
            seqno,
            insert,
            slot,
            words,
        });
        Ok(())
    }
}

impl Hardware<'_> {
    fn finish_pending(&mut self) -> Result<Option<u64>, GpuStatus> {
        let Some(job) = self.pending.take() else {
            return Ok(None);
        };
        let csg_bit = job_irq_csg(GROUP_SLOT);
        let iface = QueueIface::at(MCU_IO_PAGES_VA);
        let sync = self.work.va + (AS1_SYNC_VA - AS1_WORK_VA);
        let (input, output, windows, csg, cs, work) = (
            self.input,
            self.output,
            self.windows,
            self.csg,
            self.cs,
            self.work,
        );
        {
            let mut io = SharedGlb::new(self.device, input, output, windows);
            ack_group_events(&mut io, csg.input_va, csg.output_va);
        }
        clear_job_irqs(self.device, csg_bit | JOB_IRQ_GLOBAL_IF);
        let started = self.device.now_ns();
        let deadline = started.saturating_add(WORK_TIMEOUT_NS);
        let (mut waits, mut fired) = (0, 0);
        let (last, seqno, status) = loop {
            let publish = waits == 0;
            let remaining = deadline.saturating_sub(self.device.now_ns()).max(1_000);
            let irq = await_job_irq(self.device, csg_bit, remaining, |device| {
                if !publish {
                    return;
                }
                write_job_words(
                    job.insert,
                    job.slot,
                    &job.words,
                    RING_SIZE,
                    |offset, word| {
                        // SAFETY: write_job_words bounds each word to the mapped ring page.
                        unsafe { ((work.va + offset as u64) as *mut u64).write_volatile(word) };
                    },
                );
                dsb();
                {
                    let mut io = SharedGlb::new(&mut *device, input, output, windows);
                    publish_insert(&mut io, iface, job.slot.end);
                }
                dsb();
                {
                    let mut io = SharedGlb::new(&mut *device, input, output, windows);
                    wake_stream(&mut io, csg.input_va, csg.output_va, GROUP_SLOT, 0);
                }
                device.write32(csf_doorbell(GROUP_SLOT + 1), 1);
            })
            .map_err(|_| GpuStatus::DeviceLost)?;
            waits += 1;
            if irq.completion == Completion::Fired && irq.rawstat & csg_bit != 0 {
                fired += 1;
            }
            {
                let mut io = SharedGlb::new(&mut *self.device, input, output, windows);
                ack_group_events(&mut io, csg.input_va, csg.output_va);
            }
            // SAFETY: aligned words of the driver's retained Normal-NC queue sync page.
            let seqno = unsafe { (sync as *const u64).read_volatile() };
            let status = unsafe { ((sync + SYNC_STATUS_OFFSET) as *const u32).read_volatile() };
            if seqno >= job.seqno
                || irq.completion == Completion::TimedOut
                || self.device.now_ns() >= deadline
            {
                break (irq.completion, seqno, status);
            }
        };
        let outcome = classify_job_wait(seqno >= job.seqno, last, fired);
        if outcome != JobWait::Interrupt || seqno != job.seqno || status != 0 {
            log(
                log_level::ERR,
                format_args!(
                    "drv-gpu: client point {} failed: {:?} seqno={} status=0x{:x}\n",
                    job.point, outcome, seqno, status
                ),
            );
            report_stream_stall(self.device, input, output, windows, csg, cs);
            return Err(GpuStatus::DeviceLost);
        }
        log(
            log_level::NOTICE,
            format_args!(
                "drv-gpu: client point {} complete on JOB: hw_seqno={} status=0x0 ({} us)\n",
                job.point,
                seqno,
                (self.device.now_ns() - started) / 1_000
            ),
        );
        Ok(Some(job.point))
    }
}

pub(super) fn serve(
    channel: Handle,
    device: &mut Device,
    tables: &mut TableDma,
    arena: &mut TableArena,
    root: u64,
    work: Mapping,
    input: u64,
    output: u64,
    windows: [McuWindow; 3],
    csg: CsgControl,
    cs: CsControl,
    shader_present: u64,
) -> ! {
    let mut hw = Hardware {
        device,
        shader_present,
        tables,
        arena,
        root,
        work,
        input,
        output,
        windows,
        csg,
        cs,
        buffers: [None; MAX_BUFFERS],
        reply_grant: None,
        pending: None,
    };
    let mut session = Session::default();
    log(
        log_level::NOTICE,
        format_args!("drv-gpu: client session ready\n"),
    );
    loop {
        let mut bytes = [0u8; REQUEST_LEN + 1];
        let (len, raw_handle) =
            kumo_rt::channel_read_with_handle(channel, bytes.as_mut_ptr(), bytes.len());
        let unexpected = valid_handle(raw_handle);
        if let Some(handle) = unexpected {
            let _ = kumo_rt::handle_close(handle);
        }
        if len == 0 || len > bytes.len() {
            break;
        }
        let request = bytes.get(..len).and_then(Request::decode);
        let (id, result) = match request {
            Some(request) if unexpected.is_none() => {
                (request.id, session.dispatch(request, &mut hw))
            }
            _ => (0, Err(GpuStatus::BadRequest)),
        };
        let retired = result == Err(GpuStatus::DeviceLost);
        let grant = if matches!(result, Ok(Response::BufferCreated { .. })) {
            hw.reply_grant.take()
        } else {
            None
        };
        let frame = GpuReply { id, result }.encode();
        let sent = kumo_rt::channel_write_with_handle(
            channel,
            frame.as_ptr(),
            frame.len(),
            grant.unwrap_or(Handle(0)),
        );
        if sent != 0 {
            if let Some(handle) = grant {
                let _ = kumo_rt::handle_close(handle);
            }
            break;
        }
        if retired {
            break;
        }
        match hw.finish_pending() {
            Ok(Some(point)) => {
                if session.complete(point, 0).is_err() {
                    break;
                }
            }
            Ok(None) => (),
            Err(_) => break,
        }
    }
    // Even on failure, no DMA VMO is released: this driver parks resident after containment.
    // Sora transfers one lease and never re-grants it in this boot. — TIMBERDOODLE
    match soft_reset(hw.device) {
        Ok(_) => log(log_level::NOTICE, format_args!("drv-gpu: client session closed; GPU reset confirmed; backing retained until reboot\n")),
        Err(error) => log(log_level::ERR, format_args!("drv-gpu: client reset failed: {:?}; all DMA backing retained, no regrant\n", error)),
    }
    if let Some(handle) = hw.reply_grant.take() {
        let _ = kumo_rt::handle_close(handle);
    }
    let _ = kumo_rt::handle_close(channel);
    park()
}
