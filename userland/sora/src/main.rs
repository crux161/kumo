#![no_std]
#![no_main]

extern crate alloc;

use core::panic::PanicInfo;

use kumo_abi::Handle;
use kumo_rt::{
    channel_read, channel_write, debug_write, process_create, process_exit, process_run,
    thread_create, thread_start, vmar_map, vmo_read,
};
use kumoza::parse;

mod heap;

#[global_allocator]
static ALLOC: heap::BumpAlloc = heap::BumpAlloc;

// Tiny asm trampoline: `_start` (the ELF entry point) just calls `sora_main`.
// Bootstrap registers (x0-x3) are passed through per the aarch64 calling convention.
core::arch::global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    "_start:",
    "  bl  sora_main",
    "1: b 1b",
);

/// Bootstrap args (arrive in x0-x4, aarch64 calling convention):
///   x0: root-channel handle
///   x1: framebuffer virtual address (0 if no FB)
///   x2: console channel handle
///   x3: initrd VMO handle
///   x4: block-server channel handle (P7-g)
#[no_mangle]
extern "C" fn sora_main(
    root_handle: u64,
    _fb_va: u64,
    console_handle: u64,
    initrd_vmo: u64,
    block_handle: u64,
) -> ! {
    let root = Handle(root_handle as u32);
    let console = Handle(console_handle as u32);
    let initrd = Handle(initrd_vmo as u32);
    let block = Handle(block_handle as u32);

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
        debug_write(b"child process h".as_ptr(), 16);
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
            debug_write(b" thread=fail\n".as_ptr(), 12);
        }
        // P8-l: run the child process synchronously. Sora blocks; the child runs its
        // payload (`child_payload`) which does DebugWrite + ProcessExit; then Sora
        // resumes. The child shares Sora's TTBR0 (same address space for the scaffold).
        extern "C" {
            fn child_payload() -> !;
        }
        // Use the bottom of Sora's mapped stack region (0x101F_0000..0x1020_0000).
        // Sora is in kernel mode during ProcessRun, so its EL0 stack is frozen.
        let child_sp = 0x0000_0000_101F_8000u64;
        let run_ok = process_run(
            child,
            child_payload as *const () as usize as u64,
            child_sp,
        );
        debug_write(b" run=".as_ptr(), 5);
        if run_ok == 0 {
            debug_write(b"ok\n".as_ptr(), 3);
        } else {
            debug_write(b"fail\n".as_ptr(), 5);
        }
    }

    // P6-d/e + P7-g: the serve loop, forever, multiplexing two channels.
    //   * console: echo klog! output via DebugWrite; single-byte messages are
    //     keystrokes — echo + line-buffer (P8-b scaffold under DESIGN/006 §b).
    //   * block:   serve raw-offset or path-based reads from the initrd.
    let mut serve_buf = [0u8; 256];
    let mut block_buf = [0u8; 512];
    let mut line_buf = [0u8; 256];
    let mut line_pos = 0usize;
    loop {
        loop {
            let n = channel_read(console, serve_buf.as_mut_ptr(), 256) as usize;
            if n == 0 {
                break;
            }
            // P8-b: single-byte console messages are keystrokes forwarded by the
            // kernel. Echo them and buffer for line editing; on Enter, send the
            // completed line to the kernel via the root channel.
            if n == 1 {
                let byte = serve_buf[0];
                if byte == b'\r' || byte == b'\n' {
                    debug_write(b"\r\n".as_ptr(), 2);
                    // P8-f: use kumoza's parser + evaluator for dispatch.
                    if let Ok(line_str) = core::str::from_utf8(&line_buf[..line_pos]) {
                        if let Some(stmt) = parse(line_str) {
                            let cmd_line = &line_buf[..line_pos];
                            kumoza::evaluate(&stmt, |cmd| {
                                if cmd.name == "echo" {
                                    for (i, arg) in cmd.args.iter().enumerate() {
                                        if i > 0 {
                                            debug_write(b" ".as_ptr(), 1);
                                        }
                                        debug_write(arg.as_ptr(), arg.len());
                                    }
                                    debug_write(b"\n".as_ptr(), 1);
                                } else if cmd.name == "help" {
                                    let msg = b"KUMO Sora userspace shell (scaffold)\n\
                                        builtins: echo, help\n\
                                        other commands run via kernel shell\n";
                                    debug_write(msg.as_ptr(), msg.len());
                                } else if !cmd.name.is_empty() {
                                    channel_write(root, cmd_line.as_ptr(), cmd_line.len());
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
                        debug_write(serve_buf.as_ptr(), 1);
                    }
                }
            } else {
                debug_write(serve_buf.as_ptr(), n);
            }
        }

        loop {
            let n = channel_read(block, serve_buf.as_mut_ptr(), 256) as usize;
            if n == 0 {
                break;
            }
            if n == 16 {
                // Raw-offset request (backward compatible): offset u64 LE + len u64 LE.
                let offset = u64::from_le_bytes(serve_buf[..8].try_into().unwrap());
                let len = u64::from_le_bytes(serve_buf[8..16].try_into().unwrap());
                let len = (len as usize).min(block_buf.len());
                if len > 0 && vmo_read(initrd, offset, block_buf.as_mut_ptr(), len) == 0 {
                    channel_write(block, block_buf.as_ptr(), len);
                } else {
                    channel_write(block, b"".as_ptr(), 0);
                }
            } else {
                // P7-j/k: path-based read. Discriminator byte:
                //   0x01 = structured: [0x01][file_offset:u64 LE][len:u64 LE][path…]
                //   anything else = simple path read (backward compatible).
                let served = if serve_buf[0] == 0x01 && n >= 17 {
                    let file_off = u64::from_le_bytes(serve_buf[1..9].try_into().unwrap());
                    let req_len = u64::from_le_bytes(serve_buf[9..17].try_into().unwrap());
                    serve_file_read_at(initrd, &serve_buf[17..n], file_off, req_len, &mut block_buf)
                } else {
                    serve_file_read(initrd, &serve_buf[..n], &mut block_buf)
                };
                channel_write(block, block_buf.as_ptr(), served);
            }
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

/// P8-l: child process payload. Runs in the child's context (shared TTBR0 with
/// Sora for the scaffold). Calls DebugWrite and ProcessExit.
#[no_mangle]
extern "C" fn child_payload() -> ! {
    debug_write(b"child hello\n".as_ptr(), 12);
    process_exit(0);
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    process_exit(1);
}
