//j857
// SH0 (PLAN/022): the KUMO std runtime glue, moved verbatim from the J856 smoke so
// every std image shares one entry contract. It is `include!`d into the binary crate
// (not linked as an archive) because `start.o` enters the link through a link-arg that
// follows the rlibs: a definition in an archive would not be pulled for its undefined
// symbols. SH1's SDK may promote this to a native archive placed after `start.o`.
// The TLS/TCB/stdin layout here is the one J856 proved; do not vary it silently.
// — TIMBERDOODLE

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
/// relibc's GenericTcb header for kumo (a zero-sized OsSpecific): `{tls_end, tls_len,
/// tcb_ptr, tcb_len}`. `[TPIDR_EL0]` must name this header (`GenericTcb::current_ptr`
/// follows the stored pointer), and `tcb_ptr` must name a `Tcb` below. — TIMBERDOODLE
static mut TCB_HEADER: [usize; 4] = [0; 4];
/// Zeroed `Tcb` storage: `linker_ptr` (0x38) and every other field stay null — no
/// dynamic linker, no TLS masters, no pthread state for this static image.
static mut TCB_STORAGE: [u8; 256] = [0; 256];

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
    // relibc's Variant-I ABI slot: the word at TP names the GenericTcb header, whose
    // tcb_ptr names the Tcb. The glibc {dtv, self} pair this used to hold left
    // Tcb::current() null and faulted ld_so::fini at exit (J856).
    let header = core::ptr::addr_of_mut!(TCB_HEADER).cast::<usize>();
    let tcb = core::ptr::addr_of_mut!(TCB_STORAGE).cast::<u8>() as usize;
    core::ptr::write(header, tp);
    core::ptr::write(header.add(1), tls_size);
    core::ptr::write(header.add(2), tcb);
    core::ptr::write(header.add(3), 256);
    core::ptr::write(tp as *mut usize, header as usize);
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

/// `_start` calls this with lang_start's status once `main` returns: returning from
/// `main` must terminate the process (C semantics). The old `b .` spin left every std
/// program alive after its last record — the J856 exit defect. — TIMBERDOODLE
unsafe extern "C" {
    /// relibc's C `exit`: atexit handlers, fini arrays, stdio flush, then ProcessExit.
    fn exit(status: i32) -> !;
}

#[unsafe(no_mangle)]
pub extern "C" fn kumo_std_exit(status: i32) -> ! {
    unsafe { exit(status) }
}
