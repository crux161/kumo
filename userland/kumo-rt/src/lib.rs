#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use kumo_abi::{Handle, Status};

pub mod heap;
pub mod sys;

pub use sys::*;

pub trait Server {
    fn name(&self) -> &'static str;
    fn dispatch(&mut self, channel: Handle, message: &[u8]) -> Status;
}

pub fn run_one<S: Server>(server: &mut S, channel: Handle, message: &[u8]) -> Status {
    server.dispatch(channel, message)
}

pub fn init() {
    // KumoHeap auto-initializes on first allocation, so this is a no-op marker
    // for future explicit initialization if we switch to VMO-backed heaps.
}

/// PLAN/016 C2: the fixed-width markers userland boot milestones carry, so the boot screen
/// reads as a status list. The kernel POST table keeps its own `NAME Check … OK` form.
/// — TIMBERDOODLE
pub const BOOT_OK: &[u8] = b"[  OK  ] ";
/// A failed milestone; the same nine-column marker width as [`BOOT_OK`].
pub const BOOT_FAILED: &[u8] = b"[FAILED] ";

/// Compose `tag` plus `parts` into `out`, truncating at the end. Returns the used length.
fn boot_line(out: &mut [u8], tag: &[u8], parts: &[&[u8]]) -> usize {
    let mut n = tag.len().min(out.len());
    out[..n].copy_from_slice(&tag[..n]);
    for part in parts {
        let take = part.len().min(out.len() - n);
        out[n..n + take].copy_from_slice(&part[..take]);
        n += take;
    }
    n
}

/// Write one boot-status milestone at NOTICE: [`BOOT_OK`] then the concatenated parts.
pub fn boot_ok(parts: &[&[u8]]) {
    let mut line = [0u8; 256];
    let n = boot_line(&mut line, BOOT_OK, parts);
    let _ = log_write(kumo_abi::sys::log_level::NOTICE, &line[..n]);
}

/// The formatted form of [`boot_ok`] for `format_args!` callers.
pub fn boot_ok_fmt(args: core::fmt::Arguments<'_>) {
    use core::fmt::Write;
    let mut line = BootLine::new();
    let _ = write!(line, "{}", args);
    let _ = log_write(kumo_abi::sys::log_level::NOTICE, line.bytes());
}

/// [`BOOT_OK`]-tagged assembly for the formatted callers; `core::fmt::Write`-shaped.
struct BootLine {
    buf: [u8; 256],
    len: usize,
}

impl BootLine {
    fn new() -> Self {
        let mut line = Self {
            buf: [0u8; 256],
            len: 0,
        };
        line.len = boot_line(&mut line.buf, BOOT_OK, &[]);
        line
    }

    fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl core::fmt::Write for BootLine {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let take = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

#[cfg(all(not(test), target_os = "none"))]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    // A panic is a slice-ending failure, and a silent exit(1) turns it into an anonymous
    // ABEND in the TOWER report. Name it on the kernel log before ending, best-effort. — TIMBERDOODLE
    use core::fmt::Write;
    struct Log;
    impl Write for Log {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let _ = sys::log_write(kumo_abi::sys::log_level::ERR, s.as_bytes());
            Ok(())
        }
    }
    let mut log = Log;
    let _ = write!(
        log,
        "PANIC: {}
",
        info.message()
    );
    if let Some(location) = info.location() {
        let _ = write!(
            log,
            "PANIC AT: {}:{}
",
            location.file(),
            location.line()
        );
    }
    sys::process_exit(1);
}

#[macro_export]
macro_rules! entry {
    ($path:ident) => {
        #[cfg(target_arch = "aarch64")]
        core::arch::global_asm!(
            ".section .text._start, \"ax\"",
            ".global _start",
            "_start:",
            concat!("  bl  ", stringify!($path)),
            "1: b 1b",
        );

        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        core::arch::global_asm!(
            ".section .text._start, \"ax\"",
            ".global _start",
            "_start:",
            // The architecture-neutral program ABI supplies eight bootstrap words. AMD64
            // carries the first six in registers and finds the final two on the stack.
            "  subq $16, %rsp",
            "  movq $0, 0(%rsp)",
            "  movq $0, 8(%rsp)",
            concat!("  call ", stringify!($path)),
            "1: jmp 1b",
            options(att_syntax),
        );
    };
}

#[global_allocator]
static ALLOC: heap::KumoHeap = heap::KumoHeap::empty();

/// How many regions back this process's heap. `1` is the bootstrap floor alone; anything more means
/// the growth path ran, which is the only thing that distinguishes a heap that *can* grow from one
/// that merely says so.
pub fn heap_region_count() -> usize {
    ALLOC.region_count()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoServer;

    impl Server for EchoServer {
        fn name(&self) -> &'static str {
            "echo"
        }

        fn dispatch(&mut self, _channel: Handle, _message: &[u8]) -> Status {
            kumo_abi::Errno::Ok.status()
        }
    }

    #[test]
    fn boot_markers_are_one_nine_column_width() {
        assert_eq!(BOOT_OK, b"[  OK  ] ");
        assert_eq!(BOOT_FAILED, b"[FAILED] ");
        assert_eq!(BOOT_OK.len(), BOOT_FAILED.len());
    }

    #[test]
    fn boot_lines_tag_and_truncate() {
        let mut line = [0u8; 16];
        let n = boot_line(&mut line, BOOT_OK, &[b"ab", b"cd\n"]);
        assert_eq!(&line[..n], b"[  OK  ] abcd\n");
        let mut tiny = [0u8; 12];
        let n = boot_line(&mut tiny, BOOT_OK, &[b"0123456789"]);
        assert_eq!(n, 12);
        assert_eq!(&tiny[..12], b"[  OK  ] 012");
    }

    #[test]
    fn formatted_boot_lines_keep_the_tag_and_cap_at_256() {
        use core::fmt::Write;
        let mut line = BootLine::new();
        write!(line, "drv-gpu: online {}", 7).unwrap();
        assert_eq!(line.bytes(), b"[  OK  ] drv-gpu: online 7");
        let mut long = BootLine::new();
        for _ in 0..400 {
            write!(long, "x").unwrap();
        }
        assert_eq!(long.bytes().len(), 256);
    }

    #[test]
    fn dispatches_one_message() {
        let mut server = EchoServer;
        assert_eq!(
            run_one(&mut server, Handle(0), b"hello"),
            kumo_abi::Errno::Ok.status()
        );
    }
}
