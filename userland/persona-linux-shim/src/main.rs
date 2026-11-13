#![no_std]
#![no_main]
//j885
//j888

//! `persona-linux-shim` — the in-process Linux personality runtime (SH7, PLAN.md §10 R1).
//!
//! This is the process image. Sora runs it with a VMO holding an aarch64 Linux ELF (`x0`,
//! transferred) and that ELF's length (`x1`). The shim loads the program's `PT_LOAD` segments
//! into its own address space at their link addresses, builds the Linux initial stack
//! (argc/argv/envp/auxv), registers itself with the kernel (`PersonaShimSet`) and jumps to the
//! program's entry. From then on every `svc` the program executes is redirected here by the
//! kernel (registers only — the kernel interprets no Linux number): `persona_trap_entry` saves
//! the program's registers, `persona_dispatch` serves the call on KUMO's native ABI, and
//! `PersonaReturn` resumes the program with the result in `x0`.
//!
//! The surface is demand-driven (PLAN.md §10, DESIGN/008 §5): what the M10 payload and a musl
//! static program's startup need. Anything else answers `-ENOSYS` and says so in the log, which
//! is how the next slice learns what to add. — TEAL

use core::fmt::Write as _;

use kumo_abi::Handle;
use kumo_rt::{log_write, process_exit, vmar_map, vmar_map_anywhere, vmar_unmap, vmo_create};
use persona_linux::elf::{self, Machine};
use persona_linux::launch;

kumo_rt::entry!(main);

// ---- Linux aarch64 ABI ------------------------------------------------------------------

mod nr {
    pub const IOCTL: u64 = 29;
    pub const OPENAT: u64 = 56;
    pub const CLOSE: u64 = 57;
    pub const READ: u64 = 63;
    pub const WRITE: u64 = 64;
    pub const WRITEV: u64 = 66;
    pub const NEWFSTATAT: u64 = 79;
    pub const FSTAT: u64 = 80;
    pub const EXIT: u64 = 93;
    pub const EXIT_GROUP: u64 = 94;
    pub const SET_TID_ADDRESS: u64 = 96;
    pub const RT_SIGACTION: u64 = 134;
    pub const RT_SIGPROCMASK: u64 = 135;
    pub const GETPID: u64 = 172;
    pub const GETTID: u64 = 178;
    pub const BRK: u64 = 214;
    pub const MUNMAP: u64 = 215;
    pub const MMAP: u64 = 222;
}

mod errno {
    pub const ENOENT: i64 = 2;
    pub const EBADF: i64 = 9;
    pub const ENOMEM: i64 = 12;
    pub const EFAULT: i64 = 14;
    pub const EINVAL: i64 = 22;
    pub const ENOTTY: i64 = 25;
    pub const ENOSYS: i64 = 38;
}

const PAGE: u64 = 4096;
const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const PROT_EXEC: u64 = 4;
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;

// KUMO VMAR flags (kumo_abi::VmarFlags): READ, WRITE, EXECUTE.
const MAP_R: u64 = 1;
const MAP_W: u64 = 2;
const MAP_X: u64 = 4;

/// The program's stack: 256 KiB ending here, below the shim's image at 0x1c00_0000.
const LINUX_STACK_TOP: u64 = 0x1b00_0000;
const LINUX_STACK_SIZE: u64 = 0x4_0000;

/// SH7-c: where a position-independent program and its interpreter are loaded. The child VMAR
/// is 512 MiB from 0; the kernel places anywhere-maps above the highest mapping (the shim at
/// 0x1c00_0000), so these fixed bases stay clear of it, of each other and of the stack. The
/// fixed-address M10 hello links at 0x1000_0000 and never shares a process with them.
const PROGRAM_BIAS: u64 = 0x0100_0000;
const INTERP_BASE: u64 = 0x1400_0000;

const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_BASE: u64 = 7;
const AT_ENTRY: u64 = 9;
const AT_UID: u64 = 11;
const AT_EUID: u64 = 12;
const AT_GID: u64 = 13;
const AT_EGID: u64 = 14;
const AT_HWCAP: u64 = 16;
const AT_CLKTCK: u64 = 17;
const AT_SECURE: u64 = 23;
const AT_RANDOM: u64 = 25;

// ---- KUMO persona calls ------------------------------------------------------------------

const SYSCALL_PERSONA_SHIM_SET: u64 = 63;

fn persona_shim_set(entry: u64, code_base: u64, code_len: u64, stack_top: u64) -> u64 {
    let status: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") SYSCALL_PERSONA_SHIM_SET,
            inlateout("x0") entry => status,
            in("x1") code_base,
            in("x2") code_len,
            in("x3") stack_top,
            clobber_abi("C"),
            options(nostack),
        );
    }
    status
}

// ---- the trap path -----------------------------------------------------------------------

/// The shim's own stack for serving calls (one thread in this slice).
#[repr(C, align(16))]
struct TrapStack([u8; 64 * 1024]);
static mut TRAP_STACK: TrapStack = TrapStack([0; 64 * 1024]);

unsafe extern "C" {
    /// lld: the first byte of the image (the ELF header), so the start of the shim's code.
    static __ehdr_start: u8;
    /// lld: the end of the text.
    static etext: u8;
    fn persona_trap_entry();
    fn persona_enter(entry: u64, sp: u64) -> !;
}

core::arch::global_asm!(
    ".text",
    // Entered by the kernel's redirect: SP = the trap stack, every other register as the
    // foreign program left it at its `svc` (x8 = the Linux number, x0..x5 = arguments).
    ".global persona_trap_entry",
    ".type persona_trap_entry, %function",
    "persona_trap_entry:",
    "  sub sp, sp, #256",
    "  stp x0, x1, [sp, #0]",
    "  stp x2, x3, [sp, #16]",
    "  stp x4, x5, [sp, #32]",
    "  stp x6, x7, [sp, #48]",
    "  stp x8, x9, [sp, #64]",
    "  stp x10, x11, [sp, #80]",
    "  stp x12, x13, [sp, #96]",
    "  stp x14, x15, [sp, #112]",
    "  stp x16, x17, [sp, #128]",
    "  stp x18, x19, [sp, #144]",
    "  stp x20, x21, [sp, #160]",
    "  stp x22, x23, [sp, #176]",
    "  stp x24, x25, [sp, #192]",
    "  stp x26, x27, [sp, #208]",
    "  stp x28, x29, [sp, #224]",
    "  str x30, [sp, #240]",
    "  mov x0, sp",
    "  bl persona_dispatch",
    // Restore the foreign registers except x0 (the result) and x8 (the kernel restores it).
    "  ldr x1, [sp, #8]",
    "  ldp x2, x3, [sp, #16]",
    "  ldp x4, x5, [sp, #32]",
    "  ldp x6, x7, [sp, #48]",
    "  ldr x9, [sp, #72]",
    "  ldp x10, x11, [sp, #80]",
    "  ldp x12, x13, [sp, #96]",
    "  ldp x14, x15, [sp, #112]",
    "  ldp x16, x17, [sp, #128]",
    "  ldp x18, x19, [sp, #144]",
    "  ldp x20, x21, [sp, #160]",
    "  ldp x22, x23, [sp, #176]",
    "  ldp x24, x25, [sp, #192]",
    "  ldp x26, x27, [sp, #208]",
    "  ldp x28, x29, [sp, #224]",
    "  ldr x30, [sp, #240]",
    "  ldr x0, [sp, #0]",
    "  mov x8, #64", // PersonaReturn
    "  svc #0",
    "  brk #0x50",
    // Enter the foreign program: x0 = entry, x1 = its initial SP. Registers start clean.
    ".global persona_enter",
    ".type persona_enter, %function",
    "persona_enter:",
    "  mov sp, x1",
    "  mov x16, x0",
    "  mov x0, xzr",
    "  mov x1, xzr",
    "  mov x2, xzr",
    "  mov x3, xzr",
    "  mov x4, xzr",
    "  mov x5, xzr",
    "  mov x6, xzr",
    "  mov x7, xzr",
    "  mov x8, xzr",
    "  mov x29, xzr",
    "  mov x30, xzr",
    "  br x16",
);

/// The foreign program's state the shim keeps: its program break.
struct Program {
    brk_end: u64,
}

static mut PROGRAM: Program = Program { brk_end: 0 };

/// Serve one redirected call. `frame` holds the foreign x0..x30; the result goes in x0.
#[unsafe(no_mangle)]
extern "C" fn persona_dispatch(frame: *mut [u64; 31]) {
    let frame = unsafe { &mut *frame };
    let (a0, a1, a2) = (frame[0], frame[1], frame[2]);
    let result: i64 = match frame[8] {
        nr::WRITE => write(a0, a1, a2),
        nr::WRITEV => writev(a0, a1, a2),
        nr::READ => {
            if a0 == 0 {
                0 // no stdin in this slice: end of file
            } else {
                -errno::EBADF
            }
        }
        nr::CLOSE => 0,
        nr::OPENAT | nr::NEWFSTATAT => -errno::ENOENT,
        nr::FSTAT => -errno::EBADF,
        nr::IOCTL => -errno::ENOTTY,
        nr::SET_TID_ADDRESS | nr::GETPID | nr::GETTID => 1,
        nr::RT_SIGACTION | nr::RT_SIGPROCMASK => 0,
        nr::BRK => brk(a0) as i64,
        nr::MMAP => mmap(a0, a1, a2, frame[3]),
        nr::MUNMAP => {
            if vmar_unmap(a0, a1.next_multiple_of(PAGE)) == 0 {
                0
            } else {
                -errno::EINVAL
            }
        }
        nr::EXIT | nr::EXIT_GROUP => process_exit(a0 & 0xff),
        other => {
            log(format_args!(
                "persona-linux: syscall {} unsupported (-ENOSYS)\n",
                other
            ));
            -errno::ENOSYS
        }
    };
    frame[0] = result as u64;
}

/// Standard output and error go to the kernel log, as the Stage-A bridge sent them (an INFO
/// boot record, echoed when the console level allows).
fn write(fd: u64, buf: u64, len: u64) -> i64 {
    if fd != 1 && fd != 2 {
        return -errno::EBADF;
    }
    if buf == 0 && len != 0 {
        return -errno::EFAULT;
    }
    let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, len as usize) };
    for chunk in bytes.chunks(256) {
        let _ = log_write(kumo_abi::sys::log_level::INFO, chunk);
    }
    len as i64
}

/// One `writev` reaches the log as few records as possible: its vectors are gathered into
/// record-sized (256-byte) pieces, so a short line arrives whole. musl's stdio hands `printf`'s
/// text and its `\n` over as two vectors; written one record each, another process's line could
/// land between them (seen on metal, j888). A small `writev` is atomic on Linux too.
fn writev(fd: u64, iov: u64, count: u64) -> i64 {
    if fd != 1 && fd != 2 {
        return -errno::EBADF;
    }
    let mut record = [0u8; 256];
    let mut used = 0usize;
    let mut total = 0i64;
    for index in 0..count.min(1024) {
        let entry = iov + index * 16;
        let (base, len) = unsafe {
            (
                core::ptr::read_unaligned(entry as *const u64),
                core::ptr::read_unaligned((entry + 8) as *const u64),
            )
        };
        if base == 0 && len != 0 {
            return if total > 0 { total } else { -errno::EFAULT };
        }
        let mut bytes = unsafe { core::slice::from_raw_parts(base as *const u8, len as usize) };
        while !bytes.is_empty() {
            let take = bytes.len().min(record.len() - used);
            record[used..used + take].copy_from_slice(&bytes[..take]);
            used += take;
            bytes = &bytes[take..];
            if used == record.len() {
                let _ = log_write(kumo_abi::sys::log_level::INFO, &record);
                used = 0;
            }
        }
        total += len as i64;
    }
    if used > 0 {
        let _ = log_write(kumo_abi::sys::log_level::INFO, &record[..used]);
    }
    total
}

/// The program break: grows by mapping fresh pages at the old end; never shrinks.
fn brk(request: u64) -> u64 {
    let program = unsafe { &mut *core::ptr::addr_of_mut!(PROGRAM) };
    if request <= program.brk_end {
        return program.brk_end;
    }
    let new_end = request.next_multiple_of(PAGE);
    let len = new_end - program.brk_end;
    let Some(vmo) = vmo(len) else {
        return program.brk_end;
    };
    let mapped = vmar_map(Handle(0), vmo, 0, program.brk_end, len, MAP_R | MAP_W) == 0;
    let _ = kumo_rt::handle_close(vmo);
    if mapped {
        program.brk_end = new_end;
    }
    program.brk_end
}

/// Anonymous private mappings only in this slice; a file mapping answers -ENOSYS.
fn mmap(addr: u64, len: u64, prot: u64, flags: u64) -> i64 {
    if flags & MAP_ANONYMOUS == 0 {
        log(format_args!(
            "persona-linux: file-backed mmap unsupported (-ENOSYS)\n"
        ));
        return -errno::ENOSYS;
    }
    if len == 0 {
        return -errno::EINVAL;
    }
    let len = len.next_multiple_of(PAGE);
    let Some(vmo) = vmo(len) else {
        return -errno::ENOMEM;
    };
    let mut map = 0;
    if prot & PROT_READ != 0 {
        map |= MAP_R;
    }
    if prot & PROT_WRITE != 0 {
        map |= MAP_W;
    }
    if prot & PROT_EXEC != 0 {
        map |= MAP_X;
    }
    // MAP_FIXED must land at `addr`; without it `addr` is only a hint (Linux semantics).
    let at_hint = addr != 0 && addr % PAGE == 0 && vmar_map(Handle(0), vmo, 0, addr, len, map) == 0;
    let result = if at_hint {
        addr as i64
    } else if flags & MAP_FIXED != 0 {
        -errno::ENOMEM
    } else {
        match vmar_map_anywhere(vmo, len, map) {
            (0, mapped) if mapped != 0 => mapped as i64,
            _ => -errno::ENOMEM,
        }
    };
    let _ = kumo_rt::handle_close(vmo);
    result
}

fn vmo(len: u64) -> Option<Handle> {
    let raw = vmo_create(len);
    (raw != 0 && raw != u64::MAX).then_some(Handle(raw as u32))
}

fn log(args: core::fmt::Arguments<'_>) {
    struct Line {
        bytes: [u8; 160],
        len: usize,
    }
    impl core::fmt::Write for Line {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            let take = text.len().min(self.bytes.len() - self.len);
            self.bytes[self.len..self.len + take].copy_from_slice(&text.as_bytes()[..take]);
            self.len += take;
            Ok(())
        }
    }
    let mut line = Line {
        bytes: [0; 160],
        len: 0,
    };
    let _ = line.write_fmt(args);
    let _ = log_write(kumo_abi::sys::log_level::NOTICE, &line.bytes[..line.len]);
}

fn fail(what: &str) -> ! {
    log(format_args!("persona-linux: {}\n", what));
    process_exit(127)
}

// ---- loading ------------------------------------------------------------------------------

/// What the stack and auxv need to know about the loaded images.
struct Loaded {
    /// The program's entry, biased: `AT_ENTRY`.
    program_entry: u64,
    /// The program's headers in memory: `AT_PHDR`.
    phdr: u64,
    phnum: u64,
    /// The interpreter's base (`AT_BASE`), or 0 for a static program.
    interp_base: u64,
    /// Where control goes first: the interpreter's entry, or the program's.
    start: u64,
    /// The end of the program's image, where the break starts.
    image_end: u64,
}

#[unsafe(no_mangle)]
extern "C" fn main(
    image_vmo: u64,
    image_len: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
    _a7: u64,
    _a8: u64,
) -> ! {
    let image_vmo = Handle(image_vmo as u32);
    let map_len = image_len.next_multiple_of(PAGE);
    let image = match vmar_map_anywhere(image_vmo, map_len, MAP_R) {
        (0, addr) if addr != 0 => unsafe {
            core::slice::from_raw_parts(addr as *const u8, image_len as usize)
        },
        _ => fail("cannot map the program image"),
    };
    // SH7-c: a launch bundle carries a program and the interpreter its PT_INTERP names; a bare
    // ELF is one static program, as M10 has always sent.
    let (program, interp, name) = match launch::parse(image) {
        Ok(bundle) => (bundle.program, bundle.interp, bundle.name),
        Err(launch::LaunchError::NotABundle) => (image, None, &b"persona"[..]),
        Err(_) => fail("malformed launch bundle"),
    };
    let loaded = load(program, interp);
    let program_state = unsafe { &mut *core::ptr::addr_of_mut!(PROGRAM) };
    program_state.brk_end = loaded.image_end.next_multiple_of(PAGE);

    let sp = build_stack(&loaded, name);

    let code_base = core::ptr::addr_of!(__ehdr_start) as u64;
    let code_len = core::ptr::addr_of!(etext) as u64 - code_base;
    let trap_top =
        core::ptr::addr_of!(TRAP_STACK) as u64 + core::mem::size_of::<TrapStack>() as u64;
    if persona_shim_set(
        persona_trap_entry as *const () as u64,
        code_base,
        code_len,
        trap_top,
    ) != 0
    {
        fail("PersonaShimSet refused");
    }
    unsafe { persona_enter(loaded.start, sp) }
}

/// Load the program and, for a dynamic one, the interpreter it names, as Linux's `execve`
/// does: a PIE goes at `PROGRAM_BIAS`, the interpreter at `INTERP_BASE`, and control starts in
/// the interpreter, which finds the program through `AT_PHDR`/`AT_ENTRY`.
fn load(program: &[u8], interp: Option<&[u8]>) -> Loaded {
    let header = match elf::parse_linux_image(program, Machine::Aarch64) {
        Ok(header) => header,
        Err(_) => fail("not an aarch64 Linux executable"),
    };
    let bias = if header.position_independent {
        PROGRAM_BIAS
    } else {
        0
    };
    let wants = match elf::interpreter(program, &header) {
        Ok(path) => path,
        Err(_) => fail("bad PT_INTERP"),
    };
    let (phdr, image_end) = load_image(program, &header, bias);

    let (interp_base, start) = match (wants, interp) {
        (None, None) => (0, header.entry + bias),
        (Some(_), Some(interp)) => {
            let interp_header = match elf::parse_linux_image(interp, Machine::Aarch64) {
                Ok(h) if h.position_independent => h,
                _ => fail("the interpreter is not a position-independent aarch64 image"),
            };
            if !matches!(elf::interpreter(interp, &interp_header), Ok(None)) {
                fail("the interpreter names an interpreter of its own");
            }
            let _ = load_image(interp, &interp_header, INTERP_BASE);
            (INTERP_BASE, INTERP_BASE + interp_header.entry)
        }
        (Some(_), None) => fail("a dynamic program arrived without its interpreter"),
        (None, Some(_)) => fail("an interpreter arrived for a static program"),
    };
    Loaded {
        program_entry: header.entry + bias,
        phdr,
        phnum: header.phnum as u64,
        interp_base,
        start,
        image_end,
    }
}

/// Map every `PT_LOAD` of `image` at `bias` + its link address. Returns where the program
/// headers are in memory (from `PT_PHDR`, else the load that holds them) and the image's end.
fn load_image(image: &[u8], header: &elf::ElfHeader, bias: u64) -> (u64, u64) {
    let mut phdr_addr = 0u64;
    let mut image_end = 0u64;
    for index in 0..header.phnum {
        let Ok(ph) = elf::program_header(image, header, index) else {
            fail("bad program header");
        };
        if ph.kind == elf::PT_PHDR {
            phdr_addr = ph.virt_addr + bias;
            continue;
        }
        if ph.kind != elf::PT_LOAD || ph.mem_size == 0 {
            continue;
        }
        load_segment(image, &ph, bias);
        if phdr_addr == 0
            && header.phoff >= ph.file_offset
            && header.phoff < ph.file_offset + ph.file_size
        {
            phdr_addr = ph.virt_addr + bias + (header.phoff - ph.file_offset);
        }
        image_end = image_end.max(ph.virt_addr + bias + ph.mem_size);
    }
    (phdr_addr, image_end)
}

fn load_segment(image: &[u8], ph: &elf::ProgramHeader, bias: u64) {
    let vaddr = ph.virt_addr + bias;
    let start = vaddr & !(PAGE - 1);
    let end = (vaddr + ph.mem_size).next_multiple_of(PAGE);
    let Some(vmo) = vmo(end - start) else {
        fail("no memory for a segment");
    };
    let from = ph.file_offset as usize;
    let Some(bytes) = image.get(from..from + ph.file_size as usize) else {
        fail("segment out of range");
    };
    // Fill through a temporary writable view: VmoWrite moves at most VMO_TRANSFER_MAX (256)
    // bytes per call, far too few for a real program's segments. The fresh VMO is zeroed, so
    // the bytes between `p_filesz` and `p_memsz` (.bss) start as zero.
    if !bytes.is_empty() {
        let view = match vmar_map_anywhere(vmo, end - start, MAP_R | MAP_W) {
            (0, addr) if addr != 0 => addr,
            _ => fail("cannot fill a segment"),
        };
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                (view + (vaddr - start)) as *mut u8,
                bytes.len(),
            );
        }
        let _ = vmar_unmap(view, end - start);
    }
    let mut map = 0;
    if ph.flags & elf::PF_R != 0 {
        map |= MAP_R;
    }
    if ph.flags & elf::PF_W != 0 {
        map |= MAP_W;
    }
    if ph.flags & elf::PF_X != 0 {
        map |= MAP_X;
    }
    if vmar_map(Handle(0), vmo, 0, start, end - start, map) != 0 {
        fail("cannot map a segment at its link address");
    }
    let _ = kumo_rt::handle_close(vmo);
}

/// The Linux initial stack (SysV aarch64): argc, argv, NULL, envp NULL, auxv, then the data
/// the vectors point at. Returns the 16-byte-aligned SP that points at argc.
fn build_stack(loaded: &Loaded, name: &[u8]) -> u64 {
    let Some(vmo) = vmo(LINUX_STACK_SIZE) else {
        fail("no memory for the stack");
    };
    let base = LINUX_STACK_TOP - LINUX_STACK_SIZE;
    if vmar_map(Handle(0), vmo, 0, base, LINUX_STACK_SIZE, MAP_R | MAP_W) != 0 {
        fail("cannot map the stack");
    }
    let _ = kumo_rt::handle_close(vmo);

    // Data area at the top: argv[0] (NUL-terminated; the stack is fresh, so zeroed) and
    // AT_RANDOM's 16 bytes. `name` is at most launch::NAME_MAX bytes.
    let mut top = LINUX_STACK_TOP - 128;
    let name_at = top;
    unsafe { core::ptr::copy_nonoverlapping(name.as_ptr(), name_at as *mut u8, name.len()) };
    top -= 32;
    let random_at = top;
    let seed = kumo_rt::clock_get();
    let random = [seed ^ 0x9e37_79b9_7f4a_7c15, seed.rotate_left(29) ^ name_at];
    for (i, word) in random.iter().enumerate() {
        unsafe { ((random_at + i as u64 * 8) as *mut u64).write(*word) };
    }

    let auxv = [
        (AT_PHDR, loaded.phdr),
        (AT_PHENT, elf::ELF_PHDR_LEN as u64),
        (AT_PHNUM, loaded.phnum),
        (AT_PAGESZ, PAGE),
        (AT_BASE, loaded.interp_base),
        (AT_ENTRY, loaded.program_entry),
        (AT_UID, 0),
        (AT_EUID, 0),
        (AT_GID, 0),
        (AT_EGID, 0),
        (AT_HWCAP, 0),
        (AT_CLKTCK, 100),
        (AT_SECURE, 0),
        (AT_RANDOM, random_at),
        (AT_NULL, 0),
    ];
    // argc + argv[0] + NULL + envp NULL + auxv pairs.
    let words = 1 + 2 + 1 + auxv.len() * 2;
    let sp = (top - words as u64 * 8) & !15;
    let mut at = sp;
    let mut push = |value: u64| {
        unsafe { (at as *mut u64).write(value) };
        at += 8;
    };
    push(1);
    push(name_at);
    push(0);
    push(0);
    for (key, value) in auxv {
        push(key);
        push(value);
    }
    sp
}
