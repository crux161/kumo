#![no_std]
#![no_main]
//j885
//j888
//j889
//j890

//! `persona-linux-shim` — the in-process Linux personality runtime (SH7, PLAN.md §10 R1).
//!
//! This is the process image. Since SH7-d2 Sora launches it like any program (its `linux`
//! word): a bootstrap channel in `x0` carries stdout, argv `[shim, /path/to/program, args…]`, env
//! and a filesystem grant. The shim then does Linux `execve`'s loading itself: it opens the
//! program by path, and for a dynamic one the interpreter its `PT_INTERP` names, loads their
//! `PT_LOAD` segments into its own address space, builds the Linux initial stack
//! (argc/argv/envp/auxv), registers itself with the kernel (`PersonaShimSet`) and jumps in. From then on every `svc` the program executes is redirected here by the
//! kernel (registers only — the kernel interprets no Linux number): `persona_trap_entry` saves
//! the program's registers, `persona_dispatch` serves the call on KUMO's native ABI, and
//! `PersonaReturn` resumes the program with the result in `x0`.
//!
//! The surface is demand-driven (PLAN.md §10, DESIGN/008 §5): what the M10 payload and a musl
//! static program's startup need. Anything else answers `-ENOSYS` and says so in the log, which
//! is how the next slice learns what to add. — TEAL

use core::fmt::Write as _;

use kumo_abi::Handle;
use kumo_rt::{
    log_write, process_exit, vmar_map, vmar_map_anywhere, vmar_unmap, vmo_create, vmo_read,
};
use persona_linux::elf::{self, Machine};

mod files;

kumo_rt::entry!(main);

// ---- Linux aarch64 ABI ------------------------------------------------------------------

mod nr {
    pub const FCNTL: u64 = 25;
    pub const IOCTL: u64 = 29;
    pub const OPENAT: u64 = 56;
    pub const CLOSE: u64 = 57;
    pub const GETDENTS64: u64 = 61;
    pub const LSEEK: u64 = 62;
    pub const READ: u64 = 63;
    pub const WRITE: u64 = 64;
    pub const READV: u64 = 65;
    pub const WRITEV: u64 = 66;
    pub const PREAD64: u64 = 67;
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
    pub const MPROTECT: u64 = 226;
}

pub(crate) mod errno {
    pub const ENOENT: i64 = 2;
    pub const EIO: i64 = 5;
    pub const EBADF: i64 = 9;
    pub const ENOMEM: i64 = 12;
    pub const EACCES: i64 = 13;
    pub const EFAULT: i64 = 14;
    pub const ENODEV: i64 = 19;
    pub const ENOTDIR: i64 = 20;
    pub const EISDIR: i64 = 21;
    pub const EINVAL: i64 = 22;
    pub const EMFILE: i64 = 24;
    pub const ENOTTY: i64 = 25;
    pub const ESPIPE: i64 = 29;
    pub const EROFS: i64 = 30;
    pub const ENAMETOOLONG: i64 = 36;
    pub const ENOSYS: i64 = 38;
}

const PAGE: u64 = 4096;
const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const PROT_EXEC: u64 = 4;
const MAP_SHARED: u64 = 0x01;
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
const AT_EXECFN: u64 = 31;

// ---- KUMO persona calls ------------------------------------------------------------------

const SYSCALL_PERSONA_SHIM_SET: u64 = 63;
const SYSCALL_VMAR_PROTECT: u64 = 16;

/// SH7-d1: native `VmarProtect` on this process. Linux PROT_READ/WRITE/EXEC are the same bits as
/// KUMO's READ/WRITE/EXECUTE, so `prot` passes through unchanged.
fn vmar_protect(virt: u64, len: u64, prot: u64) -> u64 {
    let status: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") SYSCALL_VMAR_PROTECT,
            inlateout("x0") kumo_abi::INVALID_HANDLE.0 as u64 => status,
            in("x1") virt,
            in("x2") len,
            in("x3") prot,
            clobber_abi("C"),
            options(nostack),
        );
    }
    status
}

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
        // SH7-d2: the file surface, over the persona's vfs session (files.rs).
        nr::READ => files::read(a0 as i64, a1, a2),
        nr::READV => files::readv(a0 as i64, a1, a2),
        nr::PREAD64 => files::pread(a0 as i64, a1, a2, frame[3]),
        nr::OPENAT => files::openat(a0 as i64, a1, a2),
        nr::CLOSE => files::close(a0 as i64),
        nr::LSEEK => files::lseek(a0 as i64, a1 as i64, a2),
        nr::FSTAT => files::fstat(a0 as i64, a1),
        nr::NEWFSTATAT => files::newfstatat(a0 as i64, a1, a2, frame[3]),
        nr::GETDENTS64 => files::getdents64(a0 as i64, a1, a2),
        nr::FCNTL => files::fcntl(a0 as i64, a1),
        nr::IOCTL => -errno::ENOTTY,
        nr::SET_TID_ADDRESS | nr::GETPID | nr::GETTID => 1,
        nr::RT_SIGACTION | nr::RT_SIGPROCMASK => 0,
        nr::BRK => brk(a0) as i64,
        nr::MMAP => mmap(a0, a1, a2, frame[3], frame[4] as i64, frame[5]),
        nr::MUNMAP => munmap(a0, a1),
        nr::MPROTECT => mprotect(a0, a1, a2),
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

/// Standard output and error: the launcher's stdout channel (SH7-d2), else the kernel log.
fn write(fd: u64, buf: u64, len: u64) -> i64 {
    if buf == 0 && len != 0 {
        return -errno::EFAULT;
    }
    // SAFETY: the program's buffer, as it named it.
    let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, len as usize) };
    files::write_out(fd as i64, bytes)
}

/// One `writev` goes out in as few messages as possible: its vectors are gathered into 256-byte
/// pieces, so a short line arrives whole. musl's stdio hands `printf`'s text and its `\n` over
/// as two vectors; sent one message each, another process's line could land between them (seen
/// on metal, j888). A small `writev` is atomic on Linux too.
fn writev(fd: u64, iov: u64, count: u64) -> i64 {
    let mut record = [0u8; 256];
    let mut used = 0usize;
    let mut total = 0i64;
    for index in 0..count.min(1024) {
        let entry = iov + index * 16;
        // SAFETY: the program's iovec array.
        let (base, len) = unsafe {
            (
                core::ptr::read_unaligned(entry as *const u64),
                core::ptr::read_unaligned((entry + 8) as *const u64),
            )
        };
        if base == 0 && len != 0 {
            return if total > 0 { total } else { -errno::EFAULT };
        }
        // SAFETY: as above.
        let mut bytes = unsafe { core::slice::from_raw_parts(base as *const u8, len as usize) };
        while !bytes.is_empty() {
            let take = bytes.len().min(record.len() - used);
            record[used..used + take].copy_from_slice(&bytes[..take]);
            used += take;
            bytes = &bytes[take..];
            if used == record.len() {
                let sent = files::write_out(fd as i64, &record);
                if sent < 0 {
                    return if total > 0 { total } else { sent };
                }
                used = 0;
            }
        }
        total += len as i64;
    }
    if used > 0 {
        let sent = files::write_out(fd as i64, &record[..used]);
        if sent < 0 {
            return if total > 0 { total } else { sent };
        }
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

/// SH7-d1: Linux `munmap` over any page range, by the kernel's page-range VmarUnmap. A range
/// with nothing mapped succeeds, as on Linux; only an unaligned or empty one is EINVAL.
fn munmap(addr: u64, len: u64) -> i64 {
    if addr % PAGE != 0 || len == 0 {
        return -errno::EINVAL;
    }
    let _ = vmar_unmap(addr, len.next_multiple_of(PAGE));
    0
}

/// SH7-d1: Linux `mprotect` over any page range, by the kernel's page-range VmarProtect. A
/// range with a hole is ENOMEM (the kernel changes nothing then), an unaligned one EINVAL.
fn mprotect(addr: u64, len: u64, prot: u64) -> i64 {
    if addr % PAGE != 0 || prot & !(PROT_READ | PROT_WRITE | PROT_EXEC) != 0 {
        return -errno::EINVAL;
    }
    if len == 0 {
        return 0;
    }
    match vmar_protect(addr, len.next_multiple_of(PAGE), prot) as u32 as i32 {
        0 => 0,
        status if status == kumo_abi::Errno::NoMemory as i32 => -errno::ENOMEM,
        _ => -errno::EINVAL,
    }
}

/// Linux `mmap`: anonymous memory, or (SH7-d2) a private copy of a file range. A file mapping
/// reads the range into a fresh VMO through the vfs session (bytes past the end of the file are
/// zero), so MAP_PRIVATE is exact. There is no shared page cache, so a writable MAP_SHARED
/// mapping is refused (EACCES, as for a read-only open on Linux); a read-only shared one is the
/// same copy.
fn mmap(addr: u64, len: u64, prot: u64, flags: u64, fd: i64, offset: u64) -> i64 {
    if len == 0 || offset % PAGE != 0 {
        return -errno::EINVAL;
    }
    let file = if flags & MAP_ANONYMOUS == 0 {
        match files::file_of(fd) {
            Ok(file) => Some(file),
            Err(e) => return e,
        }
    } else {
        None
    };
    if file.is_some() && flags & MAP_SHARED != 0 && prot & PROT_WRITE != 0 {
        return -errno::EACCES;
    }
    let len = len.next_multiple_of(PAGE);
    let Some(vmo) = vmo(len) else {
        return -errno::ENOMEM;
    };
    if let Some((handle, size)) = file {
        let view = match vmar_map_anywhere(vmo, len, MAP_R | MAP_W) {
            (0, view) if view != 0 => view,
            _ => {
                let _ = kumo_rt::handle_close(vmo);
                return -errno::ENOMEM;
            }
        };
        let want = len.min(size.saturating_sub(offset));
        // SAFETY: `view` is this fresh, writable mapping of `len >= want` bytes.
        let out = unsafe { core::slice::from_raw_parts_mut(view as *mut u8, want as usize) };
        let read = files::vfs_pread(handle, size, offset, out);
        let _ = vmar_unmap(view, len);
        if read < 0 || read as u64 != want {
            let _ = kumo_rt::handle_close(vmo);
            return if read < 0 { read } else { -errno::EIO };
        }
    }
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
    // MAP_FIXED must land at `addr`, replacing whatever is mapped there (SH7-d1: unmap the page
    // range, then map); without it `addr` is only a hint (Linux semantics).
    if flags & MAP_FIXED != 0 {
        if addr % PAGE != 0 {
            let _ = kumo_rt::handle_close(vmo);
            return -errno::EINVAL;
        }
        let _ = vmar_unmap(addr, len);
    }
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

/// The argv wire VMOs the launcher sends hold at most this many bytes.
const ARGV_BYTES: usize = 256;
static mut ARGV_BUF: [u8; ARGV_BYTES] = [0; ARGV_BYTES];
static mut ENV_BUF: [u8; ARGV_BYTES] = [0; ARGV_BYTES];

#[unsafe(no_mangle)]
extern "C" fn main(
    bootstrap: u64,
    _a2: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
    _a7: u64,
    _a8: u64,
) -> ! {
    // SH7-d2: the standard startup protocol, like every other program Sora launches.
    let startup = kumo_rt::startup(Handle(bootstrap as u32));
    files::init(
        startup.fs.unwrap_or(Handle(0)),
        startup.stdin.unwrap_or(Handle(0)),
        startup.stdout.unwrap_or(Handle(0)),
    );
    if startup.fs.is_none() {
        fail("no filesystem grant; launch Linux programs with `linux <path>`");
    }
    // SAFETY: written once here, before the program exists; read-only afterwards.
    let argv_buf = unsafe { &mut *core::ptr::addr_of_mut!(ARGV_BUF) };
    let env_buf = unsafe { &mut *core::ptr::addr_of_mut!(ENV_BUF) };
    let Some(argv_vmo) = startup.argv else {
        fail("no argv");
    };
    if vmo_read(argv_vmo, 0, argv_buf.as_mut_ptr(), argv_buf.len()) != 0 {
        fail("argv read failed");
    }
    if let Some(env_vmo) = startup.env {
        if vmo_read(env_vmo, 0, env_buf.as_mut_ptr(), env_buf.len()) != 0 {
            env_buf.fill(0);
        }
    }
    // argv[0] is the shim itself; argv[1] is the program, which is also the program's argv[0].
    let mut linux_argv: [&[u8]; 16] = [b""; 16];
    let mut argc = 0usize;
    for arg in kumo_abi::unpack_argv(&argv_buf[..])
        .skip(1)
        .take(linux_argv.len())
    {
        linux_argv[argc] = arg;
        argc += 1;
    }
    if argc == 0 {
        fail("usage: linux <path> [args...]");
    }
    let mut envp: [&[u8]; 16] = [b""; 16];
    let mut envc = 0usize;
    for pair in kumo_abi::unpack_argv(&env_buf[..]).take(envp.len()) {
        envp[envc] = pair;
        envc += 1;
    }

    // execve: the program by path, then the interpreter its PT_INTERP names.
    let program = read_image(linux_argv[0]);
    let header = match elf::parse_linux_image(program.bytes, Machine::Aarch64) {
        Ok(header) => header,
        Err(_) => fail_path("not an aarch64 Linux executable", linux_argv[0]),
    };
    let interp = match elf::interpreter(program.bytes, &header) {
        Ok(Some(path)) => Some(read_image(path)),
        Ok(None) => None,
        Err(_) => fail_path("bad PT_INTERP", linux_argv[0]),
    };
    let loaded = load(program.bytes, interp.as_ref().map(|image| image.bytes));
    program.release();
    if let Some(interp) = interp {
        interp.release();
    }
    let program_state = unsafe { &mut *core::ptr::addr_of_mut!(PROGRAM) };
    program_state.brk_end = loaded.image_end.next_multiple_of(PAGE);

    let sp = build_stack(&loaded, &linux_argv[..argc], &envp[..envc]);

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

/// A whole file read into a private mapping for the loader.
struct Image {
    bytes: &'static [u8],
    map_at: u64,
    map_len: u64,
}

impl Image {
    fn release(self) {
        let _ = vmar_unmap(self.map_at, self.map_len);
    }
}

/// Read the file at absolute `path` through the vfs session, whole.
fn read_image(path: &[u8]) -> Image {
    let (handle, size) = match files::open_path(path) {
        Ok(open) => open,
        Err(_) => fail_path("cannot open", path),
    };
    if size == 0 {
        fail_path("empty file", path);
    }
    let map_len = size.next_multiple_of(PAGE);
    let Some(vmo) = vmo(map_len) else {
        fail_path("no memory to read", path);
    };
    let map_at = match vmar_map_anywhere(vmo, map_len, MAP_R | MAP_W) {
        (0, at) if at != 0 => at,
        _ => fail_path("cannot map to read", path),
    };
    let _ = kumo_rt::handle_close(vmo);
    // SAFETY: `map_at` is this fresh mapping of `map_len >= size` bytes.
    let out = unsafe { core::slice::from_raw_parts_mut(map_at as *mut u8, size as usize) };
    if files::vfs_pread(handle, size, 0, out) != size as i64 {
        fail_path("short read", path);
    }
    files::close_handle(handle);
    Image {
        bytes: out,
        map_at,
        map_len,
    }
}

fn fail_path(what: &str, path: &[u8]) -> ! {
    log(format_args!(
        "persona-linux: {}: {}\n",
        what,
        core::str::from_utf8(path).unwrap_or("<path>")
    ));
    process_exit(127)
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

/// The Linux initial stack (SysV aarch64): argc, argv[], NULL, envp[], NULL, auxv, with the strings
/// and AT_RANDOM's 16 bytes above them. Returns the 16-byte-aligned SP that points at argc.
fn build_stack(loaded: &Loaded, argv: &[&[u8]], envp: &[&[u8]]) -> u64 {
    let Some(vmo) = vmo(LINUX_STACK_SIZE) else {
        fail("no memory for the stack");
    };
    let base = LINUX_STACK_TOP - LINUX_STACK_SIZE;
    if vmar_map(Handle(0), vmo, 0, base, LINUX_STACK_SIZE, MAP_R | MAP_W) != 0 {
        fail("cannot map the stack");
    }
    let _ = kumo_rt::handle_close(vmo);

    // Strings at the top, each NUL-terminated (the stack is fresh, so zeroed).
    let mut top = LINUX_STACK_TOP - 16;
    let mut place = |s: &[u8]| -> u64 {
        top -= s.len() as u64 + 1;
        // SAFETY: inside the stack mapped just above.
        unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), top as *mut u8, s.len()) };
        top
    };
    let mut argv_at = [0u64; 16];
    for (slot, arg) in argv_at.iter_mut().zip(argv) {
        *slot = place(arg);
    }
    let mut envp_at = [0u64; 16];
    for (slot, pair) in envp_at.iter_mut().zip(envp) {
        *slot = place(pair);
    }
    let execfn = argv_at[0];
    top = (top - 32) & !15;
    let random_at = top;
    let seed = kumo_rt::clock_get();
    let random = [seed ^ 0x9e37_79b9_7f4a_7c15, seed.rotate_left(29) ^ execfn];
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
        (AT_EXECFN, execfn),
        (AT_NULL, 0),
    ];
    let words = 1 + argv.len() + 1 + envp.len() + 1 + auxv.len() * 2;
    let sp = (top - words as u64 * 8) & !15;
    let mut at = sp;
    let mut push = |value: u64| {
        unsafe { (at as *mut u64).write(value) };
        at += 8;
    };
    push(argv.len() as u64);
    for &arg in &argv_at[..argv.len()] {
        push(arg);
    }
    push(0);
    for &pair in &envp_at[..envp.len()] {
        push(pair);
    }
    push(0);
    for (key, value) in auxv {
        push(key);
        push(value);
    }
    sp
}
