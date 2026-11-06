//j800
//j801
//! Default-off pre-EBS Identify reference through firmware's NVMe queues, then
//! (NV5) LBA 0, LBA 1 and the last LBA through firmware's whole-namespace Block I/O.
//! No drv-nvme dependency, media write or direct controller write. — TIMBERDOODLE
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

pub(super) unsafe fn run(bs: *mut EfiBootServices, con: *mut EfiSimpleTextOutputProtocol) {
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
        return;
    }
    let protocol = raw.cast::<Protocol>();
    let mode = unsafe { (*protocol).mode };
    if mode.is_null() || !nvme::supported(unsafe { &*mode }) {
        print_line(
            con,
            format_args!("NV4 UEFI unavailable: unsupported mode/alignment\r\n"),
        );
        return;
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
        return;
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
    unsafe { media_reference(bs, con, &ns, buffer) };
}
