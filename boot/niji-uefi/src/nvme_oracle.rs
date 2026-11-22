//j800
//j801
//j804
//j805
//j933
//! Default-off pre-EBS Identify reference through firmware's NVMe queues, then
//! (NV5) LBA 0, LBA 1 and the last LBA through firmware's whole-namespace Block I/O.
//! No drv-nvme dependency or direct controller write. Media preparation is a
//! separate explicit, default-off feature for one disposable SSD. — TIMBERDOODLE
use crate::{
    efi_memory_type, guid, print_line, EfiBootServices, EfiHandle, EfiSimpleTextOutputProtocol,
};
use core::{ffi::c_void, ptr};
use kumo_sha256::{digest, Hex};
use niji_uefi::nvme::{self, BlockIo, Controller, Namespace, Protocol, PAGE};

const BY_PROTOCOL: u32 = 2;

fn halt(con: *mut EfiSimpleTextOutputProtocol, error: nvme::Error) -> ! {
    print_line(
        con,
        format_args!("NVMe UEFI oracle failed: {error:?}; buffers retained, reset required\r\n"),
    );
    // Unknown completion never reaches EBS/OS memory reuse. — TIMBERDOODLE
    loop {
        core::hint::spin_loop();
    }
}
unsafe fn snapshot(buffer: *mut u8, output: &mut [u8; PAGE]) {
    for (i, b) in output.iter_mut().enumerate() {
        *b = unsafe { ptr::read_volatile(buffer.add(i)) };
    }
}

/// The one Block I/O whose device path ends at NVMe namespace `nsid`; partition
/// children and any second match make the reference unavailable. — TIMBERDOODLE
unsafe fn whole_namespace_block_io(
    bs: *mut EfiBootServices,
    nsid: u32,
) -> Result<*mut BlockIo, &'static str> {
    let block_io = guid(
        0x964e5b21,
        0x6459,
        0x11d2,
        [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
    );
    let device_path = guid(
        0x09576e91,
        0x6d3f,
        0x11d2,
        [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
    );
    let mut count = 0usize;
    let mut handles: *mut EfiHandle = ptr::null_mut();
    let status = unsafe {
        ((*bs).locate_handle_buffer)(
            BY_PROTOCOL,
            &block_io,
            ptr::null_mut(),
            &mut count,
            &mut handles,
        )
    };
    if status != 0 || handles.is_null() {
        return Err("no Block I/O handles");
    }
    let mut found: *mut BlockIo = ptr::null_mut();
    let mut matches = 0;
    for i in 0..count {
        let handle = unsafe { *handles.add(i) };
        let mut path: *mut c_void = ptr::null_mut();
        if unsafe { ((*bs).handle_protocol)(handle, &device_path, &mut path) } != 0 {
            continue;
        }
        let Some(len) = (unsafe { nvme::path_len(path.cast()) }) else {
            continue;
        };
        let bytes = unsafe { core::slice::from_raw_parts(path.cast::<u8>(), len) };
        if nvme::whole_namespace(bytes) != Some(nsid) {
            continue;
        }
        let mut bio: *mut c_void = ptr::null_mut();
        if unsafe { ((*bs).handle_protocol)(handle, &block_io, &mut bio) } != 0 || bio.is_null() {
            continue;
        }
        matches += 1;
        found = bio.cast();
    }
    let _ = unsafe { ((*bs).free_pool)(handles.cast()) };
    match matches {
        1 => Ok(found),
        0 => Err("no whole-namespace Block I/O"),
        _ => Err("more than one whole-namespace Block I/O"),
    }
}

/// NV5 media reference. Geometry is cross-checked against Identify before the
/// first read; a failed read halts with the page retained. — TIMBERDOODLE
unsafe fn media_reference(
    bs: *mut EfiBootServices,
    con: *mut EfiSimpleTextOutputProtocol,
    ns: &Namespace,
    buffer: *mut u8,
) {
    let bio = match unsafe { whole_namespace_block_io(bs, ns.id) } {
        Ok(bio) => bio,
        Err(why) => {
            print_line(con, format_args!("NV5 UEFI unavailable: {why}\r\n"));
            return;
        }
    };
    let media = unsafe { &*(*bio).media };
    print_line(
        con,
        format_args!(
            "NV5 UEFI BLOCKIO rev={:#x} media={} block={} last={} align={} ro={}\r\n",
            unsafe { (*bio).revision },
            media.media_id,
            media.block_size,
            media.last_block,
            media.io_align,
            media.read_only
        ),
    );
    if !nvme::media_matches(media, ns) {
        print_line(
            con,
            format_args!("NV5 UEFI unavailable: Block I/O geometry differs from Identify\r\n"),
        );
        return;
    }
    let mut data = [0; PAGE];
    for lba in [0, 1, media.last_block] {
        unsafe { ptr::write_bytes(buffer, 0, PAGE) };
        if let Err(e) = unsafe { nvme::read_block(bio, lba, buffer) } {
            halt(con, e);
        }
        unsafe { snapshot(buffer, &mut data) };
        let block = &data[..media.block_size as usize];
        print_line(
            con,
            format_args!(
                "NV5 UEFI READ lba={lba} bytes={} nonzero={}\r\n",
                block.len(),
                block.iter().filter(|&&b| b != 0).count()
            ),
        );
        print_line(
            con,
            format_args!("NV5 UEFI SHA lba={lba} {}\r\n", Hex(digest(block))),
        );
    }
    unsafe { ptr::write_bytes(buffer, 0, PAGE) };
    print_line(
        con,
        format_args!("NV5 UEFI media reference complete; LoaderData page retained\r\n"),
    );
}

pub(super) unsafe fn run(
    bs: *mut EfiBootServices,
    con: *mut EfiSimpleTextOutputProtocol,
    _st: *mut crate::EfiSystemTable,
) -> bool {
    let id = guid(
        0x52c78312,
        0x8edc,
        0x4233,
        [0x98, 0xf2, 0x1a, 0x1a, 0xa5, 0xe3, 0x88, 0xa5],
    );
    let mut raw: *mut c_void = ptr::null_mut();
    let status = unsafe { ((*bs).locate_protocol)(&id, ptr::null_mut(), &mut raw) };
    if status != 0 || raw.is_null() {
        print_line(
            con,
            format_args!("NV4 UEFI unavailable: PassThru {status:#x}\r\n"),
        );
        return false;
    }
    let protocol = raw.cast::<Protocol>();
    let mode = unsafe { (*protocol).mode };
    if mode.is_null() || !nvme::supported(unsafe { &*mode }) {
        print_line(
            con,
            format_args!("NV4 UEFI unavailable: unsupported mode/alignment\r\n"),
        );
        return false;
    }
    print_line(
        con,
        format_args!(
            "NV4 UEFI transport attrs={:#x} align={} version={:#x}\r\n",
            unsafe { (*mode).attributes },
            unsafe { (*mode).io_align },
            unsafe { (*mode).version }
        ),
    );
    let mut physical = 0u64;
    let status =
        unsafe { ((*bs).allocate_pages)(0, efi_memory_type::LOADER_DATA, 1, &mut physical) };
    if status != 0 || physical == 0 || physical & 4095 != 0 {
        print_line(
            con,
            format_args!("NV4 UEFI unavailable: page allocation {status:#x}\r\n"),
        );
        return false;
    }
    // Keep this LoaderData page reserved across EBS, even after success. On any
    // unproven command halt before EBS; never recycle a timed-out DMA buffer. — TIMBERDOODLE
    let buffer = physical as *mut u8;
    unsafe { ptr::write_bytes(buffer, 0, PAGE) };
    if let Err(e) = unsafe { nvme::identify(protocol, 0, 1, buffer) } {
        halt(con, e);
    }
    let mut data = [0; PAGE];
    unsafe { snapshot(buffer, &mut data) };
    let controller = match Controller::parse(&data) {
        Ok(c) => c,
        Err(e) => halt(con, e),
    };
    print_line(
        con,
        format_args!(
            "NV4 UEFI CONTROLLER vid={:#x} ssvid={:#x}\r\n",
            controller.vid, controller.ssvid
        ),
    );
    print_line(
        con,
        format_args!("NV4 UEFI MODEL \"{}\"\r\n", controller.model()),
    );
    print_line(
        con,
        format_args!("NV4 UEFI SERIAL \"{}\"\r\n", controller.serial()),
    );
    print_line(
        con,
        format_args!("NV4 UEFI FIRMWARE \"{}\"\r\n", controller.firmware()),
    );
    print_line(
        con,
        format_args!(
            "NV4 UEFI CAPS version={:#x} mdts={} nn={} vwc={}\r\n",
            controller.version, controller.mdts, controller.namespaces, controller.vwc
        ),
    );
    let mut nsid = u32::MAX;
    let status = unsafe { ((*protocol).next_namespace)(protocol, &mut nsid) };
    if status != 0 || matches!(nsid, 0 | u32::MAX) {
        halt(con, nvme::Error::Firmware(status));
    }
    unsafe { ptr::write_bytes(buffer, 0, PAGE) };
    if let Err(e) = unsafe { nvme::identify(protocol, nsid, 0, buffer) } {
        halt(con, e);
    }
    unsafe { snapshot(buffer, &mut data) };
    let ns = match Namespace::parse(nsid, &data) {
        Ok(n) => n,
        Err(e) => halt(con, e),
    };
    print_line(
        con,
        format_args!(
            "NV4 UEFI NS nsid={} nsze={} ncap={}\r\n",
            ns.id, ns.size, ns.capacity
        ),
    );
    print_line(
        con,
        format_args!(
            "NV4 UEFI LBA nsid={} nuse={} lbaf={} lba={}\r\n",
            ns.id, ns.used, ns.format, ns.lba
        ),
    );
    print_line(
        con,
        format_args!(
            "NV4 UEFI BYTES nsid={} bytes={} metadata={} protection={}\r\n",
            ns.id, ns.bytes, ns.metadata, ns.protection
        ),
    );
    print_line(
        con,
        format_args!("NV4 UEFI reference complete; LoaderData page retained\r\n"),
    );
    #[cfg(all(feature = "nvme-prepare", not(feature = "nvme-prepare-root")))]
    unsafe {
        prepare_media(bs, con, _st, &controller, &ns, buffer)
    };
    // j933 (SH8-g): the root image builds on J804's prepared disk instead of preparing it.
    #[cfg(feature = "nvme-prepare-root")]
    unsafe {
        prepare_root(bs, con, _st, &controller, &ns, buffer)
    };
    unsafe { media_reference(bs, con, &ns, buffer) };
    #[cfg(feature = "nvme-write-reference")]
    return unsafe { write_reference(bs, con, _st, &controller, &ns, buffer) };
    #[cfg(not(feature = "nvme-write-reference"))]
    true
}

/// The whole namespace through firmware Block I/O, re-checking the media on every access
/// (J804; shared with the SH8-g root step since j933).
#[cfg(feature = "nvme-prepare")]
struct FirmwareDisk {
    bio: *mut BlockIo,
    buffer: *mut u8,
    media_id: u32,
}
#[cfg(feature = "nvme-prepare")]
impl FirmwareDisk {
    fn check(&self) -> Result<(), niji_uefi::nvme_prepare::Error> {
        let m = unsafe { &*(*self.bio).media };
        if m.media_id != self.media_id
            || m.present != 1
            || m.read_only != 0
            || m.logical_partition != 0
            || m.block_size != 512
            || m.last_block != niji_uefi::nvme_prepare::BLOCKS - 1
        {
            return Err(niji_uefi::nvme_prepare::Error::Io);
        }
        Ok(())
    }
}
#[cfg(feature = "nvme-prepare")]
impl niji_uefi::nvme_prepare::Disk for FirmwareDisk {
    fn read(&mut self, lba: u64, b: &mut [u8; 512]) -> Result<(), niji_uefi::nvme_prepare::Error> {
        self.check()?;
        unsafe { nvme::read_block(self.bio, lba, self.buffer) }
            .map_err(|_| niji_uefi::nvme_prepare::Error::Io)?;
        for (i, x) in b.iter_mut().enumerate() {
            *x = unsafe { ptr::read_volatile(self.buffer.add(i)) };
        }
        Ok(())
    }
    fn write(&mut self, lba: u64, b: &[u8; 512]) -> Result<(), niji_uefi::nvme_prepare::Error> {
        self.check()?;
        if lba >= niji_uefi::nvme_prepare::BLOCKS {
            return Err(niji_uefi::nvme_prepare::Error::Io);
        }
        for (i, x) in b.iter().enumerate() {
            unsafe { ptr::write_volatile(self.buffer.add(i), *x) };
        }
        let call = unsafe { (*self.bio).write_blocks }.ok_or(niji_uefi::nvme_prepare::Error::Io)?;
        let status = unsafe { call(self.bio, self.media_id, lba, 512, self.buffer.cast()) };
        if status == 0 {
            Ok(())
        } else {
            Err(niji_uefi::nvme_prepare::Error::Io)
        }
    }
    fn flush(&mut self) -> Result<(), niji_uefi::nvme_prepare::Error> {
        self.check()?;
        let call = unsafe { (*self.bio).flush_blocks }.ok_or(niji_uefi::nvme_prepare::Error::Io)?;
        if unsafe { call(self.bio) } == 0 {
            Ok(())
        } else {
            Err(niji_uefi::nvme_prepare::Error::Io)
        }
    }
}

/// j933 (SH8-g): the extent file `main` read from the ESP (or TFTP) before the oracle ran.
#[cfg(feature = "nvme-prepare-root")]
static ROOT_EXTENTS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
#[cfg(feature = "nvme-prepare-root")]
static ROOT_EXTENTS_LEN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Hand the oracle the extent file (`None`: not found; the step then refuses).
#[cfg(feature = "nvme-prepare-root")]
pub fn set_root_extents(file: Option<(*mut c_void, usize)>) {
    use core::sync::atomic::Ordering::Relaxed;
    let (ptr, len) = file.map_or((0, 0), |(p, l)| (p as usize, l));
    ROOT_EXTENTS.store(ptr, Relaxed);
    ROOT_EXTENTS_LEN.store(len, Relaxed);
}

/// j933 (SH8-g, G-a): add the RedoxFS root partition. The whole-namespace Block I/O and media
/// contract are J804's; only `nvme_prepare_root::inspect` can authorize a write. Before any
/// write the GPT sectors are dumped to the serial host, and the step waits for the host's
/// receipt token, which TEAL types only after CRUX's go for this boot (D8). — TEAL
#[cfg(feature = "nvme-prepare-root")]
unsafe fn prepare_root(
    bs: *mut EfiBootServices,
    con: *mut EfiSimpleTextOutputProtocol,
    st: *mut crate::EfiSystemTable,
    controller: &Controller,
    ns: &Namespace,
    buffer: *mut u8,
) {
    use core::sync::atomic::Ordering::Relaxed;
    use niji_uefi::nvme_prepare::{self as prep, Disk};
    use niji_uefi::nvme_prepare_root::{self as root, Extents, Inspection};
    let (ptr, len) = (ROOT_EXTENTS.load(Relaxed), ROOT_EXTENTS_LEN.load(Relaxed));
    if ptr == 0 || len == 0 {
        print_line(
            con,
            format_args!("SH8G PREP refused: no extent file; zero writes\r\n"),
        );
        return;
    }
    let file = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
    let ext = match Extents::parse(file) {
        Ok(ext) => ext,
        Err(e) => {
            print_line(
                con,
                format_args!("SH8G PREP refused: extent file {e:?}; zero writes\r\n"),
            );
            return;
        }
    };
    print_line(
        con,
        format_args!(
            "SH8G EXT uuid={:032x} records={} bytes={} sha={}\r\n",
            u128::from_be_bytes(ext.uuid),
            ext.count(),
            len,
            Hex(digest(file))
        ),
    );
    let bio = match unsafe { whole_namespace_block_io(bs, ns.id) } {
        Ok(bio) if !unsafe { (*bio).media }.is_null() => bio,
        _ => {
            print_line(
                con,
                format_args!("SH8G PREP refused: no unique whole namespace\r\n"),
            );
            return;
        }
    };
    let m = unsafe { &*(*bio).media };
    if !nvme::media_matches(m, ns)
        || m.read_only != 0
        || unsafe { (*bio).write_blocks }.is_none()
        || unsafe { (*bio).flush_blocks }.is_none()
    {
        print_line(con, format_args!("SH8G PREP refused: media contract\r\n"));
        return;
    }
    let mut disk = FirmwareDisk {
        bio,
        buffer,
        media_id: m.media_id,
    };
    let original = match root::inspect(&mut disk, controller, ns) {
        Ok(Inspection::Prepared) => {
            print_line(
                con,
                format_args!("SH8G PREP already prepared; zero writes\r\n"),
            );
            return;
        }
        Ok(Inspection::Original(token)) => (token, false),
        Ok(Inspection::Reimage(token)) => {
            print_line(
                con,
                format_args!("SH8G PREP re-image build: partition 2 will be rewritten\r\n"),
            );
            (token, true)
        }
        Err(prep::Error::Io) => halt(con, nvme::Error::Data),
        Err(e) => {
            print_line(
                con,
                format_args!("SH8G PREP refused: {e:?}; zero writes\r\n"),
            );
            return;
        }
    };
    let (original, reimage) = original;
    // The GPT as it is now, to the serial host, before anything changes (the rollback record:
    // restoring these 67 sectors and zeroing partition 2's header ring undoes this step).
    let mut sha = kumo_sha256::Sha256::new();
    let mut b = [0; 512];
    for (first, last) in [(0, 33), (prep::BLOCKS - 33, prep::BLOCKS - 1)] {
        for lba in first..=last {
            if disk.read(lba, &mut b).is_err() {
                halt(con, nvme::Error::Data);
            }
            sha.update(&lba.to_le_bytes());
            sha.update(&b);
            for (offset, chunk) in b.chunks_exact(32).enumerate() {
                print_line(con, format_args!("SH8G BAK {lba} {} ", offset * 32));
                for byte in chunk {
                    print_line(con, format_args!("{byte:02x}"));
                }
                print_line(con, format_args!("\r\n"));
            }
        }
    }
    print_line(con, format_args!("SH8G BAKSHA {}\r\n", Hex(sha.finish())));
    print_line(con, format_args!("SH8G PREP waiting for host receipt\r\n"));
    let input = unsafe { (*st).con_in };
    if input.is_null() {
        return;
    }
    // A re-image takes its own token: the host must mean this one.
    let token: &[u8] = if reimage {
        b"K933-REIMAGE\r"
    } else {
        b"K933-ROOT\r"
    };
    let mut matched = 0;
    let mut received = false;
    for _ in 0..12_000 {
        let mut key = crate::EfiInputKey {
            scan_code: 0,
            unicode_char: 0,
        };
        if unsafe { ((*input).read_key_stroke)(input, &mut key) } == 0 {
            if key.unicode_char == u16::from(token[matched]) {
                matched += 1;
            } else {
                matched = 0;
            }
            if matched == token.len() {
                received = true;
                break;
            }
        }
        unsafe { ((*bs).stall)(10_000) };
    }
    if !received {
        print_line(
            con,
            format_args!("SH8G PREP receipt timeout; zero writes\r\n"),
        );
        return;
    }
    print_line(
        con,
        format_args!(
            "SH8G PREP begin: partition 2, {} extents, then GPT v2\r\n",
            ext.count()
        ),
    );
    match root::apply(&mut disk, original, &ext, |phase| {
        print_line(
            con,
            format_args!("SH8G PREP phase {phase}/4 flushed and verified\r\n"),
        )
    }) {
        Ok(count) => print_line(
            con,
            format_args!("SH8G PREP complete: {count} sectors written/flushed/readback\r\n"),
        ),
        Err(e) => {
            print_line(con, format_args!("SH8G PREP failed {e:?}\r\n"));
            halt(con, nvme::Error::Data);
        }
    }
}

#[cfg(all(feature = "nvme-prepare", not(feature = "nvme-prepare-root")))]
unsafe fn prepare_media(
    bs: *mut EfiBootServices,
    con: *mut EfiSimpleTextOutputProtocol,
    st: *mut crate::EfiSystemTable,
    controller: &Controller,
    ns: &Namespace,
    buffer: *mut u8,
) {
    use niji_uefi::nvme_prepare::{self as prep, Disk, Inspection};
    let bio = match unsafe { whole_namespace_block_io(bs, ns.id) } {
        Ok(bio) if !unsafe { (*bio).media }.is_null() => bio,
        _ => {
            print_line(
                con,
                format_args!("NV7 PREP refused: no unique whole namespace\r\n"),
            );
            return;
        }
    };
    let m = unsafe { &*(*bio).media };
    if !nvme::media_matches(m, ns)
        || m.read_only != 0
        || unsafe { (*bio).write_blocks }.is_none()
        || unsafe { (*bio).flush_blocks }.is_none()
    {
        print_line(con, format_args!("NV7 PREP refused: media contract\r\n"));
        return;
    }
    let mut disk = FirmwareDisk {
        bio,
        buffer,
        media_id: m.media_id,
    };
    let original = match prep::inspect(&mut disk, controller, ns) {
        Ok(Inspection::Prepared) => {
            print_line(
                con,
                format_args!("NV7 PREP already prepared; zero writes\r\n"),
            );
            return;
        }
        Ok(Inspection::Original(token)) => token,
        Err(prep::Error::Io) => halt(con, nvme::Error::Data),
        Err(e) => {
            print_line(
                con,
                format_args!("NV7 PREP refused: {e:?}; zero writes\r\n"),
            );
            return;
        }
    };
    // Save original GPT/MBR and two NTFS prefix sectors to the serial host BEFORE
    // writing. This is metadata audit only, not a backup of the old filesystem.
    // The input token acknowledges verified host receipt, under CRUX's existing
    // disposable-media authority; it is not another erase approval. — TIMBERDOODLE
    let mut sha = kumo_sha256::Sha256::new();
    let mut b = [0; 512];
    for (first, last) in prep::AUDIT_RANGES {
        for lba in first..=last {
            if disk.read(lba, &mut b).is_err() {
                halt(con, nvme::Error::Data);
            }
            sha.update(&lba.to_le_bytes());
            sha.update(&b);
            for (offset, chunk) in b.chunks_exact(16).enumerate() {
                print_line(con, format_args!("NV7 BAK {lba} {} ", offset * 16));
                for byte in chunk {
                    print_line(con, format_args!("{byte:02x}"));
                }
                print_line(con, format_args!("\r\n"));
            }
        }
    }
    print_line(con, format_args!("NV7 BAKSHA {}\r\n", Hex(sha.finish())));
    print_line(
        con,
        format_args!("NV7 PREP waiting for host metadata receipt\r\n"),
    );
    let input = unsafe { (*st).con_in };
    if input.is_null() {
        return;
    }
    let mut matched = 0;
    let token = b"K804-625Z\r";
    let mut received = false;
    for _ in 0..12_000 {
        let mut key = crate::EfiInputKey {
            scan_code: 0,
            unicode_char: 0,
        };
        if unsafe { ((*input).read_key_stroke)(input, &mut key) } == 0 {
            if key.unicode_char == u16::from(token[matched]) {
                matched += 1;
            } else {
                matched = 0;
            }
            if matched == token.len() {
                received = true;
                break;
            }
        }
        unsafe { ((*bs).stall)(10_000) };
    }
    if !received {
        print_line(
            con,
            format_args!("NV7 PREP receipt timeout; zero writes\r\n"),
        );
        return;
    }
    print_line(
        con,
        format_args!("NV7 PREP begin authorized disposable SSD; 256MiB FAT32\r\n"),
    );
    match prep::apply(&mut disk, original, |phase| {
        print_line(
            con,
            format_args!("NV7 PREP phase {phase}/8 flushed and verified\r\n"),
        )
    }) {
        Ok(count) => print_line(
            con,
            format_args!("NV7 PREP complete: {count} sectors written/flushed/readback\r\n"),
        ),
        Err(e) => {
            print_line(con, format_args!("NV7 PREP failed {e:?}\r\n"));
            halt(con, nvme::Error::Data);
        }
    }
}

#[cfg(feature = "nvme-write-reference")]
unsafe fn write_reference(
    bs: *mut EfiBootServices,
    con: *mut EfiSimpleTextOutputProtocol,
    st: *mut crate::EfiSystemTable,
    c: &Controller,
    n: &Namespace,
    buffer: *mut u8,
) -> bool {
    // This is an independent firmware read, not drv-nvme's LBA encoder. The
    // proof image cannot enter KUMO without verified host backup receipt. — TIMBERDOODLE
    if c.vid != 0x144d
        || c.ssvid != 0x144d
        || c.model() != "Samsung SSD 970 EVO Plus 500GB"
        || c.serial() != "S58SNM0R302625Z"
        || c.firmware() != "2B2QEXM7"
        || c.namespaces != 1
        || n.id != 1
        || n.size != 976773168
        || n.capacity != 976773168
        || n.lba != 512
        || n.metadata != 0
        || n.protection != 0
    {
        return false;
    }
    let bio = match unsafe { whole_namespace_block_io(bs, n.id) } {
        Ok(b) => b,
        Err(_) => return false,
    };
    if unsafe { (*bio).media }.is_null() || !nvme::media_matches(unsafe { &*(*bio).media }, n) {
        return false;
    }
    let mut sha = kumo_sha256::Sha256::new();
    let mut page = [0; PAGE];
    for lba in [1048583u64, 1048584, 1048585] {
        if let Err(e) = unsafe { nvme::read_block(bio, lba, buffer) } {
            halt(con, e);
        }
        unsafe { snapshot(buffer, &mut page) };
        let b = &page[..512];
        sha.update(&lba.to_le_bytes());
        sha.update(b);
        print_line(
            con,
            format_args!("NV8 UEFI SHA lba={lba} {}\r\n", Hex(digest(b))),
        );
        for (off, chunk) in b.chunks_exact(16).enumerate() {
            print_line(con, format_args!("NV8 BAK {lba} {} ", off * 16));
            for byte in chunk {
                print_line(con, format_args!("{byte:02x}"));
            }
            print_line(con, format_args!("\r\n"));
        }
    }
    print_line(con, format_args!("NV8 BAKSHA {}\r\n", Hex(sha.finish())));
    print_line(
        con,
        format_args!("NV8 waiting for host metadata receipt\r\n"),
    );
    let input = unsafe { (*st).con_in };
    if input.is_null() {
        return false;
    }
    let token = b"K805-625Z\r";
    let mut at = 0;
    for _ in 0..12_000 {
        let mut key = crate::EfiInputKey {
            scan_code: 0,
            unicode_char: 0,
        };
        if unsafe { ((*input).read_key_stroke)(input, &mut key) } == 0 {
            if key.unicode_char == u16::from(token[at]) {
                at += 1;
            } else {
                at = 0;
            }
            if at == token.len() {
                print_line(
                    con,
                    format_args!("NV8 host receipt verified; proof boot allowed\r\n"),
                );
                return true;
            }
        }
        unsafe { ((*bs).stall)(10_000) };
    }
    false
}
