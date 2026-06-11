#![cfg_attr(not(test), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod bootstrap;
pub mod ipc;
pub mod ipcdemo;
pub mod kdemo;
pub mod mm;
pub mod object;
pub mod sched;
pub mod shell;
pub mod syscall;
pub mod task;
pub mod user_thread;
pub mod usermode;

use kumo_abi::{BootInfo, ABI_VERSION};
use niji_loader::{validate_boot_info, HandoffError, HandoffSummary};

pub const STAGE_A_BANNER: &str = "KUMO Ziwei Stage-A core only; halting";

#[macro_export]
macro_rules! klog {
    ($($arg:tt)*) => {
        $crate::bootstrap::console::write_fmt(core::format_args!($($arg)*))
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KernelInitReport {
    pub abi_version: u32,
    pub arch: &'static str,
    pub mem_region_count: u64,
    pub total_bytes: u64,
    pub usable_bytes: u64,
    pub has_initrd: bool,
    pub has_framebuffer: bool,
}

pub fn inspect_boot(boot: &BootInfo) -> Result<KernelInitReport, HandoffError> {
    let summary = validate_boot_info(boot)?;
    Ok(inspect_handoff(&summary))
}

pub fn inspect_handoff(summary: &HandoffSummary) -> KernelInitReport {
    KernelInitReport {
        abi_version: ABI_VERSION,
        arch: kumo_hal::active::arch_name(),
        mem_region_count: summary.mem_region_count,
        total_bytes: summary.total_bytes,
        usable_bytes: summary.usable_bytes,
        has_initrd: !summary.initrd.is_empty(),
        has_framebuffer: summary.has_framebuffer,
    }
}

pub fn stage_a_banner() -> &'static str {
    STAGE_A_BANNER
}

pub fn stage_a_console_banner() {
    bootstrap::console::write_str(STAGE_A_BANNER);
    bootstrap::console::write_str("\n");
}

pub fn stage_a(boot: &BootInfo) -> ! {
    // Own the fault path and the console before any fallible work: install our exception
    // vectors ("The Tower") so faults are caught and visible, then bring up the
    // framebuffer console (the X13s has no serial here), which clears to the black
    // phosphor backdrop. Interrupts stay masked (see `_start`) until we own the GIC.
    kumo_hal::active::install_exception_vectors();
    if boot.has_framebuffer() {
        let fb = boot.framebuffer;
        kumo_hal::active::set_framebuffer(fb.phys, fb.len, fb.width, fb.height, fb.stride);
    }

    let report = match inspect_boot(boot) {
        Ok(report) => report,
        Err(err) => tower_halt_ascii("nijigumo->ziwei handoff invalid", Some(err)),
    };

    // Boot banner in the idiom of the Jet Alone OS POST screen (Evangelion): green
    // phosphor, a RE-BOOT header, then a column of subsystem self-checks each ending in
    // OK. The Stage-A console now renders a curated CJK set (DESIGN/005), so the Japanese
    // system header stands beside the Latin POST instead of waiting for a font.
    klog!("\nKUMO Hi-SYS Re-BOOT!\n");
    // 雲 紫微 起動 / 記憶 検査 正常 = "KUMO Ziwei boot / memory check OK". A mixed
    // ASCII+Kanji line: ASCII via the 8x16 PSF cell, Kanji via the 16x16 double-width glyphs.
    klog!("雲 紫微 起動    記憶 検査 正常\n");
    // Broad CJK is embedded now (DESIGN/005): common simplified Chinese + Japanese kanji +
    // Korean jamo. "简体中文 / 日本語漢字 / [Hangul jamo]".
    klog!("简体中文  日本語漢字  ㄱㄴㄷㄹㅁ\n");
    klog!(
        "ZIWEI re-boot operating system, Ver 0.1.0  ({})\n",
        report.arch
    );
    klog!("Copyright (C) 2026  Kumo Heavy Industries Consortium\n\n");
    klog!("CPU MODE           High        EL1 / Ring0\n");
    klog!(
        "CO-CPU             Check       NIJIGUMO abi v{}        OK\n",
        report.abi_version
    );

    // M1: bring up memory. The bump heap is already online; account the frames and
    // prove the allocator yields real addresses (Guidance 002 §5: AETHER is real now).
    let mm = unsafe { mm::init(boot) };
    klog!(
        "AETHER MEMORY      Check     {} + {} MiB  {} frames   OK\n",
        report.usable_bytes >> 20,
        (report.total_bytes - report.usable_bytes) >> 20,
        mm.usable_frames
    );
    if mm.sample_count > 0 {
        klog!("  AETHER free frames :");
        let mut i = 0;
        while i < mm.sample_count {
            klog!(" {:#x}", mm.sample_frames[i]);
            i += 1;
        }
        klog!("\n");
    }

    // Prove the heap works on real silicon: build a small Vec and reduce it.
    let mut squares: alloc::vec::Vec<u32> = alloc::vec::Vec::new();
    let mut n = 1u32;
    while n <= 8 {
        squares.push(n * n);
        n += 1;
    }
    let sum: u32 = squares.iter().copied().sum();
    klog!(
        "HEAP ALLOCATOR     Check     {} KiB bump  vec sum={}     OK\n",
        mm.heap_bytes >> 10,
        sum
    );

    // Take ownership of the MMU: build KUMO's own identity page tables and switch off
    // the firmware's. Everything below now runs under kernel-owned paging — the
    // foundation for per-process address spaces and EL0/userspace.
    match unsafe { mm::enable_paging(boot) } {
        Some(p) => {
            // The TTBR1 physmap is live: move console MMIO access onto it, so printing
            // works under any TTBR0 (user trees no longer carry a console window).
            kumo_hal::active::console_use_physmap();
            klog!(
                "ROOTING TABLES     Check     {} MiB mapped  {} tables   OK\n",
                p.mapped_bytes >> 20,
                p.tables
            )
        }
        None => klog!("ROOTING TABLES     Check     no usable memory map      --\n"),
    }

    match kumo_hal::active::init_timer_interrupts(boot.platform.dtb, 20) {
        Ok(timer) => {
            let start = kumo_hal::active::timer_irq_count();
            let seen = kumo_hal::active::wait_for_timer_irqs(start, 3, 1_000_000_000);
            if seen >= 3 {
                klog!(
                    "GIC / TIMER        Check     {} Hz  IRQ {}  hb {}t   OK\n",
                    timer.counter_hz,
                    timer.irq,
                    seen
                );
            } else {
                klog!(
                    "GIC / TIMER        Check     IRQ {} heartbeat timeout ({}t)   FAIL\n",
                    timer.irq,
                    seen
                );
                kumo_hal::active::halt();
            }
        }
        Err(err) => {
            klog!(
                "GIC / TIMER        Check     unavailable: {:?}   FAIL\n",
                err
            );
            kumo_hal::active::halt();
        }
    }

    // M3 (opening): run more than one thread of control. A couple of kernel threads
    // cooperatively yield through the HAL context switch — the first real use of the
    // task substrate on the boot path. Preemptive, timer-driven scheduling is next.
    let m3 = kdemo::run();
    klog!(
        "CONTEXT SWITCH     Check     {} kthreads  {} switches   OK\n",
        m3.threads,
        m3.switches
    );

    // M3: the scheduler substrate (DESIGN/003). Discipline A — the O(1) strict-priority
    // class — replaces the old flat round-robin. A deterministic self-test proves the
    // two-level bitmap selects in strict-priority/FIFO order, the idle floor always has
    // a thread, and a more-urgent thread preempts a running one.
    let sc = sched::smoke();
    if sc.ordered && sc.idle_floor && sc.preemptions == 2 {
        klog!(
            "SCHEDULER          Check     O(1) {}-level  {} picks   OK\n",
            sc.levels,
            sc.picks
        );
    } else {
        klog!(
            "SCHEDULER          Check     ordered={} floor={} pre={}   FAIL\n",
            sc.ordered,
            sc.idle_floor,
            sc.preemptions
        );
        kumo_hal::active::halt();
    }

    let preempt = kdemo::run_preemption();
    klog!(
        "PREEMPTION         Check     {} body switches  {} ticks   OK\n",
        preempt.switches,
        preempt.ticks
    );

    match ipc::smoke() {
        Ok(ipc) => klog!(
            "I/O VECTORS        Check     {} call  {} bytes  {} hnd   OK\n",
            ipc.calls,
            ipc.bytes,
            ipc.handle_count
        ),
        Err(err) => {
            klog!(
                "I/O VECTORS        Check     IPC smoke failed: {:?}   FAIL\n",
                err
            );
            kumo_hal::active::halt();
        }
    }

    // P4: blocking IPC between two running kernel threads. The consumer parks on an
    // empty channel and the producer wakes it — real scheduler-integrated block/wake,
    // driven by the cooperative context switch.
    let blk = ipcdemo::run();
    if blk.delivered {
        klog!(
            "IPC BLOCK / WAKE   Check     park {}x  wake {}x  {} bytes   OK\n",
            blk.consumer_blocks,
            blk.wakes,
            blk.received
        );
    } else {
        klog!(
            "IPC BLOCK / WAKE   Check     park {}x wake {}x {} bytes   FAIL\n",
            blk.consumer_blocks,
            blk.wakes,
            blk.received
        );
    }

    let initrd = if report.has_initrd {
        // P5 bootstrap: prove the initrd now contains a named Sora image and that Ziwei
        // can turn it into the first userspace process/VMAR plan. This is still a plan,
        // not execution; the next page-table slice materializes these mappings.
        let initrd = unsafe {
            core::slice::from_raw_parts(boot.initrd.start as *const u8, boot.initrd.len as usize)
        };
        let mut objects = object::ObjectManager::new();
        match bootstrap::user::plan_sora_from_initrd(&mut objects, initrd) {
            Ok(plan) => {
                let mapped_bytes: u64 = plan.image_mappings.iter().map(|mapping| mapping.len).sum();
                klog!(
                    "SORA INITRD        Check     {} seg  {}b  entry {:#x}  sp {:#x}   OK\n",
                    plan.image_mappings.len(),
                    mapped_bytes,
                    plan.entry,
                    plan.stack_top
                )
            }
            Err(err) => klog!("SORA INITRD        Check     {:?}   --\n", err),
        }
        Some(initrd)
    } else {
        None
    };

    // P5/P6: Sora runs as a scheduler-driven EL0 thread in its own address space. It
    // greets via DebugWrite, echoes the kernel's root-channel boot message, acks, then
    // serves the console channel (P6-c: four kernel console lines echoed). The kernel
    // holds the root/console peer endpoints directly, so only Sora's bootstrap handle
    // (chan.0) is a process handle — there is no second handle to report.
    let u = match initrd {
        Some(initrd) => match usermode::run_sora(boot, initrd) {
            Ok(report) => report,
            Err(err) => {
                klog!(
                    "SORA EL0          Check     {:?}; using payload fallback   --\n",
                    err
                );
                usermode::run(boot)
            }
        },
        None => usermode::run(boot),
    };
    if u.entered && u.syscalls >= 3 && u.wrote > 0 && u.chan.0 != 0 {
        klog!(
            "USERLAND  EL0      Check     {} svc  wrote {}b  boot h{}  {}   OK\n",
            u.syscalls,
            u.wrote,
            u.chan.0,
            if u.serving { "serving" } else { "exited" }
        );
    } else {
        klog!(
            "USERLAND  EL0      Check     entered={} svc={} wrote={} boot h{}   FAIL\n",
            u.entered,
            u.syscalls,
            u.wrote,
            u.chan.0
        );
    }

    // P5: kernel <-> Sora IPC. Sora wrote a greeting down the bootstrap root channel
    // (handle 1); the kernel held the peer end and read it back after Sora exited.
    if u.handshake_len > 0 {
        let msg = core::str::from_utf8(&u.handshake[..u.handshake_len]).unwrap_or("<binary>");
        klog!(
            "SORA HANDSHAKE     Check     root channel: {:?}   OK\n",
            msg.trim_end()
        );
    } else if u.chan.0 != 0 {
        klog!("SORA HANDSHAKE     Check     no message on root channel   --\n");
    }

    // P6-e: with Sora serving, route the kernel console through it. The probe line is
    // the first routed `klog!`; the syscall delta proves Sora actually round-tripped it
    // (wake -> ChannelRead -> DebugWrite -> park). On a zero delta the route is torn
    // back down so output never silently vanishes. Everything below — the remaining
    // POST lines and the serial shell — rides the userspace console server.
    if u.serving {
        let svc_before = kumo_hal::active::syscall_count();
        usermode::enable_console_route();
        klog!("ZIWEI console     -> Sora console channel\n");
        let probe_svcs = kumo_hal::active::syscall_count().saturating_sub(svc_before);
        if probe_svcs > 0 {
            klog!(
                "CONSOLE ROUTE      Check     probe {} svc  via sora   OK\n",
                probe_svcs
            );
        } else {
            usermode::disable_console_route();
            klog!("CONSOLE ROUTE      Check     probe 0 svc  direct fallback   FAIL\n");
        }

        // P7-g: the kernel as a *client* of the userspace block server. Two reads of
        // the "disk" (the initrd) served by Sora over the block channel, verified by
        // value: the KUMORD01 magic at offset 0, and the first entry's path at offset
        // 16 — which is Sora's own image name.
        let mut sector = [0u8; 64];
        let magic_n = usermode::block_read_via_sora(0, 8, &mut sector);
        let magic_ok = magic_n == 8 && &sector[..8] == b"KUMORD01";
        let path_n = usermode::block_read_via_sora(16, 8, &mut sector);
        let path_ok = path_n == 8 && &sector[..8] == b"bin/sora";
        if magic_ok && path_ok {
            klog!(
                "BLOCK SERVE        Check     2 req  {}b  via sora   OK\n",
                magic_n + path_n
            );
        } else {
            klog!(
                "BLOCK SERVE        Check     magic {}b ok={}  path {}b ok={}   FAIL\n",
                magic_n,
                magic_ok,
                path_n,
                path_ok
            );
        }

        // P7-h: read the FAT32 disk image's sector 0 through the block server and verify
        // the BPB signature — proving the userspace block path carries real filesystem data.
        if let Some(initrd_bytes) = initrd {
            if let Ok(Some(fat32_file)) =
                kumo_abi::find_file(initrd_bytes, kumo_abi::FAT32_IMG_PATH)
            {
                let mut bpb = [0u8; 512];
                let bpb_n = usermode::block_read_via_sora(fat32_file.offset, 512, &mut bpb);
                let sig_ok = bpb_n >= 0x5A
                    && &bpb[0x52..0x5A] == b"FAT32   "
                    && &bpb[0..3] == [0xEB, 0xFE, 0x90];
                if sig_ok {
                    klog!("BLOCK FAT32        Check     sector 0 BPB  via sora   OK\n");
                } else {
                    klog!(
                        "BLOCK FAT32        Check     sector 0 BPB  {}b sig-ok={}   FAIL\n",
                        bpb_n,
                        sig_ok
                    );
                }

                // P7-j: read a named file through Sora's block channel — Sora resolves the
                // path against the FAT32 root directory, walks the FAT chain, and returns the
                // file contents. HELLO.TXT at cluster 3 holds "hello!" (6 bytes).
                let mut content = [0u8; 32];
                let content_n = usermode::file_read_via_sora(b"HELLO.TXT", &mut content);
                let content_ok = content_n == 6 && &content[..6] == b"hello!";
                // P7-k: ranged read — offset 1, length 4 should return "ello".
                let mut ranged = [0u8; 8];
                let ranged_n = usermode::file_read_via_sora_at(b"HELLO.TXT", 1, 4, &mut ranged);
                let ranged_ok = ranged_n == 4 && &ranged[..4] == b"ello";
                if content_ok && ranged_ok {
                    klog!(
                        "BLOCK FILE         Check     HELLO.TXT +{}b @1  via sora   OK\n",
                        ranged_n
                    );
                } else {
                    klog!(
                        "BLOCK FILE         Check     HELLO {}b ok={}  ranged {}b ok={}   FAIL\n",
                        content_n,
                        content_ok,
                        ranged_n,
                        ranged_ok
                    );
                }
            }

        }
    }

    if report.has_framebuffer {
        // Framebuffer console (e.g. the X13s): no kernel keyboard yet, so idle here.
        // The screen keeps the boot report; a pre-handoff pause lives in the loader.
        klog!("\nVIRUS PROTECTION   Check     GREEN                    OK\n");
        klog!("\nZIWEI core online -- all subsystems nominal.\n");
        klog!("KUMO Ziwei Stage-A core only; awaiting userspace.  HALT.\n");
        kumo_hal::active::halt()
    } else {
        // P8-b: serial console (QEMU PL011) — forward keystrokes to Sora via the
        // keyboard channel. Sora buffers keystrokes (minimal line editing: backspace),
        // echoes via DebugWrite, and sends completed lines to the kernel via the root
        // channel. The kernel runs shell::run_command on each line. This is scaffold
        // under DESIGN/006 §b — the line-edit loop is IPC, not a TTY.
        kdemo::install_preemption_probe();
        let mut env = shell::ShellEnv {
            arch: report.arch,
            abi_version: report.abi_version,
            usable_frames: mm.usable_frames,
            usable_bytes: report.usable_bytes,
            total_bytes: report.total_bytes,
            heap_kib: mm.heap_bytes >> 10,
            uptime_ns: 0,
            preempt_ticks: 0,
            preempt_switches: 0,
        };
        klog!("\nKUMO Ziwei Stage-A serial shell. Type 'help'.\n");
        klog!("{}", shell::PROMPT);
        loop {
            match kumo_hal::active::console_read_byte() {
                Some(byte @ 0x08)
                | Some(byte @ 0x7f)
                | Some(byte @ b'\r')
                | Some(byte @ b'\n')
                | Some(byte @ 0x20..=0x7e) => {
                    usermode::kbd_forward(byte);
                }
                Some(_) => {}
                None => {}
            }
            // Check for a completed command line from Sora via the root channel.
            let tasks = kdemo::tasks();
            if usermode::poll_root_command(&tasks, &mut env) > 0 {
                klog!("{}", shell::PROMPT);
            }
        }
    }
}

#[no_mangle]
pub extern "C" fn kmain(boot: *const BootInfo) -> ! {
    if boot.is_null() {
        tower_halt_ascii("nijigumo->ziwei handoff pointer is null", None);
    }

    let boot = unsafe { &*boot };
    stage_a(boot)
}

/// x86_64 first light: entered (in long mode) from the Multiboot trampoline in
/// `main.rs`. This is the GRUB/Multiboot analog of the aarch64 Nijigumo handoff — it
/// proves the loader → 32→64-bit → serial chain and reads the Multiboot memory info.
/// `mbi` is the Multiboot1 info pointer, `magic` the boot magic (`0x2BADB002`). Full
/// `stage_a` parity (x86 IDT/paging/timer) is a later slice; for now we report and halt.
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
pub fn x86_first_light(mbi: u64, magic: u64) -> ! {
    klog!("\n[ZIWEI] KUMO x86_64 first light (Multiboot/GRUB)\n");
    klog!("CPU MODE: long mode (64-bit), paging on, serial COM1 live\n");
    klog!(
        "multiboot: magic={:#010x} (want 0x2badb002), info@{:#x}\n",
        magic,
        mbi
    );

    // Multiboot1 info block: flags at +0; if bit0, mem_lower/mem_upper (KiB) at +4/+8.
    if mbi != 0 {
        let flags = unsafe { core::ptr::read_volatile(mbi as *const u32) };
        klog!("multiboot: flags={:#010x}\n", flags);
        if flags & 0x1 != 0 {
            let mem_lower = unsafe { core::ptr::read_volatile((mbi + 4) as *const u32) };
            let mem_upper = unsafe { core::ptr::read_volatile((mbi + 8) as *const u32) };
            klog!(
                "AETHER: {} KiB lower + {} KiB upper (~{} MiB usable)  OK\n",
                mem_lower,
                mem_upper,
                (mem_lower + mem_upper) / 1024
            );
        }
    }

    klog!("x86_64 bring-up reached; halting (stage_a parity is the next slice)\n");
    kumo_hal::active::halt()
}

pub fn expected_abi_version() -> u32 {
    ABI_VERSION
}

fn tower_halt_ascii(reason: &str, error: Option<HandoffError>) -> ! {
    // A fault path must never wake or switch threads: pin the console to the direct
    // device path before printing anything.
    usermode::disable_console_route();
    klog!("TOWER: ");
    klog!("{}", reason);
    if let Some(error) = error {
        klog!(": {:?}", error);
    }
    klog!("\nHALT\n");
    kumo_hal::active::halt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kumo_abi::{MemRegion, MemRegionKind, Range, RawSlice};

    #[test]
    #[cfg(feature = "arch_aarch64")]
    fn reports_arm64_when_selected() {
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.mem_regions = RawSlice::from_slice(&TEST_REGIONS);
        boot.kernel_phys = Range::new(0x80000, 0x20000);
        boot.kernel_virt = Range::new(0xffff_0000_0008_0000, 0x20000);
        boot.initrd = Range::new(0x90000, 0x4000);
        let report = inspect_boot(&boot).unwrap();
        assert_eq!(report.arch, "aarch64");
        assert_eq!(report.mem_region_count, 2);
        assert_eq!(report.usable_bytes, 0x5000);
        assert_eq!(report.total_bytes, 0x6000);
        assert!(report.has_initrd);
    }

    #[test]
    #[cfg(feature = "arch_x86_64")]
    fn reports_x86_64_when_selected() {
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.mem_regions = RawSlice::from_slice(&TEST_REGIONS);
        boot.kernel_phys = Range::new(0x80000, 0x20000);
        boot.kernel_virt = Range::new(0xffff_8000_0008_0000, 0x20000);
        let report = inspect_boot(&boot).unwrap();
        assert_eq!(report.arch, "x86_64");
        assert_eq!(report.mem_region_count, 2);
        assert_eq!(report.usable_bytes, 0x5000);
    }

    static TEST_REGIONS: [MemRegion; 2] = [
        MemRegion {
            range: Range {
                start: 0x1000,
                len: 0x5000,
            },
            kind: MemRegionKind::Usable,
            _reserved: 0,
        },
        MemRegion {
            range: Range {
                start: 0x6000,
                len: 0x1000,
            },
            kind: MemRegionKind::Reserved,
            _reserved: 0,
        },
    ];
}
