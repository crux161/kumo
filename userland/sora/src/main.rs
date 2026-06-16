#![no_std]
#![no_main]

extern crate alloc;

use kumo_abi::{Errno, Handle, PERSONA_LINUX_HELLO_PATH, SVC_HEALTH_PATH};
use kumo_rt::{
    address_space_create, channel_create, channel_create_pair, channel_read, channel_write,
    channel_write_with_handle, debug_write, handle_koid, interrupt_create, port_bind_channel,
    port_create, port_wait, process_create, process_run, process_wait, thread_create, thread_start,
    vmar_map, vmo_create, vmo_read, vmo_write,
};
use kumoza::parse;
use persona_linux::{arm64 as linux_arm64, elf as linux_elf};
use svc_health::{Request as HealthRequest, Response as HealthResponse};

kumo_rt::entry!(sora_main);

fn log(msg: &[u8]) {
    kumo_rt::debug_write(msg.as_ptr(), msg.len());
}

/// Bootstrap args (arrive in x0-x7, aarch64 calling convention):
///   x0: root-channel handle
///   x1: framebuffer virtual address (0 if no FB)
///   x2: console channel handle
///   x3: initrd VMO handle
///   x4: block-server channel handle (P7-g)
///   x5: root Resource handle (P9-b)
///   x6: network channel handle (P9-c)
///   x7: keyboard channel handle (P8-a restoration)
#[no_mangle]
extern "C" fn sora_main(
    root_handle: u64,
    _fb_va: u64,
    console_handle: u64,
    initrd_vmo: u64,
    block_handle: u64,
    resource_handle: u64,
    net_handle: u64,
    kbd_handle: u64,
) -> ! {
    let root = Handle(root_handle as u32);
    let console = Handle(console_handle as u32);
    let initrd = Handle(initrd_vmo as u32);
    let block = Handle(block_handle as u32);
    let _res = Handle(resource_handle as u32);
    let net = Handle(net_handle as u32);
    let kbd = Handle(kbd_handle as u32);

    // Greeting.
    debug_write(b"hello from Sora via SVC\n".as_ptr(), 24);

    // Read the kernel's boot message from the root channel.
    let mut buf = [0u8; 64];
    let n = channel_read(root, buf.as_mut_ptr(), 64) as usize;
    if n > 0 {
        debug_write(buf.as_ptr(), n);
    }

    // Acknowledge — Sora is alive and will serve the console channel.
    channel_write(root, b"sora ack\n".as_ptr(), 9);

    // P7-a: acknowledge the initrd VMO handle.
    debug_write(b"initrd vmo h0\n".as_ptr(), 14);

    // P7-b: read the initrd magic (KUMORD01) and echo it.
    let mut magic = [0u8; 8];
    if vmo_read(initrd, 0, magic.as_mut_ptr(), 8) == 0 {
        debug_write(magic.as_ptr(), 8);
        debug_write(b"\n".as_ptr(), 1);
    }

    // P7-f: walk all initrd entries to find a file by name (userspace find_file).
    // Initrd layout: header(16) + entries(N*80) + data.
    let mut header = [0u8; 16];
    if vmo_read(initrd, 0, header.as_mut_ptr(), 16) == 0 {
        let entry_count = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
        let target = b"bin/sora";
        let mut entry = [0u8; 80];
        let mut found = false;
        for i in 0..entry_count {
            let offset = 16u64 + (i as u64) * 80;
            if vmo_read(initrd, offset, entry.as_mut_ptr(), 80) != 0 {
                break;
            }
            let path_len = entry[..64].iter().position(|b| *b == 0).unwrap_or(64);
            if path_len == 0 || path_len != target.len() {
                continue;
            }
            if entry[..path_len] != target[..] {
                continue;
            }
            // Found the target file.
            let file_off = u64::from_le_bytes(entry[64..72].try_into().unwrap());
            let file_len = u64::from_le_bytes(entry[72..80].try_into().unwrap());
            debug_write(b"found: ".as_ptr(), 7);
            debug_write(entry.as_ptr(), path_len);
            debug_write(b"\n".as_ptr(), 1);

            // Read and echo the first 4 bytes of the file (ELF magic).
            let mut head = [0u8; 4];
            let n = (file_len as usize).min(4);
            if vmo_read(initrd, file_off, head.as_mut_ptr(), n) == 0 {
                debug_write(head.as_ptr(), n);
                debug_write(b"\n".as_ptr(), 1);
            }
            found = true;
            break;
        }
        if !found {
            debug_write(b"file not found\n".as_ptr(), 15);
        }
    }

    // P7-i: locate the FAT32 disk image, parse the BPB, and walk the root directory.
    // Prove userspace can read a real filesystem: compute the data-region layout from
    // the BPB, read the root-directory sector, and list every 8.3 entry.
    {
        let fat_path = b"bin/fat32.img";
        let mut header = [0u8; 16];
        if vmo_read(initrd, 0, header.as_mut_ptr(), 16) == 0 {
            let entry_count = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
            let mut entry = [0u8; 80];
            let mut fat_off = 0u64;
            for i in 0..entry_count {
                let off = 16u64 + (i as u64) * 80;
                if vmo_read(initrd, off, entry.as_mut_ptr(), 80) != 0 {
                    break;
                }
                let plen = entry[..64].iter().position(|b| *b == 0).unwrap_or(64);
                if plen == fat_path.len() && entry[..plen] == fat_path[..] {
                    fat_off = u64::from_le_bytes(entry[64..72].try_into().unwrap());
                    break;
                }
            }
            if fat_off != 0 {
                let mut bpb = [0u8; 512];
                if vmo_read(initrd, fat_off, bpb.as_mut_ptr(), 512) == 0 {
                    let bps = u16::from_le_bytes(bpb[0x0B..0x0D].try_into().unwrap()) as u64;
                    let spc = bpb[0x0D] as u64;
                    let rsvd = u16::from_le_bytes(bpb[0x0E..0x10].try_into().unwrap()) as u64;
                    let nfat = bpb[0x10] as u64;
                    let spf = u32::from_le_bytes(bpb[0x24..0x28].try_into().unwrap()) as u64;
                    let root = u32::from_le_bytes(bpb[0x2C..0x30].try_into().unwrap()) as u64;
                    let data_start = rsvd + nfat * spf;
                    let root_sec = data_start + (root - 2) * spc;
                    let root_off = fat_off + root_sec * bps;

                    let mut dir = [0u8; 512];
                    if vmo_read(initrd, root_off, dir.as_mut_ptr(), 512) == 0 {
                        let mut pos = 0;
                        while pos + 32 <= 512 && dir[pos] != 0x00 {
                            let attr = dir[pos + 11];
                            if attr != 0x0F && attr != 0x08 {
                                // 8.3 filename: 8-char name + 3-char extension
                                let name = &dir[pos..pos + 8];
                                let ext = &dir[pos + 8..pos + 11];
                                let name_end = name.iter().position(|b| *b == b' ');
                                let ext_end = ext.iter().position(|b| *b == b' ');
                                let nl = name_end.unwrap_or(8);
                                let el = ext_end.unwrap_or(3);
                                if nl > 0 {
                                    debug_write(name.as_ptr(), nl);
                                    if el > 0 {
                                        debug_write(b".".as_ptr(), 1);
                                        debug_write(ext.as_ptr(), el);
                                    }
                                    debug_write(b"\n".as_ptr(), 1);
                                }
                            }
                            pos += 32;
                        }
                    }
                }
            }
        }
    }

    // P8-g/h: demo ProcessCreate + VmarMap — create a child process, map the initrd
    // VMO into it, then print both handles. The child is still an empty shell (no
    // threads, no page tables); ThreadCreate + ThreadStart follow in P8-i.
    // VMAR must cover the address range the child actually uses (shared TTBR0).
    // Sora's code/data/stack live in 0x0..0x2000_0000; use a generous 512 MiB.
    let child_h = process_create(0x0000_0000_0000_0000, 0x0000_0000_2000_0000);
    if child_h != u64::MAX {
        let child = Handle(child_h as u32);
        // Map one page of the initrd VMO at the child's USER_IMAGE_BASE.
        let map_status = vmar_map(
            child,
            initrd,
            0,                     // vmo_offset
            0x0000_0000_1000_0000, // virt (within child VMAR)
            0x1000,                // 4 KiB
            1,                     // READ flag
        );
        debug_write(b"child process h".as_ptr(), 15);
        let mut h = child_h;
        let mut hex = [0u8; 16];
        let mut hi = 16;
        loop {
            hi -= 1;
            let d = (h & 0xF) as u8;
            hex[hi] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
            h >>= 4;
            if h == 0 && hi <= 16 - 4 {
                break;
            }
        }
        debug_write(hex[hi..].as_ptr(), 16 - hi);
        debug_write(b" map=".as_ptr(), 5);
        if map_status == 0 {
            debug_write(b"ok".as_ptr(), 2);
        } else {
            debug_write(b"fail".as_ptr(), 4);
        }
        // P8-i: create a thread in the child and start it (scaffold — kernel thread,
        // no user-mode page tables yet; won't actually run).
        let th_h = thread_create(child);
        if th_h != u64::MAX {
            let th = Handle(th_h as u32);
            let start_ok = thread_start(th, 0x4000_0000, 0x5000_0000, 0);
            debug_write(b" thread=".as_ptr(), 8);
            if start_ok == 0 {
                debug_write(b"ok\n".as_ptr(), 3);
            } else {
                debug_write(b"fail\n".as_ptr(), 5);
            }
        } else {
            debug_write(b" thread=fail\n".as_ptr(), 13);
        }
        // P9-h (retired): a cross-process pipe was demonstrated here by `process_run`ing
        // `child_payload` in Sora's *shared* address space. P10 replaced that fiction with
        // real per-process address spaces, and `ProcessRun` now requires one (built via
        // AddressSpaceCreate) and runs the child as a scheduled thread on its own TTBR0 —
        // so a shared-AS "child" is no longer valid. The P10 child-AS demo below is the
        // successor; cross-process IPC from a real child returns once a child process may
        // issue channel syscalls (today the SVC hook limits non-Sora processes to
        // DebugWrite).
    }

    // P9-a: create an Interrupt object bound to the timer IRQ (27). The handle
    // proves interrupt infrastructure works. InterruptWait is not called here —
    // it would park Sora before the serve loop, breaking channel dispatch.
    let timer_irq = interrupt_create(27);
    if timer_irq != u64::MAX {
        debug_write(b"timer irq ok\n".as_ptr(), 13);
    }

    // P9-e: handle transfer test. Create a channel pair, transfer one handle
    // to the kernel via the net channel. The kernel reads it and verifies.
    let ch = channel_create();
    if ch != u64::MAX {
        let h0 = Handle(ch as u32);
        channel_write_with_handle(net, b"h".as_ptr(), 1, h0);
    }

    // P10-b (process model): anonymous VMO + child separate address space.
    // Write a child asm payload to a fresh VMO, map it executable into the child,
    // build page tables, and run with the child's own TTBR0. Proves child code no
    // longer mutates the shared initrd VMO.
    //
    // Payload (8 aarch64 instructions, 32 bytes at offset 0x1100), verified with llvm-mc:
    //   movz x0, #0x1000, lsl #16  → x0 = 0x1000_0000
    //   movk x0, #0x1000           → x0 = 0x1000_1000 (string at child VA+0x1000)
    //   movz x1, #20               → len = 20
    //   movz x8, #29               → DebugWrite
    //   svc  #0
    //   movz x0, #0                → exit code 0
    //   movz x8, #21               → ProcessExit
    //   svc  #0
    // P10-f: verify ChannelWrite + handle transfer across a channel pair.
    // The channel_create_pair asm used `in(reg)` which could alias x8 with
    // the output register, causing ChannelCreate to execute as VmoCreate
    // (handle-is-Vmo). Fixed by switching to explicit `in("x8")`.
    {
        let (a0, a1) = channel_create_pair();
        let (b0, b1) = channel_create_pair();
        if a0 != u64::MAX && a1 != u64::MAX && b0 != u64::MAX && b1 != u64::MAX {
            let st = channel_write(Handle(a0 as u32), b"T".as_ptr(), 1);
            let st2 =
                channel_write_with_handle(Handle(a0 as u32), b"x".as_ptr(), 1, Handle(b1 as u32));
            if st == 0 && st2 == 0 {
                log(b"P10-f: ch-write+transfer ok\n");
            } else {
                log(b"P10-f: ch-write fail\n");
            }
        }
    }

    // String at VMO offset 0x1000: "hello from child ch\n" (20 bytes).
    // String at VMO offset 0x1020: "child xfer ok!\n" (16 bytes).
    const RUN_BLOCKING_CHILD_XFER_DEMO: bool = false;
    if RUN_BLOCKING_CHILD_XFER_DEMO {
        // P10-f: channel-based cross-process handle transfer. Sora writes to
        // ch0 WITH xfer1 as a transferred handle BEFORE process_run, so the
        // message waits in ch1's inbox. The child's first act is ChannelRead
        // on ch1, which installs xfer1 in its own handle table. The child
        // then writes to xfer1 and exits. Sora reads from xfer0 to verify.
        //
        // Payload (22 aarch64 instructions, 88 bytes at VMO offset 0x1100),
        // verified with llvm-mc -triple=aarch64 --show-encoding:
        let code: [u32; 22] = [
            0xaa0003f3, // mov x19, x0              (save bootstrap handle)
            // ChannelRead(x19, buf=0x1000FE00, cap=32) — buf on RW stack
            0xaa1303e0, // mov x0, x19              (channel = bootstrap)
            0xd2a20001, // movz x1, #0x1000, lsl #16
            0xf29fc001, // movk x1, #0xFE00          (buf = 0x1000FE00)
            0xd2800402, // movz x2, #32              (cap = 32)
            0xd28000a8, // movz x8, #5               (ChannelRead)
            0xd4000001, // svc #0
            0xaa0103f4, // mov x20, x1               (save transferred handle)
            // ChannelWrite(x20, string_xfer, 16)
            0xaa1403e0, // mov x0, x20              (channel = received)
            0xd2a20001, // movz x1, #0x1000, lsl #16
            0xf2820401, // movk x1, #0x1020          (ptr = 0x10001020)
            0xd2800202, // movz x2, #16              (len = 16)
            0xd2800088, // movz x8, #4               (ChannelWrite)
            0xd4000001, // svc #0
            // DebugWrite(string_hello, 20)
            0xd2a20000, // movz x0, #0x1000, lsl #16
            0xf2820000, // movk x0, #0x1000          (ptr = 0x10001000)
            0xd2800281, // movz x1, #20              (len = 20)
            0xd28003a8, // movz x8, #29              (DebugWrite)
            0xd4000001, // svc #0
            // ProcessExit(0)
            0xd2800000, // movz x0, #0
            0xd28002a8, // movz x8, #21              (ProcessExit)
            0xd4000001, // svc #0
        ];
        let mut code_bytes = [0u8; 88];
        for (i, w) in code.iter().enumerate() {
            code_bytes[i * 4..(i + 1) * 4].copy_from_slice(&w.to_le_bytes());
        }
        let child_vmo_h = vmo_create(0x2000);
        if child_vmo_h == u64::MAX {
            log(b" child as=nop (vmo create fail)\n");
        } else {
            let child_vmo = Handle(child_vmo_h as u32);
            let s1 = vmo_write(child_vmo, 0x1000, b"hello from child ch\n".as_ptr(), 20) == 0;
            let s2 = vmo_write(child_vmo, 0x1020, b"child xfer ok!\n".as_ptr(), 16) == 0;
            let c_ok = vmo_write(child_vmo, 0x1100, code_bytes.as_ptr(), 88) == 0;
            if !s1 || !s2 {
                log(b" child as=nop (string write fail)\n");
            } else if !c_ok {
                log(b" child as=nop (code write fail)\n");
            } else {
                let child_as_h = process_create(0x0000_0000_0000_0000, 0x0000_0000_2000_0000);
                if child_as_h != u64::MAX {
                    let child_as = Handle(child_as_h as u32);
                    // Map the 3 pages holding strings (0x1000, 0x1020) and code
                    // (0x1100) at child VA 0x10000000 as RX (READ|EXECUTE = 5).
                    if vmar_map(child_as, child_vmo, 0, 0x10000000, 0x2000, 5) != 0 {
                        log(b" child as=nop (map fail)\n");
                    } else if address_space_create(child_as, 0x10010000, 0x4000) == u64::MAX {
                        log(b" child as=nop (addr space fail)\n");
                    } else {
                        // P10-f: channel-based handle transfer. Create two pairs.
                        // Sora writes to ch0 with xfer1 as transfer BEFORE
                        // process_run; the child reads from ch1 (passed as the
                        // bootstrap arg) and receives xfer1 via install_transfers.
                        let (ch0, ch1) = channel_create_pair();
                        let (xfer0, xfer1) = channel_create_pair();
                        let run_as = if ch0 != u64::MAX
                            && ch1 != u64::MAX
                            && xfer0 != u64::MAX
                            && xfer1 != u64::MAX
                        {
                            // Queue the handle transfer before the child starts.
                            let xfer_st = channel_write_with_handle(
                                Handle(ch0 as u32),
                                b"x".as_ptr(),
                                1,
                                Handle(xfer1 as u32),
                            );
                            let ok = process_run(child_as, 0x10001100, 0x1000FFF0, ch1, 0, 0);
                            // Read from xfer0: the child wrote to xfer1.
                            let mut xfer_buf = [0u8; 32];
                            let xn = channel_read(Handle(xfer0 as u32), xfer_buf.as_mut_ptr(), 32)
                                as usize;
                            if xn > 0 {
                                log(b"child xfer: ");
                                debug_write(xfer_buf.as_ptr(), xn);
                            } else {
                                log(b"child xfer: (none) st=");
                                let mut hx = [0u8; 4];
                                let mut v = xfer_st as u64;
                                let mut i = 4;
                                loop {
                                    i -= 1;
                                    let d = (v & 0xF) as u8;
                                    hx[i] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
                                    v >>= 4;
                                    if v == 0 && i <= 1 {
                                        break;
                                    }
                                }
                                debug_write(hx[i..].as_ptr(), 4 - i);
                                debug_write(b"\n".as_ptr(), 1);
                            }
                            ok
                        } else {
                            u64::MAX
                        };
                        log(b" child as run=");
                        if run_as == 0 {
                            log(b"ok\n");
                        } else {
                            log(b"fail\n");
                        }
                    }
                } else {
                    log(b" child as=nop (process create fail)\n");
                }
            }
        }
    } // P10-g: async child demo — non-blocking ProcessRun (flags=1).
      // Sora spawns the child, writes "go" to the bootstrap channel,
      // calls process_wait (child preempts, reads "go", writes "world!\n",
      // DebugWrites, exits). Sora resumes, reads "world!\n" from c0.
    const RUN_ASYNC_CHILD_DEMO: bool = false;
    if RUN_ASYNC_CHILD_DEMO {
        let child_vmo_h = vmo_create(0x2000);
        if child_vmo_h != u64::MAX {
            let child_vmo = Handle(child_vmo_h as u32);
            // Code: save handle + ChannelRead + ChannelWrite + DebugWrite + ProcessExit
            // String at 0x1000: "async child ok\n" (15 bytes)
            // String at 0x1020: "world!\n" (6 bytes)
            let code: [u32; 21] = [
                0xaa0003f3, // mov x19, x0              (save bootstrap handle)
                // ChannelRead(x19, buf=0x1000FE00, cap=32)
                0xaa1303e0, // mov x0, x19
                0xd2a20001, // movz x1, #0x1000, lsl #16
                0xf29fc001, // movk x1, #0xFE00
                0xd2800402, // movz x2, #32
                0xd28000a8, // movz x8, #5               (ChannelRead)
                0xd4000001, // svc #0
                // ChannelWrite(x19, reply, 6)
                0xaa1303e0, // mov x0, x19
                0xd2a20001, // movz x1, #0x1000, lsl #16
                0xf2820401, // movk x1, #0x1020
                0xd28000c2, // movz x2, #6
                0xd2800088, // movz x8, #4               (ChannelWrite)
                0xd4000001, // svc #0
                // DebugWrite(string, 15)
                0xd2a20000, // movz x0, #0x1000, lsl #16
                0xf2820000, // movk x0, #0x1000
                0xd28001e1, // movz x1, #15
                0xd28003a8, // movz x8, #29              (DebugWrite)
                0xd4000001, // svc #0
                // ProcessExit(0)
                0xd2800000, // movz x0, #0
                0xd28002a8, // movz x8, #21              (ProcessExit)
                0xd4000001, // svc #0
            ];
            let mut cb = [0u8; 84];
            for (i, w) in code.iter().enumerate() {
                cb[i * 4..(i + 1) * 4].copy_from_slice(&w.to_le_bytes());
            }
            let s1 = vmo_write(child_vmo, 0x1000, b"async child ok\n".as_ptr(), 15) == 0;
            let s2 = vmo_write(child_vmo, 0x1020, b"world!\n".as_ptr(), 6) == 0;
            let c_ok = vmo_write(child_vmo, 0x1100, cb.as_ptr(), 84) == 0;
            if s1 && s2 && c_ok {
                let child_as_h = process_create(0, 0x2000_0000);
                if child_as_h != u64::MAX {
                    let child_as = Handle(child_as_h as u32);
                    if vmar_map(child_as, child_vmo, 0, 0x10000000, 0x2000, 5) == 0
                        && address_space_create(child_as, 0x10010000, 0x4000) != u64::MAX
                    {
                        let (c0, c1) = channel_create_pair();
                        if c0 != u64::MAX && c1 != u64::MAX {
                            // flags=1 → async
                            let st = process_run(child_as, 0x10001100, 0x1000FFF0, c1, 0, 1);
                            if st == 0 {
                                channel_write(Handle(c0 as u32), b"go".as_ptr(), 2);
                                process_wait();
                                let mut rbuf = [0u8; 16];
                                let rn =
                                    channel_read(Handle(c0 as u32), rbuf.as_mut_ptr(), 16) as usize;
                                if rn > 0 {
                                    log(b"async reply: ");
                                    debug_write(rbuf.as_ptr(), rn);
                                }
                                log(b"async run=ok\n");
                            } else {
                                log(b"async run=fail\n");
                            }
                        }
                    }
                }
            }
        }
    }

    // M10-a: first `persona-linux` smoke. This is not a native KUMO child: the
    // payload uses ARM64 Linux syscall numbers (`write` = 64, `exit_group` = 94).
    // The temporary Stage-A bridge in the kernel translates those numbers only for
    // non-Sora children until the userspace persona runner owns the trap path.
    {
        let _ = linux_arm64::WRITE;
        let _ = linux_arm64::EXIT_GROUP;
        let linux_msg = b"M10 linux hello\n";
        let child_vmo_h = vmo_create(0x2000);
        if child_vmo_h == u64::MAX {
            log(b"M10 linux: vmo fail\n");
        } else if linux_msg.len() != 16 {
            log(b"M10 linux: len fail\n");
        } else {
            let child_vmo = Handle(child_vmo_h as u32);
            // Payload (9 aarch64 instructions, 36 bytes at VMO offset 0x1100),
            // verified with `llvm-mc -triple=aarch64 --show-encoding`:
            //   movz x0, #1                → fd = stdout
            //   movz x1, #0x1000, lsl #16 → x1 = 0x1000_0000
            //   movk x1, #0x1000          → buf = 0x1000_1000
            //   movz x2, #16              → len
            //   movz x8, #64              → Linux arm64 write
            //   svc  #0
            //   movz x0, #0               → status
            //   movz x8, #94              → Linux arm64 exit_group
            //   svc  #0
            let code: [u32; 9] = [
                0xd2800020, // movz x0, #1
                0xd2a20001, // movz x1, #0x1000, lsl #16
                0xf2820001, // movk x1, #0x1000
                0xd2800202, // movz x2, #16
                0xd2800808, // movz x8, #64
                0xd4000001, // svc #0
                0xd2800000, // movz x0, #0
                0xd2800bc8, // movz x8, #94
                0xd4000001, // svc #0
            ];
            let mut cb = [0u8; 36];
            for (i, w) in code.iter().enumerate() {
                cb[i * 4..(i + 1) * 4].copy_from_slice(&w.to_le_bytes());
            }
            let msg_ok = vmo_write(child_vmo, 0x1000, linux_msg.as_ptr(), linux_msg.len()) == 0;
            let code_ok = vmo_write(child_vmo, 0x1100, cb.as_ptr(), cb.len()) == 0;
            if !msg_ok || !code_ok {
                log(b"M10 linux: write fail\n");
            } else {
                let child_as_h = process_create(0, 0x2000_0000);
                if child_as_h == u64::MAX {
                    log(b"M10 linux: process fail\n");
                } else {
                    let child_as = Handle(child_as_h as u32);
                    if vmar_map(child_as, child_vmo, 0, 0x10000000, 0x2000, 5) != 0 {
                        log(b"M10 linux: map fail\n");
                    } else if address_space_create(child_as, 0x10010000, 0x4000) == u64::MAX {
                        log(b"M10 linux: as fail\n");
                    } else {
                        let st = process_run(child_as, 0x10001100, 0x1000FFF0, 0, 0, 0);
                        if st == 0 {
                            log(b"M10 linux: run ok\n");
                        } else {
                            log(b"M10 linux: run fail\n");
                        }
                    }
                }
            }
        }
    }

    // M10-b: load the same hello path as an initrd-resident ARM64 Linux ELF.
    // This is the first step from handmade payload toward "one static ELF".
    if run_persona_linux_elf(initrd) {
        log(b"M10 elf: run ok\n");
    } else {
        log(b"M10 elf: run fail\n");
    }

    // Stage-C seed: spawn the first real server as its own process, with exactly one
    // request channel. Sora keeps the peer and verifies Ping plus Status replies.
    if run_svc_health_smoke(initrd) {
        log(b"svc-health: serve ok\n");
    } else {
        log(b"svc-health: serve fail\n");
    }

    // P10-b: VmoWrite/VmoRead demo on anonymous backing.
    let scratch_h = vmo_create(0x1000);
    if scratch_h != u64::MAX {
        let scratch = Handle(scratch_h as u32);
        let mut vbuf = [0u8; 8];
        if vmo_write(scratch, 8, b"VMO_OK\n".as_ptr(), 6) == 0
            && vmo_read(scratch, 8, vbuf.as_mut_ptr(), 6) == 0
            && &vbuf[..6] == b"VMO_OK"
        {
            log(b"anon vmo write ok\n");
        }
    }

    // Port/wait-many serve loop. A single PortWait parks once and wakes when
    // either console or block has data. Source koid matching dispatches to
    // the right handler. This replaces the per-channel-park loops and lifts
    // the 2-channel limit.
    let port_h = port_create();
    let console_koid = handle_koid(console);
    let block_koid = handle_koid(block);
    let net_koid = handle_koid(net);
    let kbd_koid = handle_koid(kbd);
    if port_h != u64::MAX
        && console_koid != u64::MAX
        && block_koid != u64::MAX
        && net_koid != u64::MAX
        && kbd_koid != u64::MAX
    {
        let port = Handle(port_h as u32);
        port_bind_channel(port, console);
        port_bind_channel(port, block);
        port_bind_channel(port, net);
        port_bind_channel(port, kbd);
        let cons_koid = Handle(console_koid as u32);
        let blk_koid = Handle(block_koid as u32);
        let net_k = Handle(net_koid as u32);
        let kbd_k = Handle(kbd_koid as u32);
        let mut serve_buf = [0u8; 256];
        let mut block_buf = [0u8; 512];
        let mut line_buf = [0u8; 256];
        let mut line_pos = 0usize;
        loop {
            let source = Handle(port_wait(port) as u32);
            // console handler — output only (klog! → DebugWrite)
            if source == cons_koid {
                let n = channel_read(console, serve_buf.as_mut_ptr(), 256) as usize;
                if n > 0 {
                    debug_write(serve_buf.as_ptr(), n);
                }
            }
            if source == blk_koid {
                let n = channel_read(block, serve_buf.as_mut_ptr(), 256) as usize;
                if n == 0 {
                    // Spurious wake: our own reply re-signalled this port (the engine signals
                    // the writer's bound port on every ChannelWrite). The request was already
                    // consumed, so the inbox is empty — do NOT reply, or each empty read writes
                    // another reply that re-signals again, spinning the serve loop until the
                    // kernel heap is exhausted.
                } else if n == 16 {
                    let offset = u64::from_le_bytes(serve_buf[..8].try_into().unwrap());
                    let len = u64::from_le_bytes(serve_buf[8..16].try_into().unwrap());
                    let len = (len as usize).min(block_buf.len());
                    if len > 0 && vmo_read(initrd, offset, block_buf.as_mut_ptr(), len) == 0 {
                        channel_write(block, block_buf.as_ptr(), len);
                    } else {
                        channel_write(block, b"".as_ptr(), 0);
                    }
                } else {
                    let served = if serve_buf[0] == 0x01 && n >= 17 {
                        let file_off = u64::from_le_bytes(serve_buf[1..9].try_into().unwrap());
                        let req_len = u64::from_le_bytes(serve_buf[9..17].try_into().unwrap());
                        serve_file_read_at(
                            initrd,
                            &serve_buf[17..n],
                            file_off,
                            req_len,
                            &mut block_buf,
                        )
                    } else {
                        serve_file_read(initrd, &serve_buf[..n], &mut block_buf)
                    };
                    channel_write(block, block_buf.as_ptr(), served);
                }
            }
            // net handler — loopback echo + lightweight control acks.
            if source == net_k {
                let n = channel_read(net, serve_buf.as_mut_ptr(), 256) as usize;
                if n == 4 && &serve_buf[..4] == b"conn" {
                    // The kernel POST cannot receive Sora-process handles as capabilities yet.
                    // Return a protocol token instead of allocating ephemeral local channels.
                    channel_write(net, b"conn".as_ptr(), 4);
                } else if n > 5 && &serve_buf[..5] == b"pipe:" {
                    channel_write(net, b"pipe".as_ptr(), 4);
                } else if n > 0 {
                    channel_write(net, serve_buf.as_ptr(), n);
                }
            }
            // keyboard handler — input only (keystrokes → line buffer)
            if source == kbd_k {
                let n = channel_read(kbd, serve_buf.as_mut_ptr(), 256) as usize;
                if n > 0 {
                    for i in 0..n {
                        let byte = serve_buf[i];
                        if byte == b'\r' || byte == b'\n' {
                            debug_write(b"\r\n".as_ptr(), 2);
                            if let Ok(ls) = core::str::from_utf8(&line_buf[..line_pos]) {
                                if let Some(stmt) = parse(ls) {
                                    let cl = &line_buf[..line_pos];
                                    kumoza::evaluate(&stmt, |cmd| {
                                        if cmd.name == "echo" {
                                            for (j, a) in cmd.args.iter().enumerate() {
                                                if j > 0 {
                                                    debug_write(b" ".as_ptr(), 1);
                                                }
                                                debug_write(a.as_ptr(), a.len());
                                            }
                                            debug_write(b"\n".as_ptr(), 1);
                                        } else if cmd.name == "help" {
                                            let msg = b"KUMO Sora userspace shell (scaffold)\n\
                                                builtins: echo, help\n\
                                                other commands run via kernel shell\n";
                                            debug_write(msg.as_ptr(), msg.len());
                                        } else if !cmd.name.is_empty() {
                                            channel_write(root, cl.as_ptr(), cl.len());
                                        }
                                        true
                                    });
                                }
                            }
                            line_pos = 0;
                        } else if byte == 0x08 || byte == 0x7f {
                            if line_pos > 0 {
                                line_pos -= 1;
                                debug_write(b"\x08 \x08".as_ptr(), 3);
                            }
                        } else if byte >= 0x20 && byte <= 0x7e {
                            if line_pos < line_buf.len() {
                                line_buf[line_pos] = byte;
                                line_pos += 1;
                                debug_write(serve_buf[i..].as_ptr(), 1);
                            }
                        }
                    }
                }
            }
        }
    } else {
        loop {
            core::hint::spin_loop();
        }
    }
}

/// Convert a path like "HELLO.TXT" to an 11-byte 8.3 FAT directory entry name.
/// Returns `None` if the name or extension is too long for 8.3 format.
fn path_to_8_3(path: &[u8]) -> Option<[u8; 11]> {
    let mut name = [b' '; 11];
    let dot = path.iter().position(|b| *b == b'.');
    let (base, ext) = match dot {
        Some(d) => (&path[..d], &path[d + 1..]),
        None => (path, &[][..]),
    };
    if base.len() > 8 || ext.len() > 3 || base.is_empty() {
        return None;
    }
    name[..base.len()].copy_from_slice(base);
    name[8..8 + ext.len()].copy_from_slice(ext);
    Some(name)
}

#[derive(Clone, Copy)]
struct PersonaLoadSegment {
    file_offset: u64,
    file_size: u64,
    virt_addr: u64,
    mem_size: u64,
    flags: u64,
}

fn find_initrd_file(initrd: Handle, target: &[u8]) -> Option<(u64, u64)> {
    let mut header = [0u8; 16];
    if vmo_read(initrd, 0, header.as_mut_ptr(), 16) != 0 {
        return None;
    }
    let entry_count = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    let mut entry = [0u8; 80];
    for i in 0..entry_count {
        let off = 16u64 + (i as u64) * 80;
        if vmo_read(initrd, off, entry.as_mut_ptr(), 80) != 0 {
            return None;
        }
        let plen = entry[..64].iter().position(|b| *b == 0).unwrap_or(64);
        if plen == target.len() && entry[..plen] == target[..] {
            let file_off = u64::from_le_bytes(entry[64..72].try_into().unwrap());
            let file_len = u64::from_le_bytes(entry[72..80].try_into().unwrap());
            return Some((file_off, file_len));
        }
    }
    None
}

fn run_persona_linux_elf(initrd: Handle) -> bool {
    const PAGE_SIZE: u64 = 4096;
    const MAX_SEGMENTS: usize = 8;

    fn align_down(value: u64) -> u64 {
        value & !(PAGE_SIZE - 1)
    }

    fn align_up(value: u64) -> Option<u64> {
        value
            .checked_add(PAGE_SIZE - 1)
            .map(|value| value & !(PAGE_SIZE - 1))
    }

    fn page_flags(elf_flags: u32) -> u64 {
        let mut flags = 0u64;
        if elf_flags & linux_elf::PF_R != 0 {
            flags |= 1;
        }
        if elf_flags & linux_elf::PF_W != 0 {
            flags |= 2;
        }
        if elf_flags & linux_elf::PF_X != 0 {
            flags |= 4;
        }
        flags
    }

    let (elf_off, elf_len) = match find_initrd_file(initrd, PERSONA_LINUX_HELLO_PATH.as_bytes()) {
        Some(file) => file,
        None => {
            log(b"M10 elf: missing\n");
            return false;
        }
    };

    let mut header = [0u8; linux_elf::ELF_HEADER_LEN];
    if vmo_read(initrd, elf_off, header.as_mut_ptr(), header.len()) != 0 {
        log(b"M10 elf: hdr read fail\n");
        return false;
    }
    let elf = match linux_elf::parse_header(&header) {
        Ok(elf) => elf,
        Err(_) => {
            log(b"M10 elf: bad hdr\n");
            return false;
        }
    };

    let ph_table_len = (elf.phnum as u64).saturating_mul(elf.phentsize as u64);
    if elf.phoff.saturating_add(ph_table_len) > elf_len || elf.phnum as usize > MAX_SEGMENTS {
        log(b"M10 elf: ph range fail\n");
        return false;
    }

    let empty = PersonaLoadSegment {
        file_offset: 0,
        file_size: 0,
        virt_addr: 0,
        mem_size: 0,
        flags: 0,
    };
    let mut segments = [empty; MAX_SEGMENTS];
    let mut segment_count = 0usize;
    let mut vmo_len = 0u64;

    for index in 0..elf.phnum as usize {
        let ph_off = elf_off + elf.phoff + (index as u64) * (elf.phentsize as u64);
        let mut ph_buf = [0u8; linux_elf::ELF_PHDR_LEN];
        if vmo_read(initrd, ph_off, ph_buf.as_mut_ptr(), ph_buf.len()) != 0 {
            log(b"M10 elf: ph read fail\n");
            return false;
        }
        let ph = match linux_elf::parse_program_header(&ph_buf) {
            Ok(ph) => ph,
            Err(_) => {
                log(b"M10 elf: bad ph\n");
                return false;
            }
        };
        if ph.kind != linux_elf::PT_LOAD {
            continue;
        }
        if segment_count >= MAX_SEGMENTS
            || ph.file_offset.saturating_add(ph.file_size) > elf_len
            || ph.file_offset.saturating_add(ph.mem_size) < ph.file_offset
        {
            log(b"M10 elf: segment range fail\n");
            return false;
        }
        vmo_len = vmo_len.max(ph.file_offset.saturating_add(ph.mem_size));
        segments[segment_count] = PersonaLoadSegment {
            file_offset: ph.file_offset,
            file_size: ph.file_size,
            virt_addr: ph.virt_addr,
            mem_size: ph.mem_size,
            flags: page_flags(ph.flags),
        };
        segment_count += 1;
    }

    if segment_count == 0 {
        log(b"M10 elf: no load\n");
        return false;
    }
    let vmo_len = match align_up(vmo_len) {
        Some(len) if len > 0 => len,
        _ => {
            log(b"M10 elf: size fail\n");
            return false;
        }
    };

    let child_vmo_h = vmo_create(vmo_len);
    if child_vmo_h == u64::MAX {
        log(b"M10 elf: vmo fail\n");
        return false;
    }
    let child_vmo = Handle(child_vmo_h as u32);

    let mut chunk = [0u8; 256];
    for segment in segments.iter().take(segment_count) {
        let mut copied = 0u64;
        while copied < segment.file_size {
            let n = ((segment.file_size - copied) as usize).min(chunk.len());
            if vmo_read(
                initrd,
                elf_off + segment.file_offset + copied,
                chunk.as_mut_ptr(),
                n,
            ) != 0
                || vmo_write(child_vmo, segment.file_offset + copied, chunk.as_ptr(), n) != 0
            {
                log(b"M10 elf: copy fail\n");
                return false;
            }
            copied += n as u64;
        }
    }

    let child_as_h = process_create(0, 0x2000_0000);
    if child_as_h == u64::MAX {
        log(b"M10 elf: process fail\n");
        return false;
    }
    let child_as = Handle(child_as_h as u32);

    for segment in segments.iter().take(segment_count) {
        let page_delta = segment.virt_addr & (PAGE_SIZE - 1);
        let virt = align_down(segment.virt_addr);
        let vmo_offset = align_down(segment.file_offset);
        let len = match align_up(page_delta.saturating_add(segment.mem_size)) {
            Some(len) => len,
            None => {
                log(b"M10 elf: map len fail\n");
                return false;
            }
        };
        if vmar_map(child_as, child_vmo, vmo_offset, virt, len, segment.flags) != 0 {
            log(b"M10 elf: map fail\n");
            return false;
        }
    }

    if address_space_create(child_as, 0x10010000, 0x4000) == u64::MAX {
        log(b"M10 elf: as fail\n");
        return false;
    }

    process_run(child_as, elf.entry, 0x1000FFF0, 0, 0, 0) == 0
}

fn run_svc_health_smoke(initrd: Handle) -> bool {
    match run_initrd_elf_with_channel(initrd, SVC_HEALTH_PATH.as_bytes()) {
        Some((child_as, entry, stack_top, server_chan, client_chan)) => {
            if process_run(child_as, entry, stack_top, server_chan.0 as u64, 0, 1) != 0 {
                log(b"svc-health: run fail\n");
                return false;
            }
            if !child_parked(process_wait()) {
                log(b"svc-health: initial park fail\n");
                return false;
            }

            let ping = HealthRequest::Ping.encode();
            if channel_write(client_chan, ping.as_ptr(), ping.len()) != 0 {
                log(b"svc-health: ping write fail\n");
                return false;
            }
            if !child_parked(process_wait()) {
                log(b"svc-health: ping pump fail\n");
                return false;
            }

            let mut reply = [0u8; 32];
            let n = channel_read(client_chan, reply.as_mut_ptr(), reply.len()) as usize;
            if HealthResponse::decode(&reply[..n]) != Some(HealthResponse::Pong) {
                return false;
            }

            let status = HealthRequest::Status.encode();
            if channel_write(client_chan, status.as_ptr(), status.len()) != 0 {
                log(b"svc-health: status write fail\n");
                return false;
            }
            if !child_parked(process_wait()) {
                log(b"svc-health: status pump fail\n");
                return false;
            }
            let n = channel_read(client_chan, reply.as_mut_ptr(), reply.len()) as usize;
            HealthResponse::decode(&reply[..n])
                == Some(HealthResponse::Status {
                    uptime_ticks: 0,
                    served: 2,
                })
        }
        None => false,
    }
}

fn child_parked(status: u64) -> bool {
    status == Errno::ShouldWait.status() as u32 as u64
}

fn run_initrd_elf_with_channel(
    initrd: Handle,
    path: &[u8],
) -> Option<(Handle, u64, u64, Handle, Handle)> {
    const PAGE_SIZE: u64 = 4096;
    const MAX_SEGMENTS: usize = 8;
    const STACK_TOP: u64 = 0x10010000;
    const STACK_SIZE: u64 = 0x4000;

    fn align_down(value: u64) -> u64 {
        value & !(PAGE_SIZE - 1)
    }

    fn align_up(value: u64) -> Option<u64> {
        value
            .checked_add(PAGE_SIZE - 1)
            .map(|value| value & !(PAGE_SIZE - 1))
    }

    fn page_flags(elf_flags: u32) -> u64 {
        let mut flags = 0u64;
        if elf_flags & linux_elf::PF_R != 0 {
            flags |= 1;
        }
        if elf_flags & linux_elf::PF_W != 0 {
            flags |= 2;
        }
        if elf_flags & linux_elf::PF_X != 0 {
            flags |= 4;
        }
        flags
    }

    let (elf_off, elf_len) = match find_initrd_file(initrd, path) {
        Some(file) => file,
        None => {
            log(b"svc-health: missing\n");
            return None;
        }
    };

    let mut header = [0u8; linux_elf::ELF_HEADER_LEN];
    if vmo_read(initrd, elf_off, header.as_mut_ptr(), header.len()) != 0 {
        log(b"svc-health: hdr read fail\n");
        return None;
    }
    let elf = match linux_elf::parse_header(&header) {
        Ok(elf) => elf,
        Err(_) => {
            log(b"svc-health: bad hdr\n");
            return None;
        }
    };

    let ph_table_len = (elf.phnum as u64).saturating_mul(elf.phentsize as u64);
    if elf.phoff.saturating_add(ph_table_len) > elf_len || elf.phnum as usize > MAX_SEGMENTS {
        log(b"svc-health: ph range fail\n");
        return None;
    }

    let empty = PersonaLoadSegment {
        file_offset: 0,
        file_size: 0,
        virt_addr: 0,
        mem_size: 0,
        flags: 0,
    };
    let mut segments = [empty; MAX_SEGMENTS];
    let mut segment_count = 0usize;
    let mut vmo_len = 0u64;

    for index in 0..elf.phnum as usize {
        let ph_off = elf_off + elf.phoff + (index as u64) * (elf.phentsize as u64);
        let mut ph_buf = [0u8; linux_elf::ELF_PHDR_LEN];
        if vmo_read(initrd, ph_off, ph_buf.as_mut_ptr(), ph_buf.len()) != 0 {
            log(b"svc-health: ph read fail\n");
            return None;
        }
        let ph = match linux_elf::parse_program_header(&ph_buf) {
            Ok(ph) => ph,
            Err(_) => {
                log(b"svc-health: bad ph\n");
                return None;
            }
        };
        if ph.kind != linux_elf::PT_LOAD {
            continue;
        }
        if segment_count >= MAX_SEGMENTS
            || ph.file_offset.saturating_add(ph.file_size) > elf_len
            || ph.file_offset.saturating_add(ph.mem_size) < ph.file_offset
        {
            log(b"svc-health: segment range fail\n");
            return None;
        }
        vmo_len = vmo_len.max(ph.file_offset.saturating_add(ph.mem_size));
        segments[segment_count] = PersonaLoadSegment {
            file_offset: ph.file_offset,
            file_size: ph.file_size,
            virt_addr: ph.virt_addr,
            mem_size: ph.mem_size,
            flags: page_flags(ph.flags),
        };
        segment_count += 1;
    }

    if segment_count == 0 {
        log(b"svc-health: no load\n");
        return None;
    }

    let child_vmo_h = vmo_create(align_up(vmo_len)?);
    if child_vmo_h == u64::MAX {
        log(b"svc-health: vmo fail\n");
        return None;
    }
    let child_vmo = Handle(child_vmo_h as u32);

    let mut chunk = [0u8; 256];
    for segment in segments.iter().take(segment_count) {
        let mut copied = 0u64;
        while copied < segment.file_size {
            let n = ((segment.file_size - copied) as usize).min(chunk.len());
            if vmo_read(
                initrd,
                elf_off + segment.file_offset + copied,
                chunk.as_mut_ptr(),
                n,
            ) != 0
                || vmo_write(child_vmo, segment.file_offset + copied, chunk.as_ptr(), n) != 0
            {
                log(b"svc-health: copy fail\n");
                return None;
            }
            copied += n as u64;
        }
    }

    let child_as_h = process_create(0, 0x2000_0000);
    if child_as_h == u64::MAX {
        log(b"svc-health: process fail\n");
        return None;
    }
    let child_as = Handle(child_as_h as u32);

    for segment in segments.iter().take(segment_count) {
        let page_delta = segment.virt_addr & (PAGE_SIZE - 1);
        let virt = align_down(segment.virt_addr);
        let vmo_offset = align_down(segment.file_offset);
        let len = align_up(page_delta.saturating_add(segment.mem_size))?;
        if vmar_map(child_as, child_vmo, vmo_offset, virt, len, segment.flags) != 0 {
            log(b"svc-health: map fail\n");
            return None;
        }
    }

    if address_space_create(child_as, STACK_TOP, STACK_SIZE) == u64::MAX {
        log(b"svc-health: as fail\n");
        return None;
    }

    let (client, server) = channel_create_pair();
    if client == u64::MAX || server == u64::MAX {
        log(b"svc-health: channel fail\n");
        return None;
    }

    Some((
        child_as,
        elf.entry,
        STACK_TOP - 0x10,
        Handle(server as u32),
        Handle(client as u32),
    ))
}

/// Resolve a file path against the FAT32 root directory and return up to `req_len` bytes
/// starting from byte `file_off`. Writes into `out` and returns bytes written (0 = error).
fn serve_file_read_at(
    initrd: Handle,
    path: &[u8],
    file_off: u64,
    req_len: u64,
    out: &mut [u8; 512],
) -> usize {
    let target = match path_to_8_3(path) {
        Some(n) => n,
        None => return 0,
    };

    // Locate bin/fat32.img.
    let mut header = [0u8; 16];
    if vmo_read(initrd, 0, header.as_mut_ptr(), 16) != 0 {
        return 0;
    }
    let entry_count = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    let mut entry = [0u8; 80];
    let fat_path = b"bin/fat32.img";
    let mut fat_off = 0u64;
    for i in 0..entry_count {
        let off = 16u64 + (i as u64) * 80;
        if vmo_read(initrd, off, entry.as_mut_ptr(), 80) != 0 {
            break;
        }
        let plen = entry[..64].iter().position(|b| *b == 0).unwrap_or(64);
        if plen == fat_path.len() && entry[..plen] == fat_path[..] {
            fat_off = u64::from_le_bytes(entry[64..72].try_into().unwrap());
            break;
        }
    }
    if fat_off == 0 {
        return 0;
    }

    // Read BPB.
    let mut bpb = [0u8; 512];
    if vmo_read(initrd, fat_off, bpb.as_mut_ptr(), 512) != 0 {
        return 0;
    }
    let bps = u16::from_le_bytes(bpb[0x0B..0x0D].try_into().unwrap()) as u64;
    let spc = bpb[0x0D] as u64;
    let rsvd = u16::from_le_bytes(bpb[0x0E..0x10].try_into().unwrap()) as u64;
    let nfat = bpb[0x10] as u64;
    let spf = u32::from_le_bytes(bpb[0x24..0x28].try_into().unwrap()) as u64;
    let data_start = rsvd + nfat * spf;
    let root_sec = data_start;
    let root_off = fat_off + root_sec * bps;

    // Walk root directory.
    let mut dir = [0u8; 512];
    if vmo_read(initrd, root_off, dir.as_mut_ptr(), 512) != 0 {
        return 0;
    }
    let mut pos = 0;
    let mut file_cluster = 0u32;
    let mut file_size = 0u32;
    while pos + 32 <= 512 && dir[pos] != 0x00 {
        let attr = dir[pos + 11];
        if attr != 0x0F && attr != 0x08 && dir[pos..pos + 11] == target[..] {
            let c_hi = u16::from_le_bytes(dir[pos + 20..pos + 22].try_into().unwrap()) as u32;
            let c_lo = u16::from_le_bytes(dir[pos + 26..pos + 28].try_into().unwrap()) as u32;
            file_cluster = (c_hi << 16) | c_lo;
            file_size = u32::from_le_bytes(dir[pos + 28..pos + 32].try_into().unwrap());
            break;
        }
        pos += 32;
    }
    if file_cluster == 0 || file_off >= file_size as u64 {
        return 0;
    }

    let max_len = (req_len as usize)
        .min(out.len())
        .min((file_size as u64 - file_off) as usize);

    // Seek to file_off within the cluster chain.
    let fat_abs = fat_off + rsvd * bps;
    let mut cluster = file_cluster;
    let mut skip = file_off;
    let bytes_per_cluster = (spc * bps) as u64;
    while skip >= bytes_per_cluster && cluster >= 2 && cluster < 0x0FFFFFF8 {
        skip -= bytes_per_cluster;
        let fat_entry_off = cluster as u64 * 4;
        let fat_sec = (fat_entry_off / bps) as usize;
        let fat_idx = (fat_entry_off % bps) as usize;
        let mut fat_buf = [0u8; 512];
        if vmo_read(
            initrd,
            fat_abs + fat_sec as u64 * bps,
            fat_buf.as_mut_ptr(),
            512,
        ) != 0
        {
            return 0;
        }
        cluster = u32::from_le_bytes(fat_buf[fat_idx..fat_idx + 4].try_into().unwrap());
    }

    // Read clusters starting from `cluster` at offset `skip`.
    let mut written = 0usize;
    let mut fat_buf = [0u8; 512];
    while cluster >= 2 && cluster < 0x0FFFFFF8 && written < max_len {
        let sec = data_start + (cluster as u64 - 2) * spc;
        let abs_off = fat_off + sec * bps + skip;
        let to_read = ((bps as u64 - skip) as usize).min(max_len - written);
        if vmo_read(initrd, abs_off, out[written..].as_mut_ptr(), to_read) != 0 {
            break;
        }
        written += to_read;
        skip = 0;

        let fat_entry_off = cluster as u64 * 4;
        let fat_sec = (fat_entry_off / bps) as usize;
        let fat_idx = (fat_entry_off % bps) as usize;
        if vmo_read(
            initrd,
            fat_abs + fat_sec as u64 * bps,
            fat_buf.as_mut_ptr(),
            512,
        ) != 0
        {
            break;
        }
        cluster = u32::from_le_bytes(fat_buf[fat_idx..fat_idx + 4].try_into().unwrap());
        if cluster >= 0x0FFFFFF8 {
            cluster = 0;
        }
    }
    written
}

/// P7-j simple path read: entire file from offset 0 (backward compatible).
fn serve_file_read(initrd: Handle, path: &[u8], out: &mut [u8; 512]) -> usize {
    serve_file_read_at(initrd, path, 0, out.len() as u64, out)
}

fn launch_lua_repl(initrd: Handle, _fb_out: Handle) {
    // 1. Mint the IPC channels using raw syscalls
    let (repl_stdin_h, _kb_stdout_h) = channel_create_pair();
    let (_fb_stdin_h, repl_stdout_h) = channel_create_pair();

    if repl_stdin_h == u64::MAX || repl_stdout_h == u64::MAX {
        debug_write(b"Failed to mint REPL channels\n".as_ptr(), 29);
        return;
    }

    let repl_stdin = Handle(repl_stdin_h as u32);
    let repl_stdout = Handle(repl_stdout_h as u32);

    debug_write(b"Lua REPL channels minted.\n".as_ptr(), 26);

    // 2. Next step (Pending Userland ELF Loader):
    // In the current Phase 10 skeleton, Sora is manually executing raw assembly
    // payloads (as seen in the `sora_main` demo). To fully launch the Piccolo binary,
    // we need to add a basic ELF parser here to read the `lua-repl` file from the
    // initrd, copy its LOAD segments into a new VMO, and map them using `vmar_map`
    // before calling `process_run`.
}
