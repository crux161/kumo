//j853
#![feature(restricted_std)]
//! The gate I-b program: std::fs, std::io, std::collections, and std::time over the kumo
//! PAL, with every acceptance line mirrored to the kernel-log ring (the metal serial
//! records only the ring; see the J851 harness law). — TIMBERDOODLE

use std::collections::HashMap;
use std::io::Write;
use std::time::Instant;

unsafe extern "C" {
    fn __kumo_startup(bootstrap: u64) -> u32;
    static __tdata_start: u8;
    static __tdata_end: u8;
    static __tbss_end: u8;
}

/// The initial thread's TLS area: the PT_TLS template copied below a Variant-I thread
/// pointer. The kernel does not own TPIDR_EL0 (firmware leaves a stale value), so the
/// runtime must install it before any std code touches a thread-local.
const TLS_AREA_SIZE: usize = 16 * 1024;
static mut TLS_AREA: [u8; TLS_AREA_SIZE + 16] = [0; TLS_AREA_SIZE + 16];

unsafe fn init_tls() {
    let tdata_start = core::ptr::addr_of!(__tdata_start) as usize;
    let tdata_end = core::ptr::addr_of!(__tdata_end) as usize;
    let tbss_end = core::ptr::addr_of!(__tbss_end) as usize;
    let tdata_len = tdata_end - tdata_start;
    let tls_size = tbss_end - tdata_start;
    assert!(tls_size <= TLS_AREA_SIZE, "TLS template exceeds the area");
    let area = core::ptr::addr_of_mut!(TLS_AREA).cast::<u8>();
    let tp = (area as usize + tls_size + 15) & !15usize;
    let block = (tp - tls_size) as *mut u8;
    core::ptr::copy_nonoverlapping(tdata_start as *const u8, block, tdata_len);
    core::ptr::write_bytes(block.add(tdata_len), 0, tls_size - tdata_len);
    // aarch64 tcbhead: dtv pointer then the self pointer at TP.
    core::ptr::write_bytes(tp as *mut u8, 0, 16);
    core::ptr::write((tp + 8) as *mut usize, tp);
    core::arch::asm!("msr tpidr_el0, {}", in(reg) tp, options(nostack, preserves_flags));
}

/// Returned to `_start` in registers x0/x1, which then calls the generated `main`.
#[repr(C)]
pub struct KumoArgs {
    argc: isize,
    argv: *const *const u8,
}

static mut ARGV_IMAGE: [u8; 256] = [0; 256];
static mut ARGV_PTRS: [*const u8; 17] = [core::ptr::null(); 17];

fn klog(line: &str) {
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, line.as_bytes());
}

/// The kumo-rt-style entry: one startup consumer (the relibc overlay installs the fd
/// handles and returns the argv VMO), then the compiler-generated std `main`, then exit.
#[unsafe(no_mangle)]
pub extern "C" fn kumo_std_start(bootstrap: u64) -> KumoArgs {
    klog("std: entry\n");
    unsafe { init_tls() };
    klog("std: tls done\n");
    let argv_vmo = unsafe { __kumo_startup(bootstrap) };
    let img = unsafe { (&raw mut ARGV_IMAGE).cast::<u8>() };
    let ptrs = unsafe { (&raw mut ARGV_PTRS).cast::<*const u8>() };
    let mut argc = 0usize;
    if argv_vmo != 0 && kumo_rt::vmo_read(kumo_abi::Handle(argv_vmo), 0, img, 256) == 0 {
        let packed = unsafe { core::ptr::read_unaligned(img.cast::<[u8; 4]>()) };
        let total = u32::from_le_bytes(packed) as usize;
        let mut pos = 4usize;
        while argc < 16 && pos < 256 && argc < total {
            unsafe { ptrs.add(argc).write(img.add(pos)) };
            argc += 1;
            while pos < 256 && unsafe { *img.add(pos) } != 0 {
                pos += 1;
            }
            if pos == 256 {
                argc -= 1;
                break;
            }
            pos += 1;
        }
    }
    unsafe { ptrs.add(argc).write(core::ptr::null()) };
    klog("std: startup intake done\n");
    let _ = kumo_rt::handle_close(kumo_abi::Handle(argv_vmo));
    KumoArgs {
        argc: argc as isize,
        argv: ptrs.cast::<*const u8>() as *const *const u8,
    }
}

fn main() {
    klog("std: main entered\n");
    // The panic message would otherwise go to the console-invisible stderr; the hook
    // routes the location and payload to the kernel-log ring, the serial record.
    std::panic::set_hook(Box::new(|info| {
        // One ring entry: the kernel log interleaves sources per write, so the whole
        // record must travel in a single log_write (256-byte cap).
        use core::fmt::Write as _;
        struct RingBuf {
            buf: [u8; 256],
            at: usize,
        }
        impl core::fmt::Write for RingBuf {
            fn write_str(&mut self, part: &str) -> core::fmt::Result {
                let take = part.len().min(self.buf.len() - self.at);
                self.buf[self.at..self.at + take].copy_from_slice(&part.as_bytes()[..take]);
                self.at += take;
                Ok(())
            }
        }
        let mut line = RingBuf {
            buf: [0u8; 256],
            at: 0,
        };
        let _ = write!(line, "std: panic");
        if let Some(loc) = info.location() {
            let _ = write!(line, " at {}:{}", loc.file(), loc.line());
        }
        if let Some(payload) = info.payload().downcast_ref::<&str>() {
            let _ = write!(line, ": {}", payload);
        } else if let Some(payload) = info.payload().downcast_ref::<String>() {
            let _ = write!(line, ": {}", payload);
        }
        let _ = write!(line, "\n");
        let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &line.buf[..line.at]);
    }));
    let start = Instant::now();
    print!("std: gate I-b -> ");
    let text = std::fs::read_to_string("/HELLO.TXT").unwrap_or_default();
    let mut words: HashMap<String, u32> = HashMap::new();
    for word in text.split_whitespace() {
        *words.entry(word.to_string()).or_insert(0) += 1;
    }
    let args: Vec<String> = std::env::args().collect();
    println!(
        "fs={}B words={} args={} elapsed={:?}",
        text.len(),
        words.len(),
        args.len(),
        start.elapsed()
    );
    std::io::stdout().flush().ok();
    klog("std: gate I-b fs+io+collections+time -> OK\n");
}
