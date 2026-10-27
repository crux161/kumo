#![no_std]
#![no_main]
//j636
//j637
//j638
//j639
//j641

//! One cooperative session granted by Sora in x0, with no Resource, MMIO or firmware handle.
//! Recovery class: soft-state. On any error exit and let drv-gpu contain the session.

use core::fmt::{self, Write};
use gpu_client::{
    bindings, compute_data, compute_stream, proof_stream, triangle_data, triangle_stream,
    BINARY_OFFSET, BUFFER_BYTES, DATA_OFFSET, PAINT_OFFSET, PIXEL_POISON, SCRATCH_OFFSET,
    TRIANGLE_BINARY_OFFSET, TRIANGLE_DATA_OFFSET, VALUES,
};
use kumo_abi::{Handle, VmarFlags};
use kumo_gpu::{
    client::{Client, Error as ClientError, Transport, TransportError},
    Status as GpuStatus, Submission, SyncState, REPLY_LEN, REQUEST_LEN, VA_START,
};

use kumo_mali::{
    check_paint, check_triangle, paint_reference_checksum, triangle_reference_checksum, CsFeatures,
    KUMO_PAINT_V10, KUMO_TRIANGLE_V10, PAINT_BYTES, PAINT_HEIGHT, PAINT_WIDTH,
};

kumo_rt::entry!(main);

struct Text {
    bytes: [u8; 256],
    len: usize,
}
impl Write for Text {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self
            .len
            .checked_add(text.len())
            .filter(|end| *end <= self.bytes.len())
            .ok_or(fmt::Error)?;
        self.bytes[self.len..end].copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}
fn log(args: fmt::Arguments<'_>) {
    let mut text = Text {
        bytes: [0; 256],
        len: 0,
    };
    let _ = text.write_fmt(args);
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &text.bytes[..text.len]);
}
fn handle(raw: u64) -> Option<Handle> {
    (raw != 0 && (raw as i64) > 0).then_some(Handle(raw as u32))
}
fn barrier() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}
struct Channel {
    channel: Handle,
}
impl Transport for Channel {
    type Handle = Handle;
    fn send(&mut self, bytes: &[u8; REQUEST_LEN]) -> Result<(), TransportError> {
        let status = kumo_rt::channel_write(self.channel, bytes.as_ptr(), bytes.len());
        if status == 0 {
            Ok(())
        } else if status == kumo_abi::Errno::PeerClosed.status() {
            Err(TransportError::Closed)
        } else {
            Err(TransportError::Failed)
        }
    }
    fn receive(
        &mut self,
        bytes: &mut [u8; REPLY_LEN + 1],
    ) -> Result<(usize, Option<Handle>), TransportError> {
        let (len, raw) =
            kumo_rt::channel_read_with_handle(self.channel, bytes.as_mut_ptr(), bytes.len());
        let grant = handle(raw);
        if len == 0 || (len as isize) < 0 || (raw != 0 && grant.is_none()) {
            if let Some(grant) = grant {
                let _ = kumo_rt::handle_close(grant);
            }
            return Err(if len == 0 {
                TransportError::Closed
            } else {
                TransportError::Failed
            });
        }
        Ok((len, grant))
    }
    fn close_handle(&mut self, handle: Handle) {
        let _ = kumo_rt::handle_close(handle);
    }
}

/// All writes target the retained Normal-NC mapping returned for BUFFER_BYTES in run.
fn upload(cpu: u64, offset: u64, bytes: &[u8]) -> Result<(), ()> {
    if offset
        .checked_add(bytes.len() as u64)
        .is_none_or(|end| end > BUFFER_BYTES)
    {
        return Err(());
    }
    for (i, byte) in bytes.iter().enumerate() {
        // SAFETY: caller's mapped buffer, with the byte extent checked above.
        unsafe {
            ((cpu + offset + i as u64) as *mut u8).write_volatile(*byte);
        }
    }
    Ok(())
}

fn submit_wait(
    client: &mut Client<Channel>,
    cpu: u64,
    group: u32,
    point: u64,
    stream: &[u64],
) -> Result<(), Failure> {
    if stream.len() * 8 > kumo_gpu::PAGE_BYTES as usize {
        return Err(Failure::Failed);
    }
    for (i, word) in stream.iter().enumerate() {
        // SAFETY: the validated command page, reused only after the last completed point.
        unsafe {
            ((cpu + i as u64 * 8) as *mut u64).write_volatile(*word);
        }
    }
    barrier();
    client.submit(Submission {
        group,
        stream_va: VA_START,
        stream_bytes: (stream.len() * 8) as u32,
        signal_point: point,
    })?;
    for _ in 0..1000 {
        match client.sync_read(group, point)? {
            SyncState::Complete { gpu_status: 0 } => {
                barrier();
                return Ok(());
            }
            SyncState::Pending => {
                let _ = kumo_rt::sleep_ns(1_000_000);
            }
            _ => return Err(Failure::Failed),
        }
    }
    Err(Failure::Failed)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Failure {
    Failed,
    Cancelled,
}
impl From<()> for Failure {
    fn from(_: ()) -> Self {
        Self::Failed
    }
}
impl From<ClientError> for Failure {
    fn from(error: ClientError) -> Self {
        log(format_args!(
            "gpu-client: native GPU API failed: {:?}\n",
            error
        ));
        Self::Failed
    }
}

fn present_image(
    channel: Handle,
    vmo: Handle,
    image: kumo_graphics::native::Image,
    lease: &mut kumo_graphics::native::Lease,
) -> Result<(), Failure> {
    if !lease.begin(image, kumo_rt::handle_koid(vmo)) {
        return Err(Failure::Failed);
    }
    let grant = handle(kumo_rt::handle_duplicate(
        vmo,
        kumo_abi::Rights::READ | kumo_abi::Rights::TRANSFER,
    ))
    .ok_or(())?;
    let bytes = image.encode();
    if kumo_rt::channel_write_with_handle(channel, bytes.as_ptr(), bytes.len(), grant) != 0 {
        let _ = kumo_rt::handle_close(grant);
        return Err(Failure::Failed);
    }
    log(format_args!("gpu-client: J0c sequence{} point{} leased READ-only; GPU/CPU writes blocked until matching retirement\n",image.sequence,image.gpu_point));
    let port = handle(kumo_rt::port_create()).ok_or(())?;
    let timer = handle(kumo_rt::timer_create(120_000_000_000)).ok_or(())?;
    if kumo_rt::port_bind(port, channel) != 0
        || kumo_rt::port_bind(port, timer) != 0
        || kumo_rt::port_wait(port) != kumo_rt::handle_koid(channel)
    {
        return Err(Failure::Failed);
    }
    let mut bytes = [0; kumo_graphics::native::REPLY_LEN + 1];
    let (n, extra) = kumo_rt::channel_read_with_handle(channel, bytes.as_mut_ptr(), bytes.len());
    let _ = kumo_rt::port_unbind(port, channel);
    let _ = kumo_rt::port_unbind(port, timer);
    let _ = kumo_rt::handle_close(timer);
    let _ = kumo_rt::handle_close(port);
    if extra != 0 {
        let _ = kumo_rt::handle_close(Handle(extra as u32));
        return Err(Failure::Failed);
    }
    let reply = bytes
        .get(..n)
        .and_then(kumo_graphics::native::Reply::decode)
        .ok_or(())?;
    if reply.is_cancelled() {
        return Err(Failure::Cancelled);
    }
    if !lease.retire(reply) {
        return Err(Failure::Failed);
    }
    log(format_args!("gpu-client: J0c sequence{} point{} retired after consumed GOP replacement/unmap; next GPU reuse permitted\n",image.sequence,image.gpu_point));
    Ok(())
}

fn run(channel: Handle, presentation: Handle) -> Result<(), Failure> {
    let mut client = Client::new(Channel { channel });
    let info = client.info()?;
    if info.gpu_id & 0xffff_0000 != 0xa867_0000 {
        return Err(Failure::Failed);
    }
    let features = CsFeatures::decode(info.cs_features);
    let compute = compute_stream(features).map_err(|_| ())?;
    let triangle = triangle_stream(features).map_err(|_| ())?;
    log(format_args!(
        "gpu-client: GPU_ID=0x{:08x}, queue registers={} scoreboards={}\n",
        info.gpu_id, features.work_regs, features.scoreboards
    ));
    let vm = client.create_vm()?;
    // A normal server refusal must consume its request ID without killing the channel.
    // The next real query proves the shared API can continue after that refusal. — KESTREL
    if client.create_vm() != Err(ClientError::Server(GpuStatus::BadState)) || client.info()? != info
    {
        return Err(Failure::Failed);
    }
    log(format_args!("gpu-client: G7a shared API: duplicate VM refused, next query live; unique request IDs, no replay\n"));
    let buffer = client.create_buffer(BUFFER_BYTES)?;
    let (vmo, bytes) = (buffer.handle, buffer.bytes);
    let (status, cpu) = kumo_rt::vmar_map_anywhere(
        vmo,
        bytes,
        (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
    );
    if status != 0 || cpu == 0 {
        return Err(Failure::Failed);
    }
    upload(cpu, BINARY_OFFSET, KUMO_PAINT_V10.binary)?;
    upload(cpu, DATA_OFFSET, &compute_data().map_err(|_| ())?)?;
    upload(cpu, TRIANGLE_BINARY_OFFSET, KUMO_TRIANGLE_V10.binary)?;
    upload(cpu, TRIANGLE_DATA_OFFSET, &triangle_data().map_err(|_| ())?)?;
    for offset in (0..PAINT_BYTES).step_by(4) {
        // SAFETY: each word lies inside the image extent of this retained mapping.
        unsafe {
            ((cpu + PAINT_OFFSET + offset) as *mut u32).write_volatile(PIXEL_POISON);
        }
    }
    barrier();
    for binding in bindings(vm, buffer.id) {
        client.bind(binding)?;
    }
    let group = client.create_group(vm)?;
    for (index, value) in VALUES.into_iter().enumerate() {
        let point = index as u64 + 1;
        // SAFETY: the scratch word is in its own mapped page, outside the image and code.
        unsafe {
            ((cpu + SCRATCH_OFFSET) as *mut u32).write_volatile(0xfeed_face);
        }
        submit_wait(
            &mut client,
            cpu,
            group,
            point,
            &proof_stream(VA_START + SCRATCH_OFFSET, value),
        )?;
        // SAFETY: read only after the matching completion reply and barrier.
        let got = unsafe { ((cpu + SCRATCH_OFFSET) as *const u32).read_volatile() };
        if got != value {
            log(format_args!(
                "gpu-client: point {} wrong value 0x{:08x}, expected 0x{:08x}\n",
                point, got, value
            ));
            return Err(Failure::Failed);
        }
        log(format_args!(
            "gpu-client: point {} complete, GPU wrote 0x{:08x} into shared VMO\n",
            point, got
        ));
    }
    log(format_args!(
        "gpu-client: two submissions complete; shared values verified\n"
    ));
    log(format_args!(
        "gpu-client: dispatching kumo_paint: binary=0x{:x} data=0x{:x} output=0x{:x}\n",
        VA_START + BINARY_OFFSET,
        VA_START + DATA_OFFSET,
        VA_START + PAINT_OFFSET
    ));
    submit_wait(&mut client, cpu, group, 3, &compute)?;
    let check = check_paint(|x, y| {
        // SAFETY: check_paint visits exactly the 256x128 words of this mapped image.
        unsafe {
            ((cpu + PAINT_OFFSET + u64::from(y * PAINT_WIDTH + x) * 4) as *const u32)
                .read_volatile()
        }
    });
    log(format_args!(
        "gpu-client: compute painted {}x{}: mismatches={} checksum=0x{:08x} (reference 0x{:08x})\n",
        PAINT_WIDTH,
        PAINT_HEIGHT,
        check.mismatches,
        check.checksum,
        paint_reference_checksum()
    ));
    if let Some((x, y, got, want)) = check.first {
        log(format_args!(
            "gpu-client: first wrong pixel ({}, {}): got 0x{:08x}, want 0x{:08x}\n",
            x, y, got, want
        ));
        return Err(Failure::Failed);
    }
    log(format_args!(
        "gpu-client: client compute frame verified; 32768 pixels match\n"
    ));
    let mut lease = kumo_graphics::native::Lease::default();
    let count = if presentation.0 == 0 {
        1
    } else {
        kumo_graphics::native::PRESENTATIONS
    };
    for sequence in 1..=count {
        // This check precedes EVERY poison/write and GPU submission. Only a verified
        // previous display retirement reply can reopen the image for production. — KESTREL
        if !lease.can_render() {
            return Err(Failure::Failed);
        }
        let point = u64::from(sequence) + 3;
        let is_triangle = sequence % 2 == 1;
        for offset in (0..PAINT_BYTES).step_by(4) {
            unsafe {
                ((cpu + PAINT_OFFSET + offset) as *mut u32).write_volatile(PIXEL_POISON);
            }
        }
        submit_wait(
            &mut client,
            cpu,
            group,
            point,
            if is_triangle { &triangle } else { &compute },
        )?;
        let read = |x, y| unsafe {
            ((cpu + PAINT_OFFSET + u64::from(y * PAINT_WIDTH + x) * 4) as *const u32)
                .read_volatile()
        };
        let check = if is_triangle {
            check_triangle(read)
        } else {
            check_paint(read)
        };
        let reference = if is_triangle {
            triangle_reference_checksum()
        } else {
            paint_reference_checksum()
        };
        log(format_args!("gpu-client: J0c sequence{} point{} {}: VMO{} mismatches={} checksum=0x{:08x} (reference 0x{:08x})\n",sequence,point,if is_triangle {"triangle"}else{"bars/ramp"},kumo_rt::handle_koid(vmo),check.mismatches,check.checksum,reference));
        if let Some((x, y, got, want)) = check.first {
            log(format_args!(
                "gpu-client: J0c mismatch ({},{}): got0x{:08x} want0x{:08x}\n",
                x, y, got, want
            ));
            return Err(Failure::Failed);
        }
        if presentation.0 != 0 {
            present_image(
                presentation,
                vmo,
                kumo_graphics::native::Image {
                    sequence,
                    surface: kumo_graphics::DEMO,
                    offset: PAINT_OFFSET,
                    vmo_bytes: BUFFER_BYTES,
                    gpu_point: point,
                    checksum: check.checksum,
                },
                &mut lease,
            )?;
        }
    }
    if presentation.0 != 0 {
        if !lease.complete() {
            return Err(Failure::Failed);
        }
        log(format_args!("gpu-client: J0c complete: 3 native frames, same VMO/image pages, 2 GPU reuses only after display retirement\n"));
    }
    let _ = kumo_rt::handle_close(vmo);
    Ok(())
}

#[no_mangle]
extern "C" fn main(channel: u64, presentation: u64) -> ! {
    let Some(channel) = handle(channel) else {
        kumo_rt::process_exit(1)
    };
    log(format_args!(
        "gpu-client: separate process starting GPU session\n"
    ));
    let presentation = Handle(presentation as u32);
    let result = run(channel, presentation);
    if presentation.0 != 0 {
        let _ = kumo_rt::handle_close(presentation);
    }
    let failed = match result {
        Ok(()) => false,
        Err(Failure::Cancelled) => {
            log(format_args!("gpu-client: presenter cancelled diagnostic; stopping without image reuse or fabricated completion\n"));
            false
        }
        Err(Failure::Failed) => {
            log(format_args!("gpu-client: session failed\n"));
            true
        }
    };
    let _ = kumo_rt::handle_close(channel);
    kumo_rt::process_exit(u64::from(failed))
}
