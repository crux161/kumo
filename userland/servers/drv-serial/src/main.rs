#![no_std]
#![no_main]
//j559

extern crate alloc;

use kumo_abi::{Handle, VmarFlags};
use kumo_rt::{channel_write, interrupt_create, interrupt_wait, resource_mint_mmio, vmar_map};

/// Diagnostics go to the kernel log (`dmesg`) since J559: shown on the console only when its level
/// allows. Failures are ERR, so a default boot still shows them.
fn log_info(message: &[u8]) {
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::INFO, message);
}

fn log_err(message: &[u8]) {
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::ERR, message);
}

kumo_rt::entry!(main);

const UART_BASE: u64 = 0x0900_0000; // QEMU PL011 base address
const UART_SIZE: u64 = 4096;
const UART_IRQ: u32 = 33; // SPI 1 = 33 on QEMU ARM virt

// PL011 registers
const UARTDR: u64 = 0x000;
const UARTFR: u64 = 0x018;
const UARTIMSC: u64 = 0x038;
const UARTICR: u64 = 0x044;

const UARTFR_RXFE: u32 = 1 << 4;

#[no_mangle]
extern "C" fn main(
    resource_handle: u64,
    console_channel: u64,
    _arg3: u64,
    _arg4: u64,
    _arg5: u64,
    _arg6: u64,
    _arg7: u64,
    _arg8: u64,
) -> ! {
    log_info(b"drv-serial starting\n");

    let res = Handle(resource_handle as u32);
    let console = Handle(console_channel as u32);

    // 1. Mint MMIO VMO
    let vmo_h = resource_mint_mmio(res, UART_BASE, UART_SIZE);
    if vmo_h == u64::MAX {
        log_err(b"drv-serial: vmo mint failed\n");
        kumo_rt::process_exit(1);
    }
    let vmo = Handle(vmo_h as u32);

    // 2. Map the VMO into our address space
    let map_virt = 0x0000_0000_1000_0000; // arbitrary unmapped region
    let map_status = vmar_map(
        Handle(0),
        vmo,
        0,
        map_virt,
        UART_SIZE,
        (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::DEVICE).0,
    );
    if map_status != 0 {
        log_err(b"drv-serial: map failed\n");
        kumo_rt::process_exit(1);
    }

    // 3. Create the Interrupt object — gated by the same device Resource that
    //    authorized the MMIO mint above (its grant covers exactly this IRQ line).
    let irq_h = interrupt_create(res, UART_IRQ);
    if irq_h == u64::MAX {
        log_err(b"drv-serial: irq failed\n");
        kumo_rt::process_exit(1);
    }
    let irq = Handle(irq_h as u32);

    // Unmask RX interrupts on PL011
    unsafe {
        let imsc = (map_virt + UARTIMSC) as *mut u32;
        imsc.write_volatile(imsc.read_volatile() | (1 << 4)); // RXIM
    }

    log_info(b"drv-serial: initialized\n");
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, b"drv-serial: online\n");

    let mut buf = [0u8; 1];

    // Main loop: wait for interrupt, read UART, write to console channel
    loop {
        // Wait for IRQ
        interrupt_wait(irq);

        // Acknowledge interrupt (clear RXIC)
        unsafe {
            let icr = (map_virt + UARTICR) as *mut u32;
            icr.write_volatile(1 << 4);
        }

        // Read all available bytes
        loop {
            let fr = unsafe { ((map_virt + UARTFR) as *mut u32).read_volatile() };
            if (fr & UARTFR_RXFE) != 0 {
                break; // RX FIFO empty
            }

            let dr = unsafe { ((map_virt + UARTDR) as *mut u32).read_volatile() };
            buf[0] = (dr & 0xFF) as u8;

            // Forward to console channel
            channel_write(console, buf.as_ptr(), 1);
        }
    }
}
