#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

use drv_fb::Console;
use kumo_abi::{BootInfo, Handle, VmarFlags};
use kumo_rt::{
    channel_read, channel_read_with_handle, debug_write, handle_koid, port_bind, port_create,
    port_wait, resource_mint_mmio, vmar_map,
};

kumo_rt::entry!(main);

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
    debug_write(b"drv-fb starting\n".as_ptr(), 16);

    let bootstrap = Handle(bootstrap_channel as u32);
    let mut buf = [0u8; 32];

    // Read fb_res, console, and bootinfo VMO sequentially from the bootstrap channel.
    let (_n1, res_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
    let res = Handle(res_raw as u32);
    let (_n2, console_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
    let console = Handle(console_raw as u32);
    let (_n3, bootinfo_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
    let bootinfo_vmo = Handle(bootinfo_raw as u32);

    if res_raw == 0 || console_raw == 0 || bootinfo_raw == 0 {
        debug_write(b"drv-fb: missing handles\n".as_ptr(), 24);
        kumo_rt::process_exit(1);
    }

    // Map BootInfo to read framebuffer geometry (exercises J156 live-write path).
    let bootinfo_va = 0x0000_0000_1000_0000;
    if vmar_map(
        Handle(0),
        bootinfo_vmo,
        0,
        bootinfo_va,
        4096,
        (VmarFlags::READ).0,
    ) != 0
    {
        debug_write(b"drv-fb: bootinfo map failed\n".as_ptr(), 28);
        kumo_rt::process_exit(1);
    }

    let bootinfo = unsafe { &*(bootinfo_va as *const BootInfo) };
    if !bootinfo.has_framebuffer() {
        debug_write(b"drv-fb: no fb in bootinfo\n".as_ptr(), 26);
        kumo_rt::process_exit(1);
    }

    let fb_phys = bootinfo.framebuffer.phys;
    let fb_len = bootinfo.framebuffer.len;
    let width = bootinfo.framebuffer.width as usize;
    let height = bootinfo.framebuffer.height as usize;
    let stride = bootinfo.framebuffer.stride as usize;

    // Mint and map the actual framebuffer.
    let fb_vmo_h = resource_mint_mmio(res, fb_phys, fb_len);
    if fb_vmo_h == u64::MAX {
        debug_write(b"drv-fb: fb vmo mint failed\n".as_ptr(), 27);
        kumo_rt::process_exit(1);
    }

    // Map the framebuffer just after the bootinfo page. Both fit easily within
    // the child VMAR ([0, 0x2000_0000) — 512 MiB, half-open). The original VA
    // 0x2000_0000 was exactly at the boundary and would be rejected by Vmar::map.
    let fb_va = 0x0000_0000_1000_1000;
    if vmar_map(
        Handle(0),
        Handle(fb_vmo_h as u32),
        0,
        fb_va,
        fb_len,
        (VmarFlags::READ | VmarFlags::WRITE).0,
    ) != 0
    {
        debug_write(b"drv-fb: fb map failed\n".as_ptr(), 22);
        kumo_rt::process_exit(1);
    }

    debug_write(b"drv-fb: initialized\n".as_ptr(), 20);

    // Build the text console over the framebuffer (clears the screen) and show first
    // light.
    let mut con = unsafe { Console::new(fb_va as *mut u32, width, height, stride) };
    con.write(b"KUMO drv-fb console ready\n");

    debug_write(b"drv-fb: console ready\n".as_ptr(), 22);

    // Pump the console channel: the bytes Sora writes become glyphs on the framebuffer.
    // Output-only — no keyboard device sits behind the framebuffer yet.
    let port_h = port_create();
    let console_koid = handle_koid(console);
    if port_h == u64::MAX
        || console_koid == u64::MAX
        || port_bind(Handle(port_h as u32), console) != 0
    {
        debug_write(b"drv-fb: port setup failed\n".as_ptr(), 26);
        kumo_rt::process_exit(1);
    }
    let port = Handle(port_h as u32);
    let console_source = Handle(console_koid as u32);

    let mut rx = [0u8; 256];
    loop {
        let source = Handle(port_wait(port) as u32);
        if source == console_source {
            let n = channel_read(console, rx.as_mut_ptr(), rx.len()) as usize;
            if n > 0 {
                con.write(&rx[..n]);
            }
        }
    }
}
