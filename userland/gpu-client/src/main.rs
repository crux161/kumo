#![no_std]
#![no_main]
//j595
//j596
//j636
//j637

//! One cooperative session granted by Sora in x0, with no Resource, MMIO or firmware handle.
//! Recovery class: soft-state. On any error exit and let drv-gpu contain the session.

use core::fmt::{self, Write};
use gpu_client::{
    bindings, compute_data, compute_stream, proof_stream, triangle_data, triangle_stream,
    BINARY_OFFSET, BUFFER_BYTES, DATA_OFFSET, PAINT_OFFSET, PIXEL_POISON, SCRATCH_OFFSET,
    TRIANGLE_BINARY_OFFSET, TRIANGLE_DATA_OFFSET, VALUES,
};
use kumo_abi::{Handle, VmarFlags};
use kumo_gpu::{Command, Reply, Request, Response, Submission, SyncState, REPLY_LEN, VA_START};

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
struct Client {
    channel: Handle,
    id: u32,
}
impl Client {
    fn call(&mut self, command: Command) -> Result<(Response, Option<Handle>), ()> {
        self.id += 1;
        let bytes = Request {
            id: self.id,
            command,
        }
        .encode();
        if kumo_rt::channel_write(self.channel, bytes.as_ptr(), bytes.len()) != 0 {
            return Err(());
        }
        let mut bytes = [0u8; REPLY_LEN + 1];
        let (len, raw) =
            kumo_rt::channel_read_with_handle(self.channel, bytes.as_mut_ptr(), bytes.len());
        let grant = handle(raw);
        let decoded = bytes.get(..len).and_then(Reply::decode);
        match decoded {
            Some(Reply {
                id,
                result: Ok(response),
            }) if id == self.id && response.handle_count() == usize::from(grant.is_some()) => {
                Ok((response, grant))
            }
            _ => {
                log(format_args!(
                    "gpu-client: request {} reply {:?} ({} bytes, handle={})\n",
                    self.id,
                    decoded,
                    len,
                    grant.is_some()
                ));
                if let Some(h) = grant {
                    let _ = kumo_rt::handle_close(h);
                }
                Err(())
            }
        }
    }
    fn simple(&mut self, command: Command) -> Result<Response, ()> {
        let (response, grant) = self.call(command)?;
        if let Some(h) = grant {
            let _ = kumo_rt::handle_close(h);
            return Err(());
        }
        Ok(response)
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
    client: &mut Client,
    cpu: u64,
    group: u32,
    point: u64,
    stream: &[u64],
) -> Result<(), ()> {
    if stream.len() * 8 > kumo_gpu::PAGE_BYTES as usize {
        return Err(());
    }
    for (i, word) in stream.iter().enumerate() {
        // SAFETY: the validated command page, reused only after the last completed point.
        unsafe {
            ((cpu + i as u64 * 8) as *mut u64).write_volatile(*word);
        }
    }
    barrier();
    if client.simple(Command::Submit(Submission {
        group,
        stream_va: VA_START,
        stream_bytes: (stream.len() * 8) as u32,
        signal_point: point,
    }))? != (Response::Submitted { point })
    {
        return Err(());
    }
    for _ in 0..1000 {
        match client.simple(Command::SyncRead { group, point })? {
            Response::Sync {
                point: p,
                state: SyncState::Complete { gpu_status: 0 },
            } if p == point => {
                barrier();
                return Ok(());
            }
            Response::Sync {
                point: p,
                state: SyncState::Pending,
            } if p == point => {
                let _ = kumo_rt::sleep_ns(1_000_000);
            }
            _ => return Err(()),
        }
    }
    Err(())
}

fn run(channel: Handle, presentation: Handle) -> Result<(), ()> {
    let mut client = Client { channel, id: 0 };
    let Response::Info(info) = client.simple(Command::GetInfo)? else {
        return Err(());
    };
    if info.gpu_id & 0xffff_0000 != 0xa867_0000 {
        return Err(());
    }
    let features = CsFeatures::decode(info.cs_features);
    let compute = compute_stream(features).map_err(|_| ())?;
    let triangle = triangle_stream(features).map_err(|_| ())?;
    log(format_args!(
        "gpu-client: GPU_ID=0x{:08x}, queue registers={} scoreboards={}\n",
        info.gpu_id, features.work_regs, features.scoreboards
    ));
    let Response::VmCreated { vm } = client.simple(Command::VmCreate)? else {
        return Err(());
    };
    let (response, grant) = client.call(Command::BufferCreate {
        bytes: BUFFER_BYTES,
    })?;
    let Response::BufferCreated { buffer, bytes } = response else {
        return Err(());
    };
    if bytes != BUFFER_BYTES {
        return Err(());
    }
    let vmo = grant.ok_or(())?;
    let (status, cpu) = kumo_rt::vmar_map_anywhere(
        vmo,
        bytes,
        (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
    );
    if status != 0 || cpu == 0 {
        return Err(());
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
    for binding in bindings(vm, buffer) {
        if client.simple(Command::VmBind(binding))? != Response::VmBound {
            return Err(());
        }
    }
    let Response::GroupCreated { group } = client.simple(Command::GroupCreate { vm })? else {
        return Err(());
    };
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
            return Err(());
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
        return Err(());
    }
    log(format_args!(
        "gpu-client: client compute frame verified; 32768 pixels match\n"
    ));
    // Point3 is complete and this image has never been leased to display. Poison
    // before the distinct triangle dispatch so every output pixel must be GPU-written. — KESTREL
    for offset in (0..PAINT_BYTES).step_by(4) {
        unsafe {
            ((cpu + PAINT_OFFSET + offset) as *mut u32).write_volatile(PIXEL_POISON);
        }
    }
    submit_wait(&mut client, cpu, group, 4, &triangle)?;
    let triangle_check = check_triangle(|x, y| unsafe {
        ((cpu + PAINT_OFFSET + u64::from(y * PAINT_WIDTH + x) * 4) as *const u32).read_volatile()
    });
    log(format_args!("gpu-client: J0b triangle compute {}x{}: mismatches={} checksum=0x{:08x} (reference 0x{:08x})\n",PAINT_WIDTH,PAINT_HEIGHT,triangle_check.mismatches,triangle_check.checksum,triangle_reference_checksum()));
    if let Some((x, y, got, want)) = triangle_check.first {
        log(format_args!(
            "gpu-client: triangle mismatch ({},{}): got0x{:08x} want0x{:08x}\n",
            x, y, got, want
        ));
        return Err(());
    }
    if presentation.0 != 0 {
        let image = kumo_graphics::native::Image {
            sequence: 1,
            surface: kumo_graphics::DEMO,
            offset: PAINT_OFFSET,
            vmo_bytes: BUFFER_BYTES,
            gpu_point: 4,
            checksum: triangle_check.checksum,
        };
        let grant = handle(kumo_rt::handle_duplicate(
            vmo,
            kumo_abi::Rights::READ | kumo_abi::Rights::TRANSFER,
        ))
        .ok_or(())?;
        let bytes = image.encode();
        if kumo_rt::channel_write_with_handle(presentation, bytes.as_ptr(), bytes.len(), grant) != 0
        {
            let _ = kumo_rt::handle_close(grant);
            return Err(());
        }
        log(format_args!("gpu-client: J0b completed point4 triangle leased READ-only; frozen until display retirement\n"));
        let port = handle(kumo_rt::port_create()).ok_or(())?;
        let timer = handle(kumo_rt::timer_create(120_000_000_000)).ok_or(())?;
        if kumo_rt::port_bind(port, presentation) != 0
            || kumo_rt::port_bind(port, timer) != 0
            || kumo_rt::port_wait(port) != kumo_rt::handle_koid(presentation)
        {
            return Err(());
        }
        let mut bytes = [0; kumo_graphics::native::REPLY_LEN + 1];
        let (n, extra) =
            kumo_rt::channel_read_with_handle(presentation, bytes.as_mut_ptr(), bytes.len());
        let _ = kumo_rt::handle_close(timer);
        let _ = kumo_rt::handle_close(port);
        if extra != 0 {
            let _ = kumo_rt::handle_close(Handle(extra as u32));
            return Err(());
        }
        if !bytes
            .get(..n)
            .and_then(kumo_graphics::native::Reply::decode)
            .is_some_and(|r| r.releases(image))
        {
            return Err(());
        }
        log(format_args!("gpu-client: J0b native triangle retired after consumed console replacement and display unmap; reuse permitted\n"));
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
    if result.is_err() {
        log(format_args!("gpu-client: session failed\n"));
    }
    let _ = kumo_rt::handle_close(channel);
    kumo_rt::process_exit(u64::from(result.is_err()))
}
