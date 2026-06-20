#![no_std]
#![no_main]

extern crate alloc;

use core::sync::atomic::{AtomicU64, Ordering};
use kumo_abi::{
    BootInfo, Errno, Framebuffer, Handle, ProcessRunFlags, Rights, VmarFlags, AUTOEXEC_PATH,
    CAT_PATH, FAT32_IMG_PATH, LS_PATH, PERSONA_LINUX_HELLO_PATH, SVC_HEALTH_PATH, TTYD_PATH,
};
use kumo_fatfs::{FatVolume, SectorReader};
use kumo_rt::{
    address_space_create, channel_create, channel_create_pair, channel_read, channel_write,
    channel_write_with_handle, debug_write, handle_close, handle_duplicate, handle_koid,
    interrupt_create, port_bind, port_create, port_unbind, port_wait, process_create, process_run,
    process_wait, resource_create_child, thread_create, thread_start, vmar_map, vmo_create,
    vmo_read, vmo_write,
};
use kumoza::parse;
use persona_linux::{arm64 as linux_arm64, elf as linux_elf};
use persona_posix::{FdTable, TtyRpc, TtyRpcTransport, TtyStream, STDIN_FILENO, STDOUT_FILENO};
use svc_health::{Request as HealthRequest, Response as HealthResponse};
use ttyd::{Reply as TtyReply, Request as TtyRequest};

/// The BootInfo VMO handle the kernel hands Sora in **x8** — the 9th bootstrap value, beyond
/// the C-ABI argument registers x0–x7 (J159). x8 is caller-saved, so reading it inside
/// `sora_main` via inline asm is unsound: the compiler may clobber x8 before the read. It did
/// exactly that on the live X13s boot — reusing x8 as the `DebugWrite` syscall number (29) for
/// the greeting and reordering the `options(nomem)` read after it — so Sora saw handle 29 and
/// the BootInfo self-map returned `BadHandle` (`st=0xfffffffe`). Capture x8 in `_start`, the
/// one place it is still guaranteed live, before any other instruction can touch it.
static BOOTINFO_VMO_HANDLE: AtomicU64 = AtomicU64::new(0);

/// A reusable scratch VMO for handing programs their argv in `x1` (J-argv). Created once
/// and rewritten per `run`; `ARGV_VMO` is Sora's full-rights write handle and
/// `ARGV_VMO_RO` the read-only alias granted to children (least privilege — a program
/// reads its args, can't mutate the shared buffer). Runs are synchronous, so one buffer
/// is safe to reuse. Statics avoid threading the handles through the shell dispatcher.
static ARGV_VMO: AtomicU64 = AtomicU64::new(0);
static ARGV_VMO_RO: AtomicU64 = AtomicU64::new(0);

// Sora's entry. Mirrors `kumo_rt::entry!` (a bare `bl sora_main`) but first stashes x8 into
// `BOOTINFO_VMO_HANDLE`. x9 is scratch; sora_main is reached with x0–x7 (its C-ABI args)
// untouched. `_start` runs at EL0 with Sora's address space live, so the static (.bss, mapped
// RW) is writable here.
core::arch::global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    "_start:",
    "  adrp x9, {slot}",
    "  add  x9, x9, :lo12:{slot}",
    "  str  x8, [x9]",
    "  bl   sora_main",
    "1: b 1b",
    slot = sym BOOTINFO_VMO_HANDLE,
);

fn log(msg: &[u8]) {
    kumo_rt::debug_write(msg.as_ptr(), msg.len());
}

/// Log a `u64` as `0x…` hex on the live serial. `no_std` Sora has no formatter, so a
/// failing syscall's raw status is otherwise invisible; this surfaces the kernel `Errno`
/// behind a collapsed "fail" so a hardware boot localises the cause (mirrors the inline
/// child-handle dump already used below).
fn log_hex(mut v: u64) {
    debug_write(b"0x".as_ptr(), 2);
    let mut buf = [0u8; 16];
    let mut hi = 16;
    loop {
        hi -= 1;
        let d = (v & 0xF) as u8;
        buf[hi] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
        v >>= 4;
        if v == 0 {
            break;
        }
    }
    debug_write(buf[hi..].as_ptr(), 16 - hi);
}

/// Dump framebuffer geometry as `phys=… len=… w=… h=… stride=…`. Used by both drv-fb
/// spawn failure paths so a corrupt or rejected geometry is always fully visible.
fn log_fb_geometry(fb: &Framebuffer) {
    log(b"phys=");
    log_hex(fb.phys);
    log(b" len=");
    log_hex(fb.len);
    log(b" w=");
    log_hex(fb.width as u64);
    log(b" h=");
    log_hex(fb.height as u64);
    log(b" stride=");
    log_hex(fb.stride as u64);
    log(b"\n");
}

/// Locate a file in the initrd by path, returning its `(offset, len)`. Sora
/// holds the initrd as a VMO handle (not a mapped slice), so it walks the entry
/// table with `vmo_read` rather than `kumo_abi::initrd::find_file`.
fn initrd_find(initrd: Handle, want: &[u8]) -> Option<(u64, u64)> {
    let mut hdr = [0u8; 16];
    if vmo_read(initrd, 0, hdr.as_mut_ptr(), 16) != 0 || &hdr[..8] != b"KUMORD01" {
        return None;
    }
    let entry_count = u32::from_le_bytes(hdr[12..16].try_into().unwrap()) as u64;
    let mut name = [0u8; 64];
    let mut tail = [0u8; 16]; // offset(8) + len(8)
    for i in 0..entry_count {
        let base = 16 + i * 80;
        if vmo_read(initrd, base, name.as_mut_ptr(), 64) != 0
            || vmo_read(initrd, base + 64, tail.as_mut_ptr(), 16) != 0
        {
            return None;
        }
        let nlen = name.iter().position(|&b| b == 0).unwrap_or(64);
        if &name[..nlen] == want {
            let off = u64::from_le_bytes(tail[..8].try_into().unwrap());
            let len = u64::from_le_bytes(tail[8..16].try_into().unwrap());
            return Some((off, len));
        }
    }
    None
}

/// A [`SectorReader`] serving 512-byte sectors from a region of the initrd VMO
/// (the FAT32 image lives as a file inside the initrd). This is the interim
/// block source until `drv-blk` serves the same image over IPC.
struct InitrdSectors {
    vmo: Handle,
    base: u64,
    len: u64,
}

/// The kernel's `VmoRead` syscall copies at most this many bytes per call
/// (`usermode.rs`), so a 512-byte sector must be read in chunks.
const VMO_READ_MAX: usize = 256;

impl SectorReader for InitrdSectors {
    fn read_sector(&mut self, lba: u32, buf: &mut [u8; kumo_fatfs::SECTOR_SIZE]) -> bool {
        let off = lba as u64 * kumo_fatfs::SECTOR_SIZE as u64;
        if off + kumo_fatfs::SECTOR_SIZE as u64 > self.len {
            return false;
        }
        let mut done = 0usize;
        while done < kumo_fatfs::SECTOR_SIZE {
            let chunk = (kumo_fatfs::SECTOR_SIZE - done).min(VMO_READ_MAX);
            if vmo_read(
                self.vmo,
                self.base + off + done as u64,
                buf[done..].as_mut_ptr(),
                chunk,
            ) != 0
            {
                return false;
            }
            done += chunk;
        }
        true
    }
}

/// A [`SectorReader`] that reads 512-byte sectors **through `drv-blk` over IPC** rather than
/// touching the initrd VMO directly — the `fatfs`-server path (the trait's intended client).
/// `base` is the FS image's byte offset within `drv-blk`'s block space (the whole initrd, so
/// `LBA·512 = initrd offset`). The image is not 512-aligned inside the initrd, so a logical
/// sector generally straddles two `drv-blk` blocks; this splices them. `client` is the serve
/// channel's client end Sora already holds.
struct BlkSectors {
    client: Handle,
    base: u64,
}

impl BlkSectors {
    /// Read one whole `drv-blk` block at absolute `lba` into `out` (write → pump → read →
    /// `read_payload`, mirroring `request_reply`). Returns `false` on any failure.
    fn read_block(&mut self, lba: u64, out: &mut [u8; kumo_fatfs::SECTOR_SIZE]) -> bool {
        let req = drv_blk::Request::read(lba, 1).encode();
        if channel_write(self.client, req.as_ptr(), req.len()) != 0 {
            return false;
        }
        if !child_parked(process_wait()) {
            return false;
        }
        let mut reply = [0u8; 1 + kumo_fatfs::SECTOR_SIZE];
        let n = channel_read(self.client, reply.as_mut_ptr(), reply.len()) as usize;
        match drv_blk::read_payload(&reply[..n]) {
            Ok(data) if data.len() >= kumo_fatfs::SECTOR_SIZE => {
                out.copy_from_slice(&data[..kumo_fatfs::SECTOR_SIZE]);
                true
            }
            _ => false,
        }
    }
}

impl SectorReader for BlkSectors {
    fn read_sector(&mut self, lba: u32, buf: &mut [u8; kumo_fatfs::SECTOR_SIZE]) -> bool {
        const S: u64 = kumo_fatfs::SECTOR_SIZE as u64;
        let byte_off = self.base + lba as u64 * S;
        let first = byte_off / S;
        let skew = (byte_off % S) as usize;
        let mut tmp = [0u8; kumo_fatfs::SECTOR_SIZE];
        if !self.read_block(first, &mut tmp) {
            return false;
        }
        if skew == 0 {
            buf.copy_from_slice(&tmp);
            return true;
        }
        let head = kumo_fatfs::SECTOR_SIZE - skew;
        buf[..head].copy_from_slice(&tmp[skew..]);
        if !self.read_block(first + 1, &mut tmp) {
            return false;
        }
        buf[head..].copy_from_slice(&tmp[..skew]);
        true
    }
}

const QEMU_PL011_MMIO_BASE: u64 = 0x0900_0000;
const QEMU_PL011_MMIO_SIZE: u64 = 0x1000;
const QEMU_PL011_IRQ: u32 = 33; // SPI 1 = 33 on QEMU ARM virt
const TIMER_IRQ: u32 = 27; // EL1 physical timer PPI
const RUN_LEGACY_SVC_HEALTH_BOOT_SMOKES: bool = false;

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
    fb_va: u64,
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
    let res = Handle(resource_handle as u32);
    let net = Handle(net_handle as u32);
    let kbd = Handle(kbd_handle as u32);

    // J159: BootInfo VMO handle arrives in x8 (beyond the C-ABI first-8). `_start` captured it
    // into `BOOTINFO_VMO_HANDLE` at entry — reading x8 here would race the compiler's reuse of
    // it (see the static's doc-comment).
    let bootinfo_vmo: u64 = BOOTINFO_VMO_HANDLE.load(Ordering::Relaxed);

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
    // IRQ authority is gated by `res` (the root Resource covers every IRQ line).
    let timer_irq = interrupt_create(res, TIMER_IRQ);
    if timer_irq != u64::MAX {
        debug_write(b"timer irq ok\n".as_ptr(), 13);
    }

    // Spawn drv-serial — but ONLY on the serial-console path (no framebuffer).
    //
    // drv-serial is the QEMU PL011 input driver: it owns the UART at 0x0900_0000 /
    // IRQ 33 and forwards RX bytes to the console channel so the kernel's serial shell
    // has a keyboard. On the framebuffer console path (the X13s) there is no PL011 at
    // that address. The mint+map still SUCCEED (the root Resource spans all of phys, so
    // resource_create_child/ResourceMintMmio range-checks pass), but drv-serial's first
    // MMIO access — the RXIM unmask write at drv-serial/main.rs — stalls the interconnect
    // on the absent/unclocked peripheral and hard-hangs the core. That is the boot freeze
    // whose last visible line is "drv-serial starting" (printed just before that write).
    // The kernel takes its own serial-shell branch exactly when there is no framebuffer
    // (kernel/src/lib.rs: `if report.has_framebuffer { halt } else { serial shell }`), so
    // gate the PL011 driver on the same signal: `fb_va == 0` ⇔ serial path. On the X13s
    // there is no kernel keyboard anyway (the fb path halts), so skipping it loses nothing.
    if fb_va == 0 {
        let serial_res_h = resource_create_child(
            res,
            QEMU_PL011_MMIO_BASE,
            QEMU_PL011_MMIO_SIZE,
            QEMU_PL011_IRQ,
            1,
        );
        if serial_res_h == u64::MAX {
            log(b"drv-serial: resource fail\n");
        } else if run_elf(
            initrd,
            kumo_abi::DRV_SERIAL_PATH.as_bytes(),
            serial_res_h,
            console_handle,
            1, // async
            b"drv-serial",
        ) {
            log(b"drv-serial: run ok\n");
        } else {
            log(b"drv-serial: run fail\n");
        }
    } else {
        log(b"drv-serial: skipped (framebuffer console, no PL011)\n");
    }

    // J160/J161: spawn drv-blk — the ramdisk block driver (M6/P7).
    // Duplicate the initrd VMO with read-only rights, compute the total
    // initrd size from the file table, and pass both (size + handle) to
    // drv-blk over a bootstrap channel.
    let mut blk_serve: Option<Handle> = None; // serve-channel client end, kept for fatfs-over-blk
    {
        // READ to map the VMO; TRANSFER so channel_write may move the handle to
        // drv-blk (the engine requires Rights::TRANSFER on a transferred handle).
        // Without TRANSFER the write fails AccessDenied and drv-blk sees no VMO (J167).
        let blk_vmo_h = handle_duplicate(initrd, Rights::READ | Rights::TRANSFER);
        if blk_vmo_h != 0 && blk_vmo_h != u64::MAX {
            // Compute the total initrd data size from the header + entry table.
            let mut initrd_size: u64 = 0;
            let mut hdr = [0u8; 16];
            if vmo_read(initrd, 0, hdr.as_mut_ptr(), 16) == 0 {
                let entry_count = u32::from_le_bytes(hdr[12..16].try_into().unwrap()) as u64;
                let mut entry = [0u8; 80];
                for i in 0..entry_count {
                    let off = 16u64 + i * 80;
                    if vmo_read(initrd, off, entry.as_mut_ptr(), 80) != 0 {
                        break;
                    }
                    let file_off = u64::from_le_bytes(entry[64..72].try_into().unwrap());
                    let file_len = u64::from_le_bytes(entry[72..80].try_into().unwrap());
                    let end = file_off.saturating_add(file_len);
                    if end > initrd_size {
                        initrd_size = end;
                    }
                }
                // Add header + table overhead. The data region starts after
                // header(16) + table(entry_count * 80).
                initrd_size = initrd_size
                    .saturating_add(16)
                    .saturating_add(entry_count.saturating_mul(80));
            }
            let (server_chan, client_chan) = channel_create_pair();
            if server_chan != u64::MAX && client_chan != u64::MAX {
                // Send the size as 8 LE bytes alongside the VMO handle.
                let size_bytes = initrd_size.to_le_bytes();
                channel_write_with_handle(
                    Handle(server_chan as u32),
                    size_bytes.as_ptr(),
                    size_bytes.len(),
                    Handle(blk_vmo_h as u32),
                );
                if run_elf(
                    initrd,
                    kumo_abi::DRV_BLK_PATH.as_bytes(),
                    0,           // arg (x0) = unused
                    client_chan, // arg2 (x1) = bootstrap channel
                    1,           // async
                    b"drv-blk",
                ) {
                    log(b"drv-blk: run ok\n");
                    // M6/P7 round-trip: drv-blk now serves the initrd VMO as a block
                    // device over IPC. Sora holds the serve channel's client end
                    // (`server_chan`). Pump once so drv-blk consumes its bootstrap
                    // (VMO + size) and parks on the serve channel, then read LBA 0 — the
                    // initrd header — and verify a FULL 512-byte sector survives the
                    // client → drv-blk → client path (the J166 codec end to end; mirrors
                    // the svc-health `request_reply` pump pattern). A whole sector only fits
                    // now that the channel cap is `MAX_INLINE_BYTES`, not 256 (J173 → J174);
                    // checking `data.len() == BLOCK_SIZE` is the proof it is not truncated.
                    let blk_client = Handle(server_chan as u32);
                    blk_serve = Some(blk_client); // reuse for the fatfs-over-drv-blk mount
                    let _ = process_wait(); // let drv-blk (and drv-serial) reach their park
                    let req = drv_blk::Request::read(0, 1).encode();
                    let mut reply = [0u8; 1 + drv_blk::BLOCK_SIZE as usize];
                    let rt_ok = channel_write(blk_client, req.as_ptr(), req.len()) == 0
                        && child_parked(process_wait())
                        && {
                            let n =
                                channel_read(blk_client, reply.as_mut_ptr(), reply.len()) as usize;
                            matches!(
                                drv_blk::read_payload(&reply[..n]),
                                Ok(data)
                                    if data.len() == drv_blk::BLOCK_SIZE as usize
                                        && &data[..8] == b"KUMORD01"
                            )
                        };
                    if rt_ok {
                        log(b"blk-rt: ok\n");
                    } else {
                        log(b"blk-rt: fail\n");
                    }
                } else {
                    log(b"drv-blk: run fail\n");
                }
            }
        }
    }

    // Live proof that kumo-fatfs reads the real bin/fat32.img shipped in the
    // initrd: mount it, resolve HELLO.TXT, read its bytes, and log them. The
    // image is read straight from the initrd VMO (the interim block source until
    // drv-blk serves it over IPC). This exercises the whole kumo-fatfs read
    // engine against reality. Verified by the local UEFI/AAVMF smoke (xtask
    // qemu-smoke boots only the raw Stage-A stub, not the kernel/sora); see
    // JOURNAL/165 for the "fatfs: HELLO.TXT = hello!" serial capture.
    if let Some((base, len)) = initrd_find(initrd, FAT32_IMG_PATH.as_bytes()) {
        let mut disk = InitrdSectors {
            vmo: initrd,
            base,
            len,
        };
        match FatVolume::mount(&mut disk) {
            Ok(vol) => {
                match vol.find_in_root(&mut disk, b"HELLO   TXT") {
                    Some(entry) => {
                        let mut data = [0u8; 16];
                        let n = vol.read_file(&mut disk, &entry, &mut data);
                        log(b"fatfs: HELLO.TXT = ");
                        debug_write(data.as_ptr(), n);
                        log(b"\n");
                    }
                    None => log(b"fatfs: HELLO.TXT missing\n"),
                }
                // J179 consumer: resolve a file in a real SUBDIRECTORY by path
                // (kumo-fatfs find_in_dir + resolve_path, J177/J178). The image
                // now ships an ESP-shaped /EFI/BOOT/BOOTAA64.EFI subtree.
                match vol.resolve_path(&mut disk, b"/EFI/BOOT/BOOTAA64.EFI") {
                    Some(entry) => {
                        let mut data = [0u8; 16];
                        let n = vol.read_file(&mut disk, &entry, &mut data);
                        log(b"fatfs-path: /EFI/BOOT/BOOTAA64.EFI = ");
                        debug_write(data.as_ptr(), n);
                        log(b"\n");
                    }
                    None => log(b"fatfs-path: /EFI/BOOT/BOOTAA64.EFI missing\n"),
                }
            }
            Err(_) => log(b"fatfs: mount failed\n"),
        }
    } else {
        log(b"fatfs: image not found\n");
    }

    // M6/P7 fatfs-server gate: mount the SAME bin/fat32.img and read HELLO.TXT, but drive
    // kumo-fatfs through `drv-blk` over IPC (a `BlkSectors` SectorReader) instead of reading
    // the initrd VMO directly. Same engine, same image, sectors now served by the block
    // driver — the end-to-end storage path. (drv-blk serves the whole initrd by LBA, so the
    // image's byte offset is the SectorReader `base`.)
    if let Some(blk) = blk_serve {
        if let Some((fat_base, _len)) = initrd_find(initrd, FAT32_IMG_PATH.as_bytes()) {
            let mut disk = BlkSectors {
                client: blk,
                base: fat_base,
            };
            match FatVolume::mount(&mut disk) {
                Ok(vol) => match vol.find_in_root(&mut disk, b"HELLO   TXT") {
                    Some(entry) => {
                        let mut data = [0u8; 16];
                        let n = vol.read_file(&mut disk, &entry, &mut data);
                        log(b"fatfs-blk: HELLO.TXT = ");
                        debug_write(data.as_ptr(), n);
                        log(b"\n");
                    }
                    None => log(b"fatfs-blk: HELLO.TXT missing\n"),
                },
                Err(_) => log(b"fatfs-blk: mount failed\n"),
            }
        } else {
            log(b"fatfs-blk: image not found\n");
        }
    }

    // J159: spawn drv-fb when a framebuffer is present.
    // Map the BootInfo VMO (populated by the kernel), read framebuffer geometry,
    // create a narrowed child Resource for the framebuffer MMIO range, and hand
    // fb_res, the console channel, and the bootinfo VMO to drv-fb over a
    // bootstrap channel pair. drv-fb reads these and paints the text console.
    if fb_va != 0 && bootinfo_vmo != 0 {
        let bi_vmo = Handle(bootinfo_vmo as u32);
        let bi_va = 0x0000_0000_3000_0000;
        let bi_map_st = vmar_map(Handle(0), bi_vmo, 0, bi_va, 4096, (VmarFlags::READ).0);
        if bi_map_st == 0 {
            let bootinfo = unsafe { &*(bi_va as *const BootInfo) };
            if bootinfo.has_framebuffer() {
                let fb = bootinfo.framebuffer;
                // Refuse to mint an MMIO Resource over implausibly-shaped geometry: a corrupt
                // snapshot read must fail here as a clear diagnostic, not reach the kernel's
                // range check disguised as a rights problem.
                let fb_res_h = if fb.is_plausible() {
                    resource_create_child(res, fb.phys, fb.len, 0, 0)
                } else {
                    u64::MAX
                };
                if fb_res_h != u64::MAX {
                    let (server_chan, client_chan) = channel_create_pair();
                    if server_chan != u64::MAX && client_chan != u64::MAX {
                        let srv = Handle(server_chan as u32);
                        // Write the three handles drv-fb expects.
                        channel_write_with_handle(srv, b"F".as_ptr(), 1, Handle(fb_res_h as u32));
                        channel_write_with_handle(srv, b"C".as_ptr(), 1, console);
                        channel_write_with_handle(srv, b"B".as_ptr(), 1, bi_vmo);
                        if run_elf(
                            initrd,
                            kumo_abi::DRV_FB_PATH.as_bytes(),
                            0,           // arg (x0) = unused
                            client_chan, // arg2 (x1) = bootstrap channel
                            1,           // async
                            b"drv-fb",
                        ) {
                            log(b"drv-fb: run ok\n");
                        } else {
                            log(b"drv-fb: run fail\n");
                        }
                    } else {
                        log(b"drv-fb: channel fail\n");
                    }
                } else {
                    // Two failure modes, now separated by the plausibility gate above.
                    // Implausible geometry is a corrupt BootInfo snapshot read (the kernel
                    // cleans the snapshot to coherency as of J219; if this still fires the read
                    // is wrong further upstream). Plausible geometry the kernel still rejects is
                    // a real Resource-rights / range-check issue — and the Sora syscall path
                    // collapses the kernel errno to u64::MAX, so surface the geometry either way.
                    if fb.is_plausible() {
                        log(b"drv-fb: resource fail ");
                    } else {
                        log(b"drv-fb: implausible framebuffer geometry ");
                    }
                    log_fb_geometry(&fb);
                }
            }
        } else {
            // Static analysis says this self-map (in-bounds of Sora's root VMAR, READ right
            // present, physical-backed BootInfo VMO, identity-map switch in the kernel
            // VmarMap path) should succeed — yet it fails on X13s. Surface the real Errno so
            // the next boot localises it (NoMemory vs InvalidArgs vs BadHandle) instead of a
            // collapsed "fail".
            log(b"drv-fb: bootinfo map fail st=");
            log_hex(bi_map_st);
            log(b"\n");
        }
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
    if run_elf(
        initrd,
        PERSONA_LINUX_HELLO_PATH.as_bytes(),
        0,
        0,
        0,
        b"M10 elf",
    ) {
        debug_write(b"M10-c Check ok\n".as_ptr(), 15);
    } else {
        log(b"M10 elf: run fail\n");
    }

    // Narrow the initrd handle before handing it to user programs. `ls` only needs READ
    // authority; Sora keeps the full bootstrap handle for its own loader duties.
    let prog_initrd = handle_duplicate(initrd, Rights::READ | Rights::DUPLICATE);
    if prog_initrd == 0 || prog_initrd == u64::MAX {
        log(b"initrd read-only dup fail\n");
    }

    // Create the reusable argv scratch VMO + its read-only alias (see the statics). A
    // program launched with arguments gets the read-only alias in x1; failure here just
    // means programs run with no argv (degrade, don't abort the boot).
    let argv_vmo = vmo_create(256);
    if argv_vmo != u64::MAX {
        ARGV_VMO.store(argv_vmo, Ordering::Relaxed);
        let argv_ro = handle_duplicate(Handle(argv_vmo as u32), Rights::READ | Rights::DUPLICATE);
        if argv_ro != 0 && argv_ro != u64::MAX {
            ARGV_VMO_RO.store(argv_ro, Ordering::Relaxed);
        } else {
            log(b"argv read-only dup fail\n");
        }
    } else {
        log(b"argv vmo create fail\n");
    }

    // Live Sora-side HandleClose proof: close a temporary read-only duplicate, then
    // HandleKoid must reject that process-local number. The original initrd handle is
    // untouched and remains the source for program loading.
    let close_probe = handle_duplicate(initrd, Rights::READ);
    if close_probe != 0
        && close_probe != u64::MAX
        && handle_close(Handle(close_probe as u32)) == Errno::Ok.status()
        && handle_koid(Handle(close_probe as u32)) == u64::MAX
    {
        log(b"handle close: sora ok\n");
    } else {
        log(b"handle close: sora fail\n");
    }

    // Autoexec (input-less): run the boot script shipped at `etc/autoexec` in the initrd —
    // one shell command per line, `#` comments and blanks skipped (`kumoza::autoexec_lines`).
    // Each line is parsed and dispatched through the SAME `eval_command` as interactive input,
    // so `run <prog>` launches a program (the J181 synchronous path) and `echo …` prints — the
    // manifest speaks the shell's own syntax. This is the X13s shell-vertical stopgap: commands
    // you add to the manifest run at boot and paint to the framebuffer with NO keyboard. Manifest
    // is small; a fixed buffer avoids the heap.
    let mut manifest = [0u8; 256];
    match find_initrd_file(initrd, AUTOEXEC_PATH.as_bytes()) {
        Some((off, len)) => {
            let n = (len as usize).min(manifest.len());
            if vmo_read(initrd, off, manifest.as_mut_ptr(), n) == 0 {
                for line in kumoza::autoexec_lines(&manifest[..n]) {
                    if let Ok(ls) = core::str::from_utf8(line) {
                        if let Some(stmt) = parse(ls) {
                            kumoza::evaluate(&stmt, |cmd| {
                                eval_command(cmd, line, initrd, prog_initrd, root)
                            });
                        }
                    }
                }
                log(b"autoexec: done\n");
            } else {
                log(b"autoexec: read fail\n");
            }
        }
        None => log(b"autoexec: no manifest\n"),
    }

    if run_ttyd_smoke(initrd) {
        log(b"ttyd: serve ok\n");
    } else {
        log(b"ttyd: serve fail\n");
    }

    if run_persona_posix_tty_write_smoke(initrd) {
        log(b"persona-posix: tty write ok\n");
    } else {
        log(b"persona-posix: tty write fail\n");
    }

    if run_persona_posix_tty_read_smoke(initrd) {
        log(b"persona-posix: tty read ok\n");
    } else {
        log(b"persona-posix: tty read fail\n");
    }

    if RUN_LEGACY_SVC_HEALTH_BOOT_SMOKES {
        // Stage-C: spawn TWO real servers as independent resident processes, each with its own
        // request channel and port. Proves the kernel's per-thread wait queue routes each
        // client's traffic to exactly the server that owns its port — no cross-wake (Journal 134).
        if run_svc_health_pair_smoke(initrd) {
            log(b"svc-health: serve ok\n");
        } else {
            log(b"svc-health: serve fail\n");
        }

        // §5.6 supervised restart: a server is shut down and reaped, then Sora rebuilds it from
        // its recipe; the fresh instance serves with reset state — the first restart (Journal 136).
        if run_svc_health_restart_smoke(initrd) {
            log(b"svc-health: restart ok\n");
        } else {
            log(b"svc-health: restart fail\n");
        }

        // §5.6 crash containment: a child that faults must be terminated by the kernel alone, not
        // halt it. Spawn one at a bogus entry; the kernel contains the fault and Sora lives on.
        if run_crash_containment_smoke(initrd) {
            log(b"svc-health: crash contained\n");
        } else {
            log(b"svc-health: crash escaped\n");
        }

        // §5.6 crash restart: trigger an explicit crash on the server. The kernel contains it,
        // tears it down, and fires PEER_CLOSED to our port wait, waking us to respawn it.
        if run_crash_restart_smoke(initrd) {
            log(b"svc-health: crash-restart ok\n");
        } else {
            log(b"svc-health: crash-restart fail\n");
        }
    } else {
        log(b"svc-health: legacy boot smokes skipped\n");
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
        port_bind(port, console);
        port_bind(port, block);
        port_bind(port, net);
        port_bind(port, kbd);
        let cons_koid = Handle(console_koid as u32);
        let blk_koid = Handle(block_koid as u32);
        let net_k = Handle(net_koid as u32);
        let kbd_k = Handle(kbd_koid as u32);
        let mut serve_buf = [0u8; 256];
        let mut block_buf = [0u8; 512];

        // Launch Piccolo Lua REPL directly before falling back to the basic shell loop.
        launch_lua_repl(initrd, kbd, console);
        let ttyd_recipe = sora::ServerRecipe {
            name: "ttyd",
            image_path: TTYD_PATH,
            restart: sora::RestartPolicy::OnFailure,
        };
        let mut interactive_ttyd = spawn_ttyd_session(initrd, ttyd_recipe);
        let mut interactive_ttyd_koid = interactive_ttyd.and_then(|service| {
            arm_supervised_process(port, service).inspect(|_| {
                log(b"ttyd: supervision armed\n");
            })
        });
        if interactive_ttyd_koid.is_none() {
            log(b"ttyd: supervision arm fail\n");
        }

        // One bounded restart probe: ask the initial instance to stop, then let the
        // permanent port observe its process TERMINATED signal. The replacement is not
        // sent this frame, so the proof cannot loop indefinitely.
        if let Some(service) = interactive_ttyd {
            let mut request = [0u8; ttyd::REQUEST_BUF_BYTES];
            let sent = TtyRequest::shutdown()
                .encode_into(&mut request)
                .map(|n| channel_write(service.instance.client, request.as_ptr(), n) == 0)
                .unwrap_or(false);
            if sent {
                log(b"ttyd: restart probe sent\n");
            } else {
                log(b"ttyd: restart probe fail\n");
            }
        }

        // Bound how many times the interactive service may be respawned before Sora gives
        // up on it (DESIGN/002 §5 give-up ladder floor): a crash-looping instance must not
        // be restarted forever. The boot probe consumes exactly one unit, well within cap.
        const TTYD_MAX_RESTARTS: u32 = 3;
        let mut ttyd_restart_budget = sora::RestartBudget::new(TTYD_MAX_RESTARTS);

        loop {
            let source = Handle(port_wait(port) as u32);
            if Some(source) == interactive_ttyd_koid {
                let Some(dead) = interactive_ttyd.take() else {
                    interactive_ttyd_koid = None;
                    continue;
                };
                interactive_ttyd_koid = None;
                let recipe = dead.recipe;
                let restart = recipe.restart.should_restart(true);
                // Drop this instance's port watch before closing its handles. Teardown only
                // reclaims bindings keyed on the dead process's *own* handles, never Sora's
                // watch on it, so without this the permanent port would retain one inert
                // binding per restart. Idempotent in the kernel; a real failure is loud.
                if port_unbind(port, dead.instance.process) != 0 {
                    log(b"ttyd: dead watch unbind fail\n");
                }
                if !close_supervised_service(dead) {
                    log(b"ttyd: dead instance cleanup fail\n");
                }
                // Consume one unit of the give-up budget only when policy actually wants a
                // restart — short-circuit means an intentional or Never termination spends
                // nothing, and an exhausted budget gives the service up instead of looping.
                let allowed = restart && ttyd_restart_budget.try_consume();
                if allowed {
                    log(b"ttyd: restart required\n");
                    if let Some(replacement) = spawn_ttyd_session(initrd, recipe) {
                        let mut reply = [0u8; ttyd::REPLY_BUF_BYTES];
                        let serves = ttyd_request_reply(
                            replacement.instance.client,
                            TtyRequest::clear(),
                            &mut reply,
                        )
                        .and_then(|n| TtyReply::parse(&reply[..n]))
                        .map(|reply| reply.status == ttyd::TTY_OK)
                        .unwrap_or(false);
                        if serves {
                            if let Some(koid) = arm_supervised_process(port, replacement) {
                                if koid != source {
                                    interactive_ttyd = Some(replacement);
                                    interactive_ttyd_koid = Some(koid);
                                    log(b"ttyd: restart ok\n");
                                } else {
                                    let _ = close_supervised_service(replacement);
                                    log(b"ttyd: restart identity fail\n");
                                }
                            } else {
                                let _ = close_supervised_service(replacement);
                                log(b"ttyd: restart rearm fail\n");
                            }
                        } else {
                            let _ = close_supervised_service(replacement);
                            log(b"ttyd: restart serve fail\n");
                        }
                    } else {
                        log(b"ttyd: restart spawn fail\n");
                    }
                } else if restart {
                    log(b"ttyd: restart budget exhausted\n");
                } else {
                    log(b"ttyd: terminated\n");
                }
                continue;
            }
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
            // keyboard handler — input only (keystrokes → ttyd → parsed command line)
            if source == kbd_k {
                let n = channel_read(kbd, serve_buf.as_mut_ptr(), 256) as usize;
                if n > 0 && interactive_ttyd.is_some() {
                    let tty = interactive_ttyd.unwrap().instance;
                    let mut tty_reply = [0u8; ttyd::REPLY_BUF_BYTES];
                    for i in 0..n {
                        dispatch_ttyd_key(
                            tty.client,
                            serve_buf[i],
                            &mut tty_reply,
                            initrd,
                            prog_initrd,
                            root,
                        );
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

fn arm_supervised_process(port: Handle, service: sora::SupervisedService<'_>) -> Option<Handle> {
    let koid = handle_koid(service.instance.process);
    if koid != u64::MAX && port_bind(port, service.instance.process) == 0 {
        Some(Handle(koid as u32))
    } else {
        None
    }
}

fn close_supervised_service(service: sora::SupervisedService<'_>) -> bool {
    sora::close_handles(
        &[
            Some(service.instance.process),
            Some(service.instance.client),
        ],
        handle_close,
    )
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

/// Dispatch one parsed shell command. Shared by the interactive line editor and the
/// boot autoexec (J183), so both speak the same syntax: `echo`/`help`/`ls`/`run` are
/// handled by this dispatcher; any other non-empty command is forwarded verbatim (`raw`, the
/// original line bytes) to the kernel shell over the `root` channel. `prog_initrd` is a
/// read-only initrd handle granted to `ls` (capability-passing, J186). Always returns
/// `true` — a builtin failure logs but never aborts the surrounding `kumoza::evaluate`.
fn eval_command(
    cmd: &kumoza::Command,
    raw: &[u8],
    initrd: Handle,
    prog_initrd: u64,
    root: Handle,
) -> bool {
    if kumoza::write_builtin_output(cmd, |bytes| {
        debug_write(bytes.as_ptr(), bytes.len());
    }) {
        return true;
    } else if cmd.name == "cat" {
        // cat combines both program-startup slots: the read-only initrd capability in
        // x0 and its read-only argv VMO in x1. Only this explicit shell launcher grants
        // the filesystem image; plain `run cat ...` remains authority-free.
        if cmd.args.len() != 1 {
            debug_write(b"usage: cat <path>\n".as_ptr(), 18);
        } else if prog_initrd == 0 || prog_initrd == u64::MAX {
            debug_write(b"cat: no initrd handle\n".as_ptr(), 22);
        } else {
            let argv_handle = build_command_argv_handle(cmd);
            if argv_handle == 0 {
                debug_write(b"cat: argv unavailable\n".as_ptr(), 22);
            } else {
                run_elf(
                    initrd,
                    CAT_PATH.as_bytes(),
                    prog_initrd,
                    argv_handle,
                    0,
                    b"cat",
                );
            }
        }
    } else if cmd.name == "ls" {
        // ls: list the initrd's entries from an ordinary program that receives only a
        // read-only initrd handle — the first capability-using KUMO userland command.
        if prog_initrd == 0 || prog_initrd == u64::MAX {
            debug_write(b"ls: no initrd handle\n".as_ptr(), 20);
        } else {
            run_elf(initrd, LS_PATH.as_bytes(), prog_initrd, 0, 0, b"ls");
        }
    } else if cmd.name == "run" {
        // run <program> [args...]: spawn bin/<program> and run it to completion, handing it
        // its arguments via a read-only argv VMO (x1). `cmd.args` is already [program, args…],
        // i.e. the program's argv with argv[0] = its name. The interactive twin of the boot
        // autoexec — both drive the same `run_program`.
        if cmd.args.is_empty() {
            debug_write(b"usage: run <program> [args...]\n".as_ptr(), 31);
        } else {
            run_program(initrd, &cmd.args);
        }
    } else if !cmd.name.is_empty() {
        channel_write(root, raw.as_ptr(), raw.len());
    }
    true
}

/// Pack `argv` into the shared scratch VMO and return the read-only handle to grant the
/// child in `x1`, or `0` for no argv (empty, the VMO unavailable, or argv too large for
/// the 256-byte buffer). `argv[0]` is the program name (execve convention). The child
/// `vmo_read`s the handle and walks it with `kumo_abi::unpack_argv`.
fn build_argv_handle(argv: &[alloc::string::String]) -> u64 {
    if argv.is_empty() {
        return 0;
    }
    // Collect argv as byte-slices in a bounded, heap-free array, then pack.
    const MAX_ARGS: usize = 16;
    let mut slots: [&[u8]; MAX_ARGS] = [b""; MAX_ARGS];
    let mut count = 0;
    for arg in argv.iter().take(MAX_ARGS) {
        slots[count] = arg.as_bytes();
        count += 1;
    }
    write_argv_handle(&slots[..count])
}

/// Build argv from a parsed command (`argv[0]` = command name), used by builtins
/// that launch a native program while granting it an explicit capability.
fn build_command_argv_handle(cmd: &kumoza::Command) -> u64 {
    const MAX_ARGS: usize = 16;
    let mut slots: [&[u8]; MAX_ARGS] = [b""; MAX_ARGS];
    slots[0] = cmd.name.as_bytes();
    let mut count = 1;
    for arg in cmd.args.iter().take(MAX_ARGS - 1) {
        slots[count] = arg.as_bytes();
        count += 1;
    }
    write_argv_handle(&slots[..count])
}

fn write_argv_handle(argv: &[&[u8]]) -> u64 {
    let vmo = ARGV_VMO.load(Ordering::Relaxed);
    let vmo_ro = ARGV_VMO_RO.load(Ordering::Relaxed);
    if vmo == 0 || vmo == u64::MAX || vmo_ro == 0 || vmo_ro == u64::MAX {
        return 0;
    }
    let mut buf = [0u8; 256];
    let written = match kumo_abi::pack_argv(argv, &mut buf) {
        Some(n) => n,
        None => return 0, // argv exceeds the 256-byte VMO; run with none rather than fail
    };
    // VmoWrite is capped at 256/call; `written <= 256`, so one write suffices. The VMO is
    // reused across runs — a partial write leaves stale tail bytes, but `unpack_argv`
    // bounds its walk by the fresh `argc`, so they are never read.
    if vmo_write(Handle(vmo as u32), 0, buf.as_ptr(), written) != 0 {
        return 0;
    }
    vmo_ro
}

/// Run an initrd program: spawn `bin/<argv[0]>` to completion (synchronous, `flags=0` —
/// the J181 exec-vertical path), handing it `argv` in a read-only VMO (`x1`). `x0` (the
/// capability slot) is `0` — a plain `run` grants no authority; the `ls` builtin is the
/// capability-granting launcher. The `bin/` prefix is built into a fixed buffer (no
/// heap). Returns whether the program ran (false also logs the reason).
fn run_program(initrd: Handle, argv: &[alloc::string::String]) -> bool {
    let name = match argv.first() {
        Some(n) => n.as_bytes(),
        None => return false,
    };
    const PREFIX: &[u8] = b"bin/";
    let mut path = [0u8; 64];
    if PREFIX.len() + name.len() > path.len() {
        debug_write(b"run: name too long\n".as_ptr(), 19);
        return false;
    }
    path[..PREFIX.len()].copy_from_slice(PREFIX);
    path[PREFIX.len()..PREFIX.len() + name.len()].copy_from_slice(name);
    let full = &path[..PREFIX.len() + name.len()];
    let argv_handle = build_argv_handle(argv);
    run_elf(initrd, full, 0, argv_handle, 0, full)
}

fn run_elf(initrd: Handle, path: &[u8], arg: u64, arg2: u64, flags: u64, name: &[u8]) -> bool {
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

    let (elf_off, elf_len) = match find_initrd_file(initrd, path) {
        Some(file) => file,
        None => {
            log(name);
            log(b": missing\n");
            return false;
        }
    };

    let mut header = [0u8; linux_elf::ELF_HEADER_LEN];
    if vmo_read(initrd, elf_off, header.as_mut_ptr(), header.len()) != 0 {
        log(name);
        log(b": hdr read fail\n");
        return false;
    }
    let elf = match linux_elf::parse_header(&header) {
        Ok(elf) => elf,
        Err(_) => {
            log(name);
            log(b": bad hdr\n");
            return false;
        }
    };

    let ph_table_len = (elf.phnum as u64).saturating_mul(elf.phentsize as u64);
    if elf.phoff.saturating_add(ph_table_len) > elf_len || elf.phnum as usize > MAX_SEGMENTS {
        log(name);
        log(b": ph range fail\n");
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
            log(name);
            log(b": ph read fail\n");
            return false;
        }
        let ph = match linux_elf::parse_program_header(&ph_buf) {
            Ok(ph) => ph,
            Err(_) => {
                log(name);
                log(b": bad ph\n");
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
            log(name);
            log(b": segment range fail\n");
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
        log(name);
        log(b": no load\n");
        return false;
    }
    let vmo_len = match align_up(vmo_len) {
        Some(len) if len > 0 => len,
        _ => {
            log(name);
            log(b": size fail\n");
            return false;
        }
    };

    let child_vmo_h = vmo_create(vmo_len);
    if child_vmo_h == u64::MAX {
        log(name);
        log(b": vmo fail\n");
        return false;
    }
    let child_vmo = Handle(child_vmo_h as u32);

    // From the first successful allocation onward, all exits break through the one
    // cleanup point below. Slot 0 owns the loader VMO; slot 1 begins empty and takes
    // ownership of the process handle if ProcessCreate succeeds.
    let mut loader_handles = [Some(child_vmo), None];
    let loaded = 'load: {
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
                    log(name);
                    log(b": copy fail\n");
                    break 'load false;
                }
                copied += n as u64;
            }
        }

        let child_as_h = process_create(0, 0x2000_0000);
        if child_as_h == u64::MAX {
            log(name);
            log(b": process fail\n");
            break 'load false;
        }
        let child_as = Handle(child_as_h as u32);
        loader_handles[1] = Some(child_as);

        for segment in segments.iter().take(segment_count) {
            let page_delta = segment.virt_addr & (PAGE_SIZE - 1);
            let virt = align_down(segment.virt_addr);
            let vmo_offset = align_down(segment.file_offset);
            let len = match align_up(page_delta.saturating_add(segment.mem_size)) {
                Some(len) => len,
                None => {
                    log(name);
                    log(b": map len fail\n");
                    break 'load false;
                }
            };
            if vmar_map(child_as, child_vmo, vmo_offset, virt, len, segment.flags) != 0 {
                log(name);
                log(b": map fail\n");
                break 'load false;
            }
        }

        if address_space_create(child_as, 0x10010000, 0x4000) == u64::MAX {
            log(name);
            log(b": as fail\n");
            break 'load false;
        }

        process_run(child_as, elf.entry, 0x1000FFF0, arg, arg2, flags) == 0
    };

    // `VmarMap`/`AddressSpaceCreate` copied everything an admitted child needs
    // into its process record. Sora does not supervise this path by handle, so
    // both success and every post-allocation failure surrender all acquired handles.
    let cleanup_ok = sora::close_handles(&loader_handles, handle_close);
    if !cleanup_ok {
        log(name);
        log(b": handle cleanup fail\n");
    }
    loaded && cleanup_ok
}

/// Stage-C milestone (Journal 134): two `svc-health` servers run as **independent**
/// resident processes at the same time. Each binds its own request channel to its own port
/// and parks in `PortWait`; the kernel's per-thread wait queue routes each client write to
/// exactly the server that owns that port, so the two never cross-wake. Independence is
/// proved by each server's own `served` counter reaching 2 from its own Ping + Status —
/// neither sees the other's traffic. (This subsumes the former single-resident smoke.)
/// A server's **construction recipe** (DESIGN/002 §5.6): everything Sora needs to (re)build
/// the server from scratch. For `svc-health` — a *stateless* recovery-class server — that is
/// just where to find its ELF; a fresh instance gets a fresh address space and channel, and
/// its advisory counters reset. Respawning after a crash/stop is simply calling
/// [`spawn_from_recipe`] again.
struct SvcRecipe {
    initrd: Handle,
    path: &'static str,
}

/// Spawn one fresh server instance from its recipe: reload the ELF into a new address space,
/// grant it a fresh request channel, and start it as a resident (async) process. The server
/// endpoint is moved into the child; Sora retains only the process handle for supervision
/// and the client endpoint for RPC. The caller pumps (`process_wait`) to let the server reach
/// `PortWait`.
fn spawn_from_recipe(recipe: &SvcRecipe) -> Option<sora::SupervisedServer> {
    let (child_as, entry, stack_top, server_chan, client_chan) =
        run_initrd_elf_with_channel(recipe.initrd, recipe.path.as_bytes())?;
    let flags = (ProcessRunFlags::ASYNC | ProcessRunFlags::TRANSFER_ARG).bits();
    if process_run(child_as, entry, stack_top, server_chan.0 as u64, 0, flags) != 0 {
        if !sora::close_handles(
            &[Some(child_as), Some(server_chan), Some(client_chan)],
            handle_close,
        ) {
            log(b"spawn: failure cleanup fail\n");
        }
        return None;
    }
    if handle_koid(server_chan) != u64::MAX {
        log(b"spawn: bootstrap transfer fail\n");
        return None;
    }
    log(b"spawn: bootstrap transfer ok\n");
    Some(sora::SupervisedServer {
        process: child_as,
        client: client_chan,
    })
}

fn run_svc_health_pair_smoke(initrd: Handle) -> bool {
    let recipe = SvcRecipe {
        initrd,
        path: SVC_HEALTH_PATH,
    };
    // Spawn both as resident (async) servers; one pump drains both to their own PortWait.
    let cli1 = match spawn_from_recipe(&recipe) {
        Some(c) => c,
        None => {
            log(b"svc-pair: run1 fail\n");
            return false;
        }
    };
    let cli2 = match spawn_from_recipe(&recipe) {
        Some(c) => c,
        None => {
            log(b"svc-pair: run2 fail\n");
            return false;
        }
    };
    if !child_parked(process_wait()) {
        log(b"svc-pair: park fail\n");
        return false;
    }

    // Ping each resident on its own channel: each must reply Pong while the other stays
    // asleep (no cross-wake).
    if request_reply(cli1, HealthRequest::Ping) != Some(HealthResponse::Pong) {
        log(b"svc-pair: ping1 fail\n");
        return false;
    }
    if request_reply(cli2, HealthRequest::Ping) != Some(HealthResponse::Pong) {
        log(b"svc-pair: ping2 fail\n");
        return false;
    }

    // Status each: served == 2 (its own Ping + Status) proves separate per-process state —
    // neither counted the other's Ping.
    let want = Some(HealthResponse::Status {
        uptime_ticks: 0,
        served: 2,
    });
    if request_reply(cli1, HealthRequest::Status) != want {
        log(b"svc-pair: status1 fail\n");
        return false;
    }
    if request_reply(cli2, HealthRequest::Status) != want {
        log(b"svc-pair: status2 fail\n");
        return false;
    }

    // Termination + detection (Journal 135): shut down resident #1. Its serve loop exits
    // and it process-exits; the harness reaps it. Resident #2 must keep serving — proving
    // an *independent lifecycle*, not just independent wakeups — so ProcessWait still
    // reports a resident remaining.
    if !child_parked(shutdown_resident(cli1)) {
        log(b"svc-pair: shutdown1 fail\n");
        return false;
    }
    if request_reply(cli2, HealthRequest::Ping) != Some(HealthResponse::Pong) {
        log(b"svc-pair: survivor fail\n");
        return false;
    }
    // Shut down the survivor too; now ProcessWait must report no resident left.
    if !all_residents_gone(shutdown_resident(cli2)) {
        log(b"svc-pair: shutdown2 fail\n");
        return false;
    }
    true
}

/// First **supervised restart** (Journal 136, DESIGN/002): a server dies and Sora rebuilds
/// it from its recipe. Instance A serves until its counter reaches 2, then is shut down and
/// reaped. Sora respawns from the *same* recipe; the fresh instance B must serve, and its
/// first `Status` reports `served: 1` — not 3 — proving it is a **new process with reset
/// state** (the stateless recovery class), not a revived A.
fn run_svc_health_restart_smoke(initrd: Handle) -> bool {
    let recipe = SvcRecipe {
        initrd,
        path: SVC_HEALTH_PATH,
    };

    // Instance A: serve Ping + Status (counter climbs to 2), then "die" via Shutdown.
    let a = match spawn_from_recipe(&recipe) {
        Some(c) => c,
        None => {
            log(b"svc-restart: a spawn fail\n");
            return false;
        }
    };
    if !child_parked(process_wait()) {
        log(b"svc-restart: a park fail\n");
        return false;
    }
    if request_reply(a, HealthRequest::Ping) != Some(HealthResponse::Pong) {
        log(b"svc-restart: a ping fail\n");
        return false;
    }
    if request_reply(a, HealthRequest::Status)
        != Some(HealthResponse::Status {
            uptime_ticks: 0,
            served: 2,
        })
    {
        log(b"svc-restart: a status fail\n");
        return false;
    }
    if !all_residents_gone(shutdown_resident(a)) {
        log(b"svc-restart: a shutdown fail\n");
        return false;
    }

    // Supervised restart from the same recipe. Instance B is a fresh process: its first
    // Status reports served:1 (not 3), proving reset state — the restart actually worked.
    let b = match spawn_from_recipe(&recipe) {
        Some(c) => c,
        None => {
            log(b"svc-restart: respawn fail\n");
            return false;
        }
    };
    if !child_parked(process_wait()) {
        log(b"svc-restart: b park fail\n");
        return false;
    }
    if request_reply(b, HealthRequest::Status)
        != Some(HealthResponse::Status {
            uptime_ticks: 0,
            served: 1,
        })
    {
        log(b"svc-restart: b state fail\n");
        return false;
    }
    if !all_residents_gone(shutdown_resident(b)) {
        log(b"svc-restart: b shutdown fail\n");
        return false;
    }
    true
}

/// §5.6 crash containment (Journal 137): a child that *faults* must be terminated by the
/// kernel — never halt it. Spawn a child at a bogus (unmapped) entry so it instruction-aborts
/// on its first fetch. If the kernel contains the fault and reaps the child, Sora keeps
/// running and `ProcessWait` reports no resident remaining; before this slice the boot would
/// have stopped at the Tower ("Ziwei seizes the wheel") and never reached this line.
fn run_crash_containment_smoke(initrd: Handle) -> bool {
    // Build a valid child address space + stack + channel, then run it at an unmapped entry.
    let (child_as, _entry, stack_top, server_chan, _client) =
        match run_initrd_elf_with_channel(initrd, SVC_HEALTH_PATH.as_bytes()) {
            Some(t) => t,
            None => return false,
        };
    const BOGUS_ENTRY: u64 = 0xDEAD_0000; // unmapped: instruction abort on first fetch
    if process_run(child_as, BOGUS_ENTRY, stack_top, server_chan.0 as u64, 0, 1) != 0 {
        log(b"svc-crash: run fail\n");
        return false;
    }
    // The child faults immediately; the kernel contains it and reaps it. Reaching here at all
    // proves the kernel survived; `Ok` proves the crashed child was reaped (no resident left).
    all_residents_gone(process_wait())
}

/// Start a server from a recipe, bind its channel to a port, request a crash, and verify
/// we wake up from the port wait with PEER_CLOSED, enabling true supervised restart.
fn run_crash_restart_smoke(initrd: Handle) -> bool {
    // 1. Spawn from recipe
    let recipe = SvcRecipe {
        initrd,
        path: SVC_HEALTH_PATH,
    };
    let Some(mut server) = spawn_from_recipe(&recipe) else {
        log(b"svc-crash-restart: spawn fail\n");
        return false;
    };
    if request_reply(server, HealthRequest::Ping) != Some(HealthResponse::Pong) {
        log(b"svc-crash-restart: ping 1 fail\n");
        return false;
    }

    // 2. Bind the client channel to a port
    let port = Handle(port_create() as u32);
    if port.0 as u64 == u64::MAX {
        return false;
    }
    if port_bind(port, server.client) != 0 {
        return false;
    }

    // 3. Send a Crash request (no reply expected)
    let bytes = HealthRequest::Crash.encode();
    if channel_write(server.client, bytes.as_ptr(), bytes.len()) != 0 {
        return false;
    }

    // 4. Wait on the port. We expect to be woken by the crash closing the channel.
    let packet = port_wait(port);
    if packet == 0 {
        log(b"svc-crash-restart: port wait fail\n");
        return false;
    }
    // The source should be the channel koid
    let source_koid = packet >> 32;
    let signals = (packet & 0xFFFFFFFF) as u32;
    if signals != kumo_abi::Signals::PEER_CLOSED.bits() {
        log(b"svc-crash-restart: missing PEER_CLOSED signal\n");
        return false;
    }

    // 5. Respawn using the recipe
    let Some(respawned) = spawn_from_recipe(&recipe) else {
        log(b"svc-crash-restart: respawn fail\n");
        return false;
    };
    server = respawned;

    // 6. Verify it's a fresh instance
    let want_status = Some(HealthResponse::Status {
        uptime_ticks: 0,
        served: 1, // just this status request
    });
    if request_reply(server, HealthRequest::Status) != want_status {
        log(b"svc-crash-restart: fresh status fail\n");
        return false;
    }

    // Shutdown the respawned instance so the smoke test cleans up
    shutdown_resident(server) == Errno::Ok.status() as u64
}

/// Send one request to a resident server on `client`, pump the scheduler so that server
/// wakes, serves, and re-parks, then read and decode its reply.
fn request_reply(client: sora::SupervisedServer, request: HealthRequest) -> Option<HealthResponse> {
    let bytes = request.encode();
    if channel_write(client.client, bytes.as_ptr(), bytes.len()) != 0 {
        return None;
    }
    if !child_parked(process_wait()) {
        return None;
    }
    let mut reply = [0u8; 32];
    let n = channel_read(client.client, reply.as_mut_ptr(), reply.len()) as usize;
    HealthResponse::decode(&reply[..n])
}

/// Send `Shutdown` to a resident on `client` and pump: the server's serve loop exits and it
/// process-exits, so the harness reaps it. Returns the `ProcessWait` status — `ShouldWait`
/// while another resident remains, `Ok` once none do.
fn shutdown_resident(client: sora::SupervisedServer) -> u64 {
    let bytes = HealthRequest::Shutdown.encode();
    let _ = channel_write(client.client, bytes.as_ptr(), bytes.len());
    process_wait()
}

fn spawn_ttyd_session(
    initrd: Handle,
    recipe: sora::ServerRecipe<'static>,
) -> Option<sora::SupervisedService<'static>> {
    let spawn_recipe = SvcRecipe {
        initrd,
        path: recipe.image_path,
    };
    let instance = spawn_from_recipe(&spawn_recipe)?;
    if !child_parked(process_wait()) {
        log(b"ttyd: interactive park fail\n");
        return None;
    }
    log(b"ttyd: interactive ok\n");
    Some(sora::SupervisedService { recipe, instance })
}

fn dispatch_ttyd_key(
    tty: Handle,
    byte: u8,
    reply: &mut [u8],
    initrd: Handle,
    prog_initrd: u64,
    root: Handle,
) -> bool {
    let Some(n) = ttyd_request_reply(tty, TtyRequest::input(byte), reply) else {
        log(b"ttyd: interactive input fail\n");
        return false;
    };
    let Some(parsed) = TtyReply::parse(&reply[..n]) else {
        log(b"ttyd: interactive parse fail\n");
        return false;
    };
    if parsed.status != ttyd::TTY_OK && parsed.status != ttyd::TTY_OVERFLOW {
        log(b"ttyd: interactive status fail\n");
        return false;
    }
    if !parsed.echo.is_empty() {
        debug_write(parsed.echo.as_ptr(), parsed.echo.len());
    }
    let Some(line) = parsed.line else {
        return true;
    };
    if let Ok(ls) = core::str::from_utf8(line) {
        if let Some(stmt) = parse(ls) {
            kumoza::evaluate(&stmt, |cmd| {
                eval_command(cmd, line, initrd, prog_initrd, root)
            });
        }
    }
    true
}

/// First live P8 `ttyd` proof: spawn a private `ttyd` server, send `h`, `i`, Enter,
/// and require the submitted line `hi` to come back over the request channel. This keeps
/// Sora's existing interactive path untouched while proving the real server binary/loop.
fn run_ttyd_smoke(initrd: Handle) -> bool {
    let recipe = SvcRecipe {
        initrd,
        path: TTYD_PATH,
    };
    let client = match spawn_from_recipe(&recipe) {
        Some(c) => c,
        None => {
            log(b"ttyd: spawn fail\n");
            return false;
        }
    };
    if handle_koid(client.process) == u64::MAX {
        log(b"ttyd: process handle fail\n");
        return false;
    }
    log(b"ttyd: process handle ok\n");
    if !child_parked(process_wait()) {
        log(b"ttyd: park fail\n");
        return false;
    }

    let mut reply = [0u8; ttyd::REPLY_BUF_BYTES];
    let Some(n) = ttyd_request_reply(client.client, TtyRequest::input(b'h'), &mut reply) else {
        log(b"ttyd: h fail\n");
        return false;
    };
    let Some(parsed) = TtyReply::parse(&reply[..n]) else {
        log(b"ttyd: h parse fail\n");
        return false;
    };
    if parsed.status != ttyd::TTY_OK || parsed.echo != b"h" || parsed.line.is_some() {
        log(b"ttyd: h reply fail\n");
        return false;
    }

    let Some(n) = ttyd_request_reply(client.client, TtyRequest::input(b'i'), &mut reply) else {
        log(b"ttyd: i fail\n");
        return false;
    };
    let Some(parsed) = TtyReply::parse(&reply[..n]) else {
        log(b"ttyd: i parse fail\n");
        return false;
    };
    if parsed.status != ttyd::TTY_OK || parsed.echo != b"i" || parsed.line.is_some() {
        log(b"ttyd: i reply fail\n");
        return false;
    }

    let Some(n) = ttyd_request_reply(client.client, TtyRequest::input(b'\n'), &mut reply) else {
        log(b"ttyd: enter fail\n");
        return false;
    };
    let Some(parsed) = TtyReply::parse(&reply[..n]) else {
        log(b"ttyd: enter parse fail\n");
        return false;
    };
    if parsed.status != ttyd::TTY_OK || parsed.echo != b"\r\n" || parsed.line != Some(&b"hi"[..]) {
        log(b"ttyd: line fail\n");
        return false;
    }

    let mut req = [0u8; ttyd::REQUEST_BUF_BYTES];
    let Some(req_len) = TtyRequest::shutdown().encode_into(&mut req) else {
        return false;
    };
    if channel_write(client.client, req.as_ptr(), req_len) != 0 {
        return false;
    }
    let status = process_wait();
    child_parked(status) || all_residents_gone(status)
}

struct ChannelTtyTransport {
    output: [u8; 16],
    len: usize,
}

impl ChannelTtyTransport {
    const fn new() -> ChannelTtyTransport {
        ChannelTtyTransport {
            output: [0; 16],
            len: 0,
        }
    }

    fn output(&self) -> &[u8] {
        &self.output[..self.len]
    }
}

impl TtyRpcTransport for ChannelTtyTransport {
    fn call(
        &mut self,
        stream: TtyStream,
        request: &[u8],
        reply: &mut [u8],
    ) -> persona_posix::PosixResult<usize> {
        let client = Handle(stream.handle);
        if channel_write(client, request.as_ptr(), request.len()) != 0 {
            return Err(persona_posix::PosixErrno::NoDevice);
        }
        if !child_parked(process_wait()) {
            return Err(persona_posix::PosixErrno::NoDevice);
        }
        let n = channel_read(client, reply.as_mut_ptr(), reply.len()) as usize;
        if let Some(parsed) = TtyReply::parse(&reply[..n]) {
            let room = self.output.len().saturating_sub(self.len);
            let take = parsed.echo.len().min(room);
            self.output[self.len..self.len + take].copy_from_slice(&parsed.echo[..take]);
            self.len += take;
        }
        Ok(n)
    }
}

fn run_persona_posix_tty_write_smoke(initrd: Handle) -> bool {
    let recipe = SvcRecipe {
        initrd,
        path: TTYD_PATH,
    };
    let client = match spawn_from_recipe(&recipe) {
        Some(c) => c,
        None => {
            log(b"persona-posix: tty spawn fail\n");
            return false;
        }
    };
    if !child_parked(process_wait()) {
        log(b"persona-posix: tty park fail\n");
        return false;
    }

    let mut table = FdTable::with_stdio(TtyStream {
        handle: client.client.0,
    });
    let mut tty = TtyRpc::new(ChannelTtyTransport::new());
    if table.write(STDOUT_FILENO, b"ok\n", &mut tty) != Ok(3) {
        log(b"persona-posix: tty write call fail\n");
        return false;
    }
    if tty.transport().output() != b"ok\n" {
        log(b"persona-posix: tty output fail\n");
        return false;
    }

    let mut req = [0u8; ttyd::REQUEST_BUF_BYTES];
    let Some(req_len) = TtyRequest::shutdown().encode_into(&mut req) else {
        return false;
    };
    if channel_write(client.client, req.as_ptr(), req_len) != 0 {
        return false;
    }
    let status = process_wait();
    child_parked(status) || all_residents_gone(status)
}

fn run_persona_posix_tty_read_smoke(initrd: Handle) -> bool {
    let recipe = SvcRecipe {
        initrd,
        path: TTYD_PATH,
    };
    let client = match spawn_from_recipe(&recipe) {
        Some(c) => c,
        None => {
            log(b"persona-posix: tty read spawn fail\n");
            return false;
        }
    };
    if !child_parked(process_wait()) {
        log(b"persona-posix: tty read park fail\n");
        return false;
    }

    let mut reply = [0u8; ttyd::REPLY_BUF_BYTES];
    for &byte in b"ok\n" {
        if ttyd_request_reply(client.client, TtyRequest::input(byte), &mut reply).is_none() {
            log(b"persona-posix: tty read feed fail\n");
            return false;
        }
    }

    let mut table = FdTable::with_stdio(TtyStream {
        handle: client.client.0,
    });
    let mut tty = TtyRpc::new(ChannelTtyTransport::new());
    let mut out = [0u8; 8];
    if table.read(STDIN_FILENO, &mut out, &mut tty) != Ok(2) || &out[..2] != b"ok" {
        log(b"persona-posix: tty read call fail\n");
        return false;
    }
    if table.read(STDIN_FILENO, &mut out, &mut tty) != Ok(0) {
        log(b"persona-posix: tty read drain fail\n");
        return false;
    }

    let mut req = [0u8; ttyd::REQUEST_BUF_BYTES];
    let Some(req_len) = TtyRequest::shutdown().encode_into(&mut req) else {
        return false;
    };
    if channel_write(client.client, req.as_ptr(), req_len) != 0 {
        return false;
    }
    let status = process_wait();
    child_parked(status) || all_residents_gone(status)
}

fn ttyd_request_reply(client: Handle, request: TtyRequest, reply: &mut [u8]) -> Option<usize> {
    let mut req = [0u8; ttyd::REQUEST_BUF_BYTES];
    let req_len = request.encode_into(&mut req)?;
    if channel_write(client, req.as_ptr(), req_len) != 0 {
        return None;
    }
    if !child_parked(process_wait()) {
        return None;
    }
    let n = channel_read(client, reply.as_mut_ptr(), reply.len()) as usize;
    Some(n)
}

fn child_parked(status: u64) -> bool {
    status == Errno::ShouldWait.status() as u32 as u64
}

fn all_residents_gone(status: u64) -> bool {
    status == Errno::Ok.status() as u32 as u64
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

    // The loader VMO is always temporary. The process and channel handles are
    // tracked as they are acquired, then preserved only when the full recipe is
    // returned to the supervisor. Every post-allocation failure keeps nothing.
    let mut loader_handles = [Some(child_vmo), None, None, None];
    let loaded = 'load: {
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
                    break 'load None;
                }
                copied += n as u64;
            }
        }

        let child_as_h = process_create(0, 0x2000_0000);
        if child_as_h == u64::MAX {
            log(b"svc-health: process fail\n");
            break 'load None;
        }
        let child_as = Handle(child_as_h as u32);
        loader_handles[1] = Some(child_as);

        for segment in segments.iter().take(segment_count) {
            let page_delta = segment.virt_addr & (PAGE_SIZE - 1);
            let virt = align_down(segment.virt_addr);
            let vmo_offset = align_down(segment.file_offset);
            let len = match align_up(page_delta.saturating_add(segment.mem_size)) {
                Some(len) => len,
                None => {
                    log(b"svc-health: map len fail\n");
                    break 'load None;
                }
            };
            if vmar_map(child_as, child_vmo, vmo_offset, virt, len, segment.flags) != 0 {
                log(b"svc-health: map fail\n");
                break 'load None;
            }
        }

        if address_space_create(child_as, STACK_TOP, STACK_SIZE) == u64::MAX {
            log(b"svc-health: as fail\n");
            break 'load None;
        }

        let (client_h, server_h) = channel_create_pair();
        if client_h != u64::MAX {
            loader_handles[2] = Some(Handle(client_h as u32));
        }
        if server_h != u64::MAX {
            loader_handles[3] = Some(Handle(server_h as u32));
        }
        if client_h == u64::MAX || server_h == u64::MAX {
            log(b"svc-health: channel fail\n");
            break 'load None;
        }

        break 'load Some((
            child_as,
            elf.entry,
            STACK_TOP - 0x10,
            Handle(server_h as u32),
            Handle(client_h as u32),
        ));
    };

    let cleanup_ok = match loaded {
        Some((child_as, _, _, server, client)) => {
            sora::close_handles_except(&loader_handles, &[child_as, server, client], handle_close)
        }
        None => sora::close_handles(&loader_handles, handle_close),
    };
    if !cleanup_ok {
        log(b"svc-health: loader cleanup fail\n");
        return None;
    }
    loaded
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

fn launch_lua_repl(initrd: Handle, kbd: Handle, console: Handle) {
    debug_write(b"Launching Lua REPL...\n".as_ptr(), 22);

    // 2. Launch the ELF binary from the initrd, passing kbd as stdin and console as stdout
    if run_elf(
        initrd,
        b"bin/lua-repl",
        kbd.0 as u64,
        console.0 as u64,
        0, // synchronous run
        b"lua-repl",
    ) {
        debug_write(b"Lua REPL exited normally.\n".as_ptr(), 26);
    } else {
        debug_write(b"Lua REPL failed to run.\n".as_ptr(), 24);
    }
}
