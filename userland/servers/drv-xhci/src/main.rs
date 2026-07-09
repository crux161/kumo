#![no_std]
#![no_main]

//j426
//j430

use kumo_abi::{Handle, VmarFlags};
use kumo_rt::{channel_read_with_handle, debug_write, process_exit, resource_mint_mmio, vmar_map};
use kumo_xhci::{
    portsc_offset, CapabilityRegisters, PortStatus, RegisterLayout, XhciProbeConfig,
    XHCI_PROBE_CONFIG_LEN,
};

kumo_rt::entry!(main);

const MMIO_VA: u64 = 0x0000_0000_1100_0000;
const MAX_LOGGED_PORTS: u8 = 16;
const PAGE_SIZE: u64 = 4096;

#[no_mangle]
extern "C" fn main(
    _arg0: u64,
    bootstrap_channel: u64,
    _arg2: u64,
    _arg3: u64,
    _arg4: u64,
    _arg5: u64,
    _arg6: u64,
    _arg7: u64,
) -> ! {
    log(b"drv-xhci starting\n");

    let bootstrap = Handle(bootstrap_channel as u32);
    let mut encoded = [0u8; XHCI_PROBE_CONFIG_LEN];
    let (received, resource_raw) =
        channel_read_with_handle(bootstrap, encoded.as_mut_ptr(), encoded.len());
    if received != encoded.len() {
        log(b"drv-xhci: bad bootstrap\n");
        process_exit(1);
    }
    let Some(config) = XhciProbeConfig::decode(&encoded) else {
        log(b"drv-xhci: bad bootstrap\n");
        process_exit(1);
    };
    if resource_raw == 0 || resource_raw == u64::MAX {
        log(b"drv-xhci: missing resource\n");
        process_exit(1);
    }

    log(b"drv-xhci: usb0 mmio=");
    log_hex(config.mmio_base);
    log(b" len=");
    log_hex(config.mmio_length);
    log(b" irq=");
    log_hex(config.irq as u64);
    log(b" stream=");
    log_hex(config.stream_id as u64);
    log(b"\n");

    let Some(map_len) = align_up(config.mmio_length, PAGE_SIZE) else {
        log(b"drv-xhci: mmio len overflow\n");
        process_exit(1);
    };
    if map_len != config.mmio_length {
        log(b"drv-xhci: mmio len unaligned\n");
        process_exit(1);
    }

    let vmo_raw = resource_mint_mmio(Handle(resource_raw as u32), config.mmio_base, map_len);
    if vmo_raw == 0 || vmo_raw == u64::MAX {
        log(b"drv-xhci: mmio vmo mint failed\n");
        process_exit(1);
    }

    let status = vmar_map(
        Handle(0),
        Handle(vmo_raw as u32),
        0,
        MMIO_VA,
        map_len,
        (VmarFlags::READ | VmarFlags::DEVICE).0,
    );
    if status != 0 {
        log(b"drv-xhci: mmio map failed st=");
        log_hex(status);
        log(b"\n");
        process_exit(1);
    }

    let cap_word = read32(0);
    let hcsparams1 = read32(0x04);
    let hccparams1 = read32(0x10);
    let dboff = read32(0x14);
    let rtsoff = read32(0x18);
    let caps = match CapabilityRegisters::from_words(cap_word, hcsparams1, hccparams1) {
        Ok(caps) => caps,
        Err(_) => {
            log(b"drv-xhci: invalid caps cap=");
            log_hex(cap_word as u64);
            log(b" hcs=");
            log_hex(hcsparams1 as u64);
            log(b" hcc=");
            log_hex(hccparams1 as u64);
            log(b"\n");
            process_exit(1);
        }
    };

    log(b"drv-xhci: cap=");
    log_hex(caps.caplength() as u64);
    log(b" hci=");
    log_hex(caps.hciversion() as u64);
    log(b" slots=");
    log_hex(caps.max_slots() as u64);
    log(b" intrs=");
    log_hex(caps.max_interrupters() as u64);
    log(b" ports=");
    log_hex(caps.max_ports() as u64);
    log(b" ctx=");
    log_hex(caps.context_size().bytes() as u64);
    log(b" xecp=");
    log_hex(caps.extended_capabilities_offset() as u64);
    log(b"\n");

    log(b"drv-xhci: layout raw dboff=");
    log_hex(dboff as u64);
    log(b" rtsoff=");
    log_hex(rtsoff as u64);
    log(b"\n");
    match RegisterLayout::new(caps.caplength(), dboff, rtsoff, config.mmio_length) {
        Ok(layout) => log_layout(layout),
        Err(_) => log(b"drv-xhci: layout outside grant\n"),
    }

    let mut port = 0u8;
    let max_ports = caps.max_ports().min(MAX_LOGGED_PORTS);
    while port < max_ports {
        if let Some(offset) = portsc_offset(caps.caplength(), port) {
            if (offset as u64).saturating_add(4) <= config.mmio_length {
                log_port(port + 1, PortStatus::new(read32(offset)));
            }
        }
        port += 1;
    }
    if caps.max_ports() > MAX_LOGGED_PORTS {
        log(b"drv-xhci: port log truncated\n");
    }

    log(b"drv-xhci: first light done\n");
    process_exit(0);
}

fn log(msg: &[u8]) {
    debug_write(msg.as_ptr(), msg.len());
}

fn log_port(index: u8, port: PortStatus) {
    log(b"drv-xhci: port");
    log_hex(index as u64);
    log(b" raw=");
    log_hex(port.raw() as u64);
    log(b" ccs=");
    log_hex(port.connected() as u64);
    log(b" ped=");
    log_hex(port.enabled() as u64);
    log(b" pls=");
    log_hex(port.link_state() as u64);
    log(b" speed=");
    log_hex(port.speed() as u64);
    log(b" pp=");
    log_hex(port.powered() as u64);
    log(b"\n");
}

fn log_layout(layout: RegisterLayout) {
    log(b"drv-xhci: layout op=");
    log_hex(layout.operational_offset() as u64);
    log(b" run=");
    log_hex(layout.runtime_offset() as u64);
    log(b" db=");
    log_hex(layout.doorbell_offset() as u64);
    log(b" crcr=");
    log_hex(layout.command_ring_offset() as u64);
    log(b" dcbaa=");
    log_hex(layout.dcbaa_offset() as u64);
    log(b" ir0_erstsz=");
    log_hex(layout.interrupter0_erst_size_offset() as u64);
    log(b"\n");
}

fn log_hex(mut value: u64) {
    debug_write(b"0x".as_ptr(), 2);
    let mut digits = [0u8; 16];
    let mut start = digits.len();
    loop {
        start -= 1;
        let digit = (value & 0xf) as u8;
        digits[start] = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        };
        value >>= 4;
        if value == 0 {
            break;
        }
    }
    debug_write(digits[start..].as_ptr(), digits.len() - start);
}

fn read32(offset: usize) -> u32 {
    unsafe { core::ptr::read_volatile((MMIO_VA as usize + offset) as *const u32) }
}

fn align_up(value: u64, align: u64) -> Option<u64> {
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
}
