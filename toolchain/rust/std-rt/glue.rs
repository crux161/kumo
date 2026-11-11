//j857
//j873
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
    /// lld's ELF-header symbol: link.ld keeps the file and program headers in the first
    /// PT_LOAD, so the image can read its own PT_TLS (as musl does through AT_PHDR).
    static __ehdr_start: u8;
}

/// The initial thread's TLS area: the PT_TLS template copied at the AArch64 Variant-I
/// offset above the thread pointer. The kernel does not own TPIDR_EL0 (firmware leaves a
/// stale value), so the runtime must install it before any std code touches a thread-local.
const TLS_AREA_SIZE: usize = 16 * 1024;
static mut TLS_AREA: [u8; TLS_AREA_SIZE] = [0; TLS_AREA_SIZE];

/// relibc's `Tcb` for the initial thread, laid out as relibc lays its own (ld_so::tcb):
/// a `GenericTcb {tls_end, tls_len, tcb_ptr, tcb_len}` that names itself, then
/// `masters_ptr`, `masters_len`, `num_copied_masters`, `linker_ptr` (null: no dynamic
/// linker), then the pthread state relibc's `pthread::init` fills. `[TPIDR_EL0]` names it.
#[repr(C, align(16))]
struct TcbStorage([usize; 32]);
static mut TCB_STORAGE: TcbStorage = TcbStorage([0; 32]);
/// relibc's `Master {ptr, image_size, segment_size, offset}`: the TLS initialisation image a
/// thread made by `pthread_create` copies into its own block (SH5). `offset` is measured
/// from relibc's TLS region start, which sits 16 bytes above the thread pointer.
static mut TLS_MASTER: [usize; 4] = [0; 4];

/// PT_TLS's alignment, read from this image's own program headers (at least 1).
unsafe fn tls_align() -> usize {
    let ehdr = core::ptr::addr_of!(__ehdr_start);
    let phoff = core::ptr::read_unaligned(ehdr.add(0x20).cast::<u64>()) as usize;
    let phentsize = core::ptr::read_unaligned(ehdr.add(0x36).cast::<u16>()) as usize;
    let phnum = core::ptr::read_unaligned(ehdr.add(0x38).cast::<u16>()) as usize;
    for index in 0..phnum {
        let phdr = ehdr.add(phoff + index * phentsize);
        if core::ptr::read_unaligned(phdr.cast::<u32>()) == 7 {
            // PT_TLS: p_align is the last field of an Elf64_Phdr.
            return (core::ptr::read_unaligned(phdr.add(0x30).cast::<u64>()) as usize).max(1);
        }
    }
    1
}

//j873
// SH5: one Variant-I layout for every thread. The compiler addresses a thread-local at
// TP + align_up(16, p_align) + offset. The old setup copied the template BELOW the thread
// pointer (so an initialised `.tdata` value read back as zero) and kept relibc's header in
// a separate static (so `pthread_create` sized a new thread's TLS from a zeroed `tls_len`
// and faulted). Sizes come from the section symbols, not PT_TLS's memsz, which lld reports
// short when link.ld page-aligns `.tbss`. — TEAL
unsafe fn init_tls() {
    let tdata_start = core::ptr::addr_of!(__tdata_start) as usize;
    let tdata_end = core::ptr::addr_of!(__tdata_end) as usize;
    let tbss_end = core::ptr::addr_of!(__tbss_end) as usize;
    let image_size = tdata_end - tdata_start;
    let segment_size = tbss_end - tdata_start;
    let align = tls_align();
    assert!(align.is_power_of_two(), "PT_TLS alignment");
    let gap = (16 + align - 1) & !(align - 1);
    // relibc's TLS region starts at TP + 16: the image sits `gap - 16` into it.
    let offset = gap - 16;
    let tls_len = offset + segment_size;
    assert!(gap + segment_size + align <= TLS_AREA_SIZE, "TLS template exceeds the area");
    let area = core::ptr::addr_of_mut!(TLS_AREA).cast::<u8>() as usize;
    let block = (area + gap + align - 1) & !(align - 1);
    let tp = block - gap;
    core::ptr::copy_nonoverlapping(tdata_start as *const u8, block as *mut u8, image_size);
    core::ptr::write_bytes((block + image_size) as *mut u8, 0, segment_size - image_size);

    let master = core::ptr::addr_of_mut!(TLS_MASTER).cast::<usize>();
    core::ptr::write(master, tdata_start);
    core::ptr::write(master.add(1), image_size);
    core::ptr::write(master.add(2), segment_size);
    core::ptr::write(master.add(3), offset);

    let tcb = core::ptr::addr_of_mut!(TCB_STORAGE).cast::<usize>();
    core::ptr::write(tcb, tp + 16 + tls_len); // tls_end
    core::ptr::write(tcb.add(1), tls_len);
    core::ptr::write(tcb.add(2), tcb as usize); // tcb_ptr: itself
    core::ptr::write(tcb.add(3), core::mem::size_of::<TcbStorage>());
    core::ptr::write(tcb.add(4), master as usize); // masters_ptr
    core::ptr::write(tcb.add(5), 4 * core::mem::size_of::<usize>()); // masters_len (bytes)
    core::ptr::write(tcb.add(6), 1); // num_copied_masters: this thread holds its copy
    core::ptr::write(tp as *mut usize, tcb as usize);
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
