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

// The serial record interleaves sources per write; every acceptance line goes out as ONE
// formatted LogWrite entry.
fn ring_line(args: core::fmt::Arguments<'_>) {
    use core::fmt::Write as _;
    struct RingBuf {
        buf: [u8; 200],
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
        buf: [0u8; 200],
        at: 0,
    };
    let _ = write!(line, "std: net {}", args);
    let _ = write!(line, "\n");
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &line.buf[..line.at]);
}

macro_rules! netlog {
    ($($arg:tt)*) => { ring_line(format_args!($($arg)*)) };
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
    if std::env::args().nth(1).as_deref() == Some("net") {
        net_campaign();
        return;
    }
    gate_ib();
}

/// The stream generator, shared by sender and verifier (the C smoke's contract).
fn stream_byte(index: usize) -> u8 {
    ((index * 31 + 7) & 0xFF) as u8
}

/// NET14: std::net over the PAL — UdpSocket and TcpListener/TcpStream on loopback, with
/// try_clone, nonblocking/read-timeout readiness, and a server-failure wake.
///
/// The child's stack is 16 KiB, so nothing here may hold a whole stream: sends generate
/// bytes chunk by chunk and the echo drains what it just read.
#[inline(never)]
fn net_campaign() {
    use std::io::{ErrorKind, Read, Write};
    use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
    use std::time::Duration;

    let mut failures = 0u32;
    macro_rules! check {
        ($cond:expr, $msg:literal) => {
            if $cond {
                netlog!("{} -> OK", $msg);
            } else {
                failures += 1;
                netlog!("{} -> FAIL", $msg);
            }
        };
    }

    // Numeric SocketAddrs throughout: the &str path goes through getaddrinfo, which the
    // PAL does not provide (PLAN/020's scope is numeric IPv4).
    let addr_a = SocketAddr::from(([127, 0, 0, 1], 7811));
    let addr_b = SocketAddr::from(([127, 0, 0, 1], 7812));
    // UDP: exact bytes both ways, source address, the size bounds, WouldBlock, timeout.
    let (a, b) = match (UdpSocket::bind(addr_a), UdpSocket::bind(addr_b)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            netlog!("udp bind -> FAIL {:?}", e);
            return;
        }
    };
    let mut got = [0u8; 64];
    let sent = a.send_to(b"kumo-std-udp", addr_b);
    let recv = b.recv_from(&mut got);
    let exact = matches!((sent, recv), (Ok(12), Ok((12, src)))
        if &got[..12] == b"kumo-std-udp"
            && src.port() == 7811
            && src.ip().is_loopback());
    check!(exact, "udp datagram+source");
    let zero = a.send_to(&[], addr_b).is_ok()
        && matches!(b.recv_from(&mut got), Ok((0, _)));
    check!(zero, "udp zero-length");
    let mut big = [0u8; 1472];
    for (i, byte) in big.iter_mut().enumerate() {
        *byte = i as u8;
    }
    // (big_got below is one chunk, not a stream.)
    let mut big_got = [0u8; 1472];
    let big_ok = a.send_to(&big, addr_b).is_ok()
        && matches!(b.recv_from(&mut big_got), Ok((1472, _)))
        && big_got == big;
    check!(big_ok, "udp 1472-byte bound");
    b.set_nonblocking(true).ok();
    let would_block =
        matches!(b.recv_from(&mut got), Err(ref e) if e.kind() == ErrorKind::WouldBlock);
    check!(would_block, "udp nonblocking WouldBlock");
    b.set_nonblocking(false).ok();
    b.set_read_timeout(Some(Duration::from_millis(300))).ok();
    let timed = matches!(
        b.recv_from(&mut got),
        Err(ref e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut
    );
    check!(timed, "udp read timeout");
    b.set_read_timeout(None).ok();
    let recover = a.send_to(b"after", addr_b).is_ok()
        && matches!(b.recv_from(&mut got), Ok((5, _))) && &got[..5] == b"after";
    check!(recover, "udp after timeout");

    // TCP: pair, 12000-byte echo pump, shutdown/EOF, try_clone, server-failure wake.
    let addr_tcp = SocketAddr::from(([127, 0, 0, 1], 7813));
    let listener = match TcpListener::bind(addr_tcp) {
        Ok(l) => l,
        Err(e) => {
            failures += 1;
            netlog!("tcp listener -> FAIL {:?} campaign failed={}", e, failures);
            return;
        }
    };
    let client = match TcpStream::connect(addr_tcp) {
        Ok(c) => c,
        Err(e) => {
            failures += 1;
            netlog!("tcp connect -> FAIL {:?} campaign failed={}", e, failures);
            return;
        }
    };
    let mut client = match client.try_clone() {
        Ok(c) => c,
        Err(_) => {
            failures += 1;
            netlog!("tcp try_clone -> FAIL campaign failed={}", failures);
            return;
        }
    };
    let mut server = match listener.accept() {
        Ok((s, peer)) => {
            check!(peer.port() != 0, "tcp accept peer");
            s
        }
        Err(e) => {
            failures += 1;
            netlog!("tcp accept -> FAIL {:?} campaign failed={}", e, failures);
            return;
        }
    };
    client.set_nonblocking(true).ok();
    server.set_nonblocking(true).ok();
    const TOTAL: usize = 12_000;
    const CHUNK: usize = 2048;
    let mut send_buf = [0u8; CHUNK];
    let mut srv_buf = [0u8; CHUNK];
    let mut cli_buf = [0u8; CHUNK];
    let (mut sent, mut echoed, mut got_total) = (0usize, 0usize, 0usize);
    // The echo buffer holds exactly one read; these track that chunk, not the stream.
    let (mut srv_pending, mut srv_written) = (0usize, 0usize);
    let (mut client_shut, mut server_shut, mut server_eof, mut client_eof) =
        (false, false, false, false);
    let mut exact_stream = true;
    for _ in 0..400_000 {
        if client_eof && server_eof && got_total == TOTAL {
            break;
        }
        if sent < TOTAL {
            let n = (TOTAL - sent).min(CHUNK);
            for (i, byte) in send_buf[..n].iter_mut().enumerate() {
                *byte = stream_byte(sent + i);
            }
            match client.write(&send_buf[..n]) {
                Ok(0) => {}
                Ok(w) => sent += w,
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(_) => {
                    exact_stream = false;
                    break;
                }
            }
        } else if !client_shut {
            client_shut = client.shutdown(Shutdown::Write).is_ok();
        }
        if srv_written == srv_pending {
            match server.read(&mut srv_buf) {
                Ok(0) => server_eof = true,
                Ok(n) => {
                    srv_pending = n;
                    srv_written = 0;
                    echoed += n;
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(_) => {
                    exact_stream = false;
                    break;
                }
            }
        }
        while srv_written < srv_pending {
            match server.write(&srv_buf[srv_written..srv_pending]) {
                Ok(0) => break,
                Ok(w) => srv_written += w,
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => {
                    exact_stream = false;
                    break;
                }
            }
        }
        if server_eof && srv_written == srv_pending && !server_shut {
            server_shut = server.shutdown(Shutdown::Write).is_ok();
        }
        match client.read(&mut cli_buf) {
            Ok(0) => client_eof = true,
            Ok(n) => {
                if got_total + n > TOTAL {
                    exact_stream = false;
                    break;
                }
                for (i, byte) in cli_buf[..n].iter().enumerate() {
                    if *byte != stream_byte(got_total + i) {
                        exact_stream = false;
                        break;
                    }
                }
                if !exact_stream {
                    break;
                }
                got_total += n;
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(_) => {
                exact_stream = false;
                break;
            }
        }
    }
    check!(
        exact_stream && client_shut && server_eof && client_eof && got_total == TOTAL,
        "tcp 12000-byte duplex + shutdown + EOF"
    );

    // Server failure wakes the client: closing the peer ends a blocked read promptly.
    let addr_wake = SocketAddr::from(([127, 0, 0, 1], 7814));
    let pair = TcpListener::bind(addr_wake).ok().and_then(|l| {
        let c = TcpStream::connect(addr_wake).ok()?;
        let (srv, _) = l.accept().ok()?;
        Some((c, srv))
    });
    if let Some((mut wake_client, wake_server)) = pair {
        wake_client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .ok();
        drop(wake_server);
        let woke = matches!(wake_client.read(&mut cli_buf), Ok(0));
        check!(woke, "server failure wake");
    } else {
        failures += 1;
        netlog!("server failure wake -> FAIL campaign failed={}", failures);
        return;
    }

    netlog!("campaign failed={}", failures);
    if failures == 0 {
        netlog!("loopback -> OK");
    }
}

fn gate_ib() {
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
