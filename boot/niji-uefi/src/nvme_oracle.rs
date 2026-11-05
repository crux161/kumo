//j800
//! Default-off pre-EBS Identify reference through firmware's NVMe queues.
//! No drv-nvme dependency, media command or direct controller write. — TIMBERDOODLE
use crate::{efi_memory_type, guid, print_line, EfiBootServices, EfiSimpleTextOutputProtocol};
use core::{ffi::c_void, ptr};
use niji_uefi::nvme::{self, Controller, Namespace, Protocol, PAGE};

fn halt(con: *mut EfiSimpleTextOutputProtocol, error: nvme::Error) -> ! {
    print_line(
        con,
        format_args!("NV4 UEFI failed: {error:?}; buffers retained, reset required\r\n"),
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
}
