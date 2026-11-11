//j868
//j869
//j870
//j871
//j873
#![feature(restricted_std)]
//! The gate I-b program: std::fs, std::io, std::collections, and std::time over the kumo
//! PAL, with every acceptance line mirrored to the kernel-log ring (the metal serial
//! records only the ring; see the J851 harness law). — TIMBERDOODLE

use std::collections::HashMap;
use std::io::Write;
use std::time::Instant;

include!("../std-rt/glue.rs");



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

/// SH2: the env proof's ring record, one formatted entry under "std: env".
fn env_line(args: core::fmt::Arguments<'_>) {
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
    let _ = write!(line, "std: env {}", args);
    let _ = write!(line, "\n");
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &line.buf[..line.at]);
}

macro_rules! envlog {
    ($($arg:tt)*) => { env_line(format_args!($($arg)*)) };
}

/// SH3-d: the std::fs metadata proof's ring record, one formatted entry under
/// "std: sh3".
fn sh3_line(args: core::fmt::Arguments<'_>) {
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
    let _ = write!(line, "std: sh3 {}", args);
    let _ = write!(line, "\n");
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &line.buf[..line.at]);
}

macro_rules! sh3log {
    ($($arg:tt)*) => { sh3_line(format_args!($($arg)*)) };
}

/// SH5: the threads proof's ring record, one formatted entry under "std: sh5".
fn sh5_line(args: core::fmt::Arguments<'_>) {
    let text = format!("std: sh5 {}\n", args);
    let bytes = text.as_bytes();
    let _ = kumo_rt::log_write(
        kumo_abi::sys::log_level::NOTICE,
        &bytes[..bytes.len().min(200)],
    );
}

macro_rules! sh5log {
    ($($arg:tt)*) => { sh5_line(format_args!($($arg)*)) };
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
    // The mode word is the launcher's grant word: only `net` opens a netstack
    // session (slots[1] == "net"), so the wire campaign rides extra arguments.
    match std::env::args().nth(1).as_deref() {
        Some("env") => {
            env_probe();
            return;
        }
        Some("fsmeta") => {
            sh3_probe();
            return;
        }
        Some("mediasuite") => {
            media_suite();
            return;
        }
        Some("mediacheck") => {
            media_check();
            return;
        }
        Some("lockhold") => {
            let args: Vec<String> = std::env::args().collect();
            lock_hold(&args);
            return;
        }
        Some("locktry") => {
            let args: Vec<String> = std::env::args().collect();
            lock_try(&args);
            return;
        }
        // SH4: the std::process::Command proof (scripts/spawn-smoke.py drives spawnparent;
        // the other three are the children it spawns from the image's SMOKE.ELF copy).
        Some("spawnparent") => {
            spawn_parent();
            return;
        }
        Some("spawnchild") => {
            use std::io::{Read as _, Write as _};
            let mut fed = String::new();
            let _ = std::io::stdin().read_to_string(&mut fed);
            print!("child says hi: {}", fed.to_uppercase());
            eprint!("child stderr\n");
            std::io::stdout().flush().ok();
            std::io::stderr().flush().ok();
            std::process::exit(7);
        }
        Some("spawnspin") => {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while std::time::Instant::now() < deadline {}
            std::process::exit(0);
        }
        // SH5: the std::thread proof (scripts/thread-smoke.py drives `threads`; `threadexit`
        // is the child it spawns from SMOKE.ELF to prove exit() from a thread is a group exit).
        Some("threads") => {
            threads_suite();
            return;
        }
        Some("threadexit") => {
            let worker = std::thread::spawn(|| {
                std::process::exit(5);
            });
            // The main thread parks in join; the worker's exit ends both.
            let _ = worker.join();
            std::process::exit(9);
        }
        Some("spawnnogrant") => {
            use std::io::Write as _;
            // No filesystem grant travels with a spawn: the file this child was loaded
            // from is not openable by the child.
            match std::fs::File::open("/SMOKE.ELF") {
                Err(_) => print!("nogrant: open refused\n"),
                Ok(_) => print!("nogrant: open succeeded\n"),
            }
            std::io::stdout().flush().ok();
            std::process::exit(0);
        }
        Some("net") => {
            let args: Vec<String> = std::env::args().collect();
            if args.len() >= 5 {
                net_wire(&args);
            } else if args.len() == 4 && args[2] == "recover-wait" {
                recover_wait(&args);
            } else if args.len() == 4 && args[2] == "recover-fresh" {
                recover_fresh(&args);
            } else if args.len() == 4 {
                net_name(&args);
            } else {
                net_campaign();
            }
            return;
        }
        _ => {}
    }
    gate_ib();
}

/// SH2: the environment proof. A granted environment must be visible through std
/// (`std::env`, i.e. relibc's environ), a missing grant must be empty rather than
/// ambient, and setenv/unsetenv must round-trip. The ring records are the evidence.
#[inline(never)]
fn env_probe() {
    let path = std::env::var("PATH");
    let home = std::env::var("HOME");
    let total = std::env::vars().count();
    if path.is_ok() || home.is_ok() {
        match &path {
            Ok(value) => envlog!("PATH={} -> OK", value),
            Err(_) => envlog!("PATH -> FAIL"),
        }
        match &home {
            Ok(value) => envlog!("HOME={} -> OK", value),
            Err(_) => envlog!("HOME -> FAIL"),
        }
        envlog!("granted ({} vars) -> OK", total);
    } else if total == 0 {
        envlog!("empty (no grant) -> OK");
    } else {
        envlog!("empty (no grant) -> FAIL vars={}", total);
    }
    // SAFETY: single-threaded probe; no other thread reads the environment.
    unsafe { std::env::set_var("KUMO_SET", "1") };
    let set = std::env::var("KUMO_SET").ok().as_deref() == Some("1");
    unsafe { std::env::remove_var("KUMO_SET") };
    let removed = std::env::var("KUMO_SET").is_err();
    if set && removed {
        envlog!("set+remove -> OK");
    } else {
        envlog!("set+remove -> FAIL");
    }
}

/// The pre-wire retry, mirroring the NET13a C smoke's proven loop: autoexec runs
/// before the NIC wave, so ENETUNREACH and EAGAIN are retried under one bounded
/// budget with no sleep (each attempt is an IPC round trip, so the budget spans the
/// wave), then reported.
fn wire_retry(what: &str, mut attempt: impl FnMut() -> std::io::Result<usize>, want: usize) -> bool {
    // The wire attach lands as the service wave completes, seconds to tens of seconds
    // after autoexec begins. The budget is wall-clock: each attempt is an IPC round
    // trip whose cost the caller cannot know (attempt counts cost 1 ms in one phase
    // and 29 ms in the C smoke's cadence).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        match attempt() {
            Ok(n) if n == want => return true,
            Ok(_) => return false,
            Err(ref e) => {
                use std::io::ErrorKind;
                let transient = e.kind() == ErrorKind::WouldBlock
                    || matches!(e.kind(), ErrorKind::NetworkUnreachable | ErrorKind::NetworkDown)
                    || matches!(e.raw_os_error(), Some(101) | Some(11));
                if !transient || std::time::Instant::now() >= deadline {
                    netlog!("wire {} -> FAIL {:?}", what, e);
                    return false;
                }
            }
        }
    }
}

/// NET15: the same std programs exchange exact bytes with the real peer through AX.
/// argv: `netwire <peer-ip> <udp-port> <tcp-port>`.
#[inline(never)]
fn net_wire(args: &[String]) {
    use std::io::ErrorKind;
    use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
    use std::time::Duration;

    let ip = args.get(2).and_then(|s| s.parse::<Ipv4Addr>().ok());
    let udp_port = args.get(3).and_then(|s| s.parse::<u16>().ok());
    let tcp_port = args.get(4).and_then(|s| s.parse::<u16>().ok());
    let (Some(ip), Some(udp_port), Some(tcp_port)) = (ip, udp_port, tcp_port) else {
        netlog!("wire usage -> FAIL");
        return;
    };
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

    // The shared contract with scripts/net15-peer.py: index 0 is the zero-length
    // datagram, index 1 the 1472-byte maximum, every other index the prefix plus
    // index % 16 fill bytes.
    const COUNT: usize = 24;
    fn payload(buf: &mut [u8], side: &[u8], index: usize) -> usize {
        if index == 0 {
            return 0;
        }
        let mut at = 0;
        for part in [side, &b"-std-"[..]] {
            buf[at..at + part.len()].copy_from_slice(part);
            at += part.len();
        }
        let mut digits = [0u8; 4];
        let mut n = 0;
        let mut value = index;
        while value > 0 {
            digits[n] = b'0' + (value % 10) as u8;
            value /= 10;
            n += 1;
        }
        while n > 0 {
            n -= 1;
            buf[at] = digits[n];
            at += 1;
        }
        buf[at] = b'-';
        at += 1;
        if index == 1 {
            while at < 1472 {
                buf[at] = 0;
                at += 1;
            }
        } else {
            for _ in 0..(index % 16) {
                buf[at] = b'x';
                at += 1;
            }
        }
        at
    }

    let sock = match UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0))) {
        Ok(s) => s,
        Err(e) => {
            netlog!("wire udp bind -> FAIL {:?}", e);
            return;
        }
    };
    let peer_udp = SocketAddr::from((ip, udp_port));
    let mut tx = [0u8; 1472];
    let mut sent_ok = true;
    for i in 0..COUNT {
        let len = payload(&mut tx, b"kumo", i);
        let ok = wire_retry("udp send", || sock.send_to(&tx[..len], peer_udp), len);
        if !ok {
            sent_ok = false;
            break;
        }
    }
    check!(sent_ok, "udp 24 datagrams sent");

    // Echo the peer's numbered datagrams back to their source, byte-exact.
    let mut rx = [0u8; 1472];
    let mut echoed = 0usize;
    let mut echo_ok = true;
    sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
    while echoed < COUNT {
        let (n, src) = match sock.recv_from(&mut rx) {
            Ok(pair) => pair,
            Err(_) => {
                echo_ok = false;
                break;
            }
        };
        let mut want = [0u8; 1472];
        let wlen = payload(&mut want, b"peer", echoed);
        if n != wlen || rx[..n] != want[..wlen] {
            echo_ok = false;
            break;
        }
        if sock.send_to(&rx[..n], src).is_err() {
            echo_ok = false;
            break;
        }
        echoed += 1;
    }
    check!(echo_ok && echoed == COUNT, "udp 24 peer datagrams echoed exact");

    // TCP: the same generator duplex as the loopback campaign, against the real peer.
    let peer_tcp = SocketAddr::from((ip, tcp_port));
    let mut connected = None;
    let mut attempts = 0u32;
    loop {
        match TcpStream::connect(peer_tcp) {
            Ok(c) => {
                connected = Some(c);
                break;
            }
            Err(ref e)
                if (e.kind() == ErrorKind::WouldBlock
                    || matches!(
                        e.kind(),
                        ErrorKind::NetworkUnreachable | ErrorKind::NetworkDown
                    )
                    || matches!(e.raw_os_error(), Some(101) | Some(11)))
                    && attempts < 1500 =>
            {
                attempts += 1;
            }
            Err(e) => {
                netlog!("wire tcp connect -> FAIL {:?}", e);
                return;
            }
        }
    }
    let stream = connected.expect("connected above");
    let (exact, sent, got) = tcp_duplex(stream);
    check!(
        exact && sent == 12_000 && got == 12_000,
        "tcp 12000-byte duplex + shutdown + EOF"
    );

    // NET15: one background job carries both external phases. The launcher's `wait`
    // drain stalls the autoexec before ttyd when a background program has run, so a
    // second bg job chained through `wait` can never start; the hostname phase rides
    // this job's trailing arguments instead.
    let mut name_failures = 0u32;
    if let (Some(name), Some(name_port)) = (
        args.get(5),
        args.get(6).and_then(|s| s.parse::<u16>().ok()),
    ) {
        let mut connected = None;
        let mut last = None;
        for _ in 0..600 {
            match TcpStream::connect((name.as_str(), name_port)) {
                Ok(c) => {
                    connected = Some(c);
                    break;
                }
                Err(e) => {
                    let transient = matches!(e.raw_os_error(), Some(101) | Some(11) | Some(111));
                    last = Some(e);
                    if !transient {
                        break;
                    }
                }
            }
        }
        match connected {
            Some(stream) => {
                netlog!("name resolve+connect -> OK");
                let (exact, sent, got) = tcp_duplex(stream);
                if exact && sent == 12_000 && got == 12_000 {
                    netlog!("name tcp duplex exact -> OK");
                } else {
                    name_failures += 1;
                    netlog!("name tcp duplex -> FAIL sent={} got={}", sent, got);
                }
            }
            None => {
                name_failures += 1;
                netlog!("name resolve+connect -> FAIL {:?}", last);
            }
        }
    }
    netlog!("name campaign failed={}", name_failures);
    failures += name_failures;

    netlog!("wire campaign failed={}", failures);
    if failures == 0 {
        netlog!("ax -> OK");
    }
}

/// The stream generator, shared by sender and verifier (the C smoke's contract).
/// The loopback campaign's generator duplex, shared by every wire mode: pump 12000
/// generator bytes up, read the peer's 12000 back exact, half-close, and read EOF.
#[inline(never)]
fn tcp_duplex(stream: std::net::TcpStream) -> (bool, usize, usize) {
    use std::io::{ErrorKind, Read, Write};
    use std::net::Shutdown;

    let mut client = match stream.try_clone() {
        Ok(c) => c,
        Err(e) => {
            netlog!("tcp try_clone -> FAIL {:?}", e);
            return (false, 0, 0);
        }
    };
    client.set_nonblocking(true).ok();
    let mut buf = [0u8; 2048];
    let (mut sent, mut got) = (0usize, 0usize);
    let mut shut = false;
    let mut eof = false;
    let mut exact = true;
    for _ in 0..400_000 {
        if got == 12_000 && shut && eof {
            break;
        }
        if sent < 12_000 {
            let n = (12_000 - sent).min(2048);
            for (i, byte) in buf[..n].iter_mut().enumerate() {
                *byte = stream_byte(sent + i);
            }
            match client.write(&buf[..n]) {
                Ok(0) => {}
                Ok(w) => sent += w,
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(_) => {
                    exact = false;
                    break;
                }
            }
        } else if !shut {
            shut = client.shutdown(Shutdown::Write).is_ok();
        }
        match client.read(&mut buf) {
            Ok(0) => eof = true,
            Ok(n) => {
                if got + n > 12_000 {
                    exact = false;
                    break;
                }
                for (i, byte) in buf[..n].iter().enumerate() {
                    if *byte != stream_byte(got + i) {
                        exact = false;
                        break;
                    }
                }
                if !exact {
                    break;
                }
                got += n;
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(_) => {
                exact = false;
                break;
            }
        }
    }
    (exact && shut && eof, sent, got)
}

/// NET15: controlled hostname resolution — a `&str` ToSocketAddrs routes through
/// getaddrinfo to relibc's lookup_host, which the kumo hook answers through the NET12
/// resolver over this process's netstack session. argv: `net <name> <tcp-port>`.
#[inline(never)]
fn net_name(args: &[String]) {
    use std::net::TcpStream;

    let name = args.get(2).cloned().unwrap_or_default();
    let Some(port) = args.get(3).and_then(|s| s.parse::<u16>().ok()) else {
        netlog!("name usage -> FAIL");
        return;
    };
    let mut failures = 0u32;
    let mut connected = None;
    let mut last = None;
    for _ in 0..600 {
        match TcpStream::connect((name.as_str(), port)) {
            Ok(c) => {
                connected = Some(c);
                break;
            }
            Err(e) => {
                // ETIMEDOUT already burned the resolver's own bounded budget and is
                // permanent for this boot; EACCES (no launcher grant) and ENOENT are
                // permanent outright. Only the fast transient classes are retried, so
                // a broken boot ends here instead of holding a child for hours.
                let transient = matches!(e.raw_os_error(), Some(101) | Some(11) | Some(111));
                last = Some(e);
                if !transient {
                    break;
                }
            }
        }
    }
    match connected {
        Some(stream) => {
            netlog!("name resolve+connect -> OK");
            let (exact, sent, got) = tcp_duplex(stream);
            if exact && sent == 12_000 && got == 12_000 {
                netlog!("name tcp duplex exact -> OK");
            } else {
                failures += 1;
                netlog!("name tcp duplex -> FAIL sent={} got={}", sent, got);
            }
        }
        None => {
            failures += 1;
            netlog!("name resolve+connect -> FAIL {:?}", last);
        }
    }
    netlog!("name campaign failed={}", failures);
    if failures == 0 {
        netlog!("hostname -> OK");
    }
}

/// NET15-c1 half one: arm a live TCP session and a pending read, so a netstack
/// restart must surface as an explicit old-socket failure (the j821 restartwait shape).
#[inline(never)]
fn recover_wait(args: &[String]) {
    use std::io::Read;
    use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
    use std::time::Duration;

    let Some(port) = args.get(3).and_then(|s| s.parse::<u16>().ok()) else {
        netlog!("recover-wait usage -> FAIL");
        return;
    };
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = match TcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) => {
            netlog!("recover-wait bind -> FAIL {:?}", e);
            return;
        }
    };
    let client = match TcpStream::connect(addr) {
        Ok(c) => c,
        Err(e) => {
            netlog!("recover-wait connect -> FAIL {:?}", e);
            return;
        }
    };
    let mut far = match listener.accept() {
        Ok((s, _)) => s,
        Err(e) => {
            netlog!("recover-wait accept -> FAIL {:?}", e);
            return;
        }
    };
    let _ = far.set_read_timeout(Some(Duration::from_secs(60)));
    netlog!("recover-wait armed (live TCP, pending read)");
    // Sora's NET15-c1 proof gates the supervised kill on this stdout record; the ring
    // record above stays the durable acceptance line. — TIMBERDOODLE
    {
        use std::io::Write as _;
        let mut out = std::io::stdout();
        let _ = out.write_all(b"recover-wait armed\n");
        let _ = out.flush();
    }
    let mut buf = [0u8; 64];
    match far.read(&mut buf) {
        Err(ref e)
            if e.kind() == std::io::ErrorKind::ConnectionReset
                || e.raw_os_error() == Some(104) =>
        {
            netlog!("recover-wait old session -> ECONNRESET ok");
        }
        Err(e) => netlog!("recover-wait old session -> {:?} FAIL", e),
        Ok(n) => netlog!("recover-wait old session -> Ok({}) FAIL", n),
    }
    // J856 diagnostics: localize the dead-session teardown. The records bracket the
    // socket drops and the return into std's exit path.
    netlog!("recover-wait teardown");
    drop(far);
    drop(listener);
    drop(client);
    netlog!("recover-wait teardown done");
    netlog!("recover-wait returning");
}

/// NET15-c1 half two: a fresh process (and therefore a fresh launcher grant to the
/// restarted netstack) rebinds the old port and completes an exact exchange.
#[inline(never)]
fn recover_fresh(args: &[String]) {
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};

    let Some(port) = args.get(3).and_then(|s| s.parse::<u16>().ok()) else {
        netlog!("recover-fresh usage -> FAIL");
        return;
    };
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut failures = 0u32;
    let mut bound = None;
    let mut last = None;
    // Sora delays the netstack respawn; each attempt is an IPC round trip, so the
    // budget spans the gap without spinning.
    for _ in 0..3000 {
        match TcpListener::bind(addr) {
            Ok(l) => {
                bound = Some(l);
                break;
            }
            Err(e) => {
                let transient =
                    matches!(e.raw_os_error(), Some(98) | Some(101) | Some(11) | Some(111) | Some(104));
                last = Some(e);
                if !transient {
                    break;
                }
            }
        }
    }
    let Some(listener) = bound else {
        netlog!("recover-fresh bind -> FAIL {:?}", last);
        netlog!("recover campaign failed=1");
        return;
    };
    netlog!("recover-fresh bind on the old port -> OK");
    match TcpStream::connect(addr) {
        Ok(mut client) => match listener.accept() {
            Ok((mut far, _)) => {
                let _ = client.write_all(b"fresh-session");
                let mut buf = [0u8; 13];
                let ok = far.read_exact(&mut buf).is_ok() && &buf == b"fresh-session";
                if ok {
                    netlog!("recover-fresh exact exchange -> OK");
                } else {
                    failures += 1;
                    netlog!("recover-fresh exact exchange -> FAIL");
                }
            }
            Err(e) => {
                failures += 1;
                netlog!("recover-fresh accept -> FAIL {:?}", e);
            }
        },
        Err(e) => {
            failures += 1;
            netlog!("recover-fresh connect -> FAIL {:?}", e);
        }
    }
    netlog!("recover campaign failed={}", failures);
    if failures == 0 {
        netlog!("recovery -> OK");
    }
}

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

/// SH3-e: the cargo-shaped basics on the granted volume, whose root this process sees
/// as "/". Leaves the tree in place for the reboot read-back.
#[inline(never)]
fn media_suite() {
    use std::fs;
    macro_rules! check {
        ($cond:expr, $msg:literal) => {
            if $cond {
                sh3log!("{} -> OK", $msg);
            } else {
                sh3log!("{} -> FAIL", $msg);
            }
        };
    }

    // A rerun must not trip on a tree an earlier attempt left behind — including an
    // attempt whose proof lines the console level hid. The suite owns /SH3E and
    // starts from a clean slate.
    let _ = fs::remove_dir_all("/SH3E");
    check!(fs::create_dir("/SH3E").is_ok(), "e1 mkdir");
    check!(fs::create_dir("/SH3E/SRC").is_ok(), "e1 mkdir tree");
    check!(fs::write("/SH3E/TMP.TXT", b"kumo-metal").is_ok(), "e1 write");
    check!(
        fs::rename("/SH3E/TMP.TXT", "/SH3E/FINAL.TXT").is_ok(),
        "e1 rename"
    );
    check!(
        fs::metadata("/SH3E/FINAL.TXT")
            .map(|m| m.len() == 10 && m.is_file())
            .unwrap_or(false),
        "e1 stat"
    );
    check!(
        fs::read("/SH3E/FINAL.TXT")
            .map(|d| d == b"kumo-metal")
            .unwrap_or(false),
        "e1 read"
    );
    match fs::read_dir("/SH3E") {
        Ok(entries) => {
            let mut have_final = false;
            let mut have_src = false;
            for entry in entries.flatten() {
                let name = entry.file_name();
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if name == "FINAL.TXT" && !is_dir {
                    have_final = true;
                }
                if name == "SRC" && is_dir {
                    have_src = true;
                }
            }
            check!(have_final && have_src, "e1 readdir");
        }
        Err(_) => sh3log!("e1 readdir -> FAIL"),
    }
}

/// SH3-e: the reboot read-back — the tree survives a power cycle on real media, then the
/// suite cleans up after itself.
#[inline(never)]
fn media_check() {
    use std::fs;
    macro_rules! check {
        ($cond:expr, $msg:literal) => {
            if $cond {
                sh3log!("{} -> OK", $msg);
            } else {
                sh3log!("{} -> FAIL", $msg);
            }
        };
    }

    check!(
        fs::read("/SH3E/FINAL.TXT")
            .map(|d| d == b"kumo-metal")
            .unwrap_or(false),
        "e3 persisted"
    );
    check!(fs::remove_dir_all("/SH3E").is_ok(), "e3 cleanup");
    check!(
        fs::metadata("/SH3E").map(|_| false).unwrap_or(true),
        "e3 cleaned"
    );
}

/// SH3-e: hold an exclusive lock for a bounded window. No sleep primitive exists in the
/// ABI, so the hold is a clock-bounded spin; preemption keeps the machine responsive and
/// the window is small.
#[inline(never)]
fn lock_hold(args: &[String]) {
    use std::fs;
    use std::time::{Duration, Instant};

    let path = args.get(2).map(String::as_str).unwrap_or("/SH3E/LOCK");
    let ms: u64 = args
        .get(3)
        .and_then(|value| value.parse().ok())
        .unwrap_or(3000);
    match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
    {
        Ok(file) => match file.lock() {
            Ok(()) => {
                sh3log!("lock held");
                let deadline = Instant::now() + Duration::from_millis(ms);
                while Instant::now() < deadline {}
                let _ = file.unlock();
                sh3log!("lock released");
            }
            Err(_) => sh3log!("lock hold -> FAIL"),
        },
        Err(_) => sh3log!("lock hold -> FAIL"),
    }
}

/// SH3-e: the contender — a held exclusive lock answers WouldBlock, and after the holder
/// releases, the same probe acquires.
#[inline(never)]
fn lock_try(args: &[String]) {
    use std::fs;

    let path = args.get(2).map(String::as_str).unwrap_or("/SH3E/LOCK");
    match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
    {
        Ok(file) => match file.try_lock() {
            Ok(()) => {
                sh3log!("lock acquired");
                let _ = file.unlock();
            }
            Err(fs::TryLockError::WouldBlock) => sh3log!("lock wouldblock"),
            Err(_) => sh3log!("lock try -> FAIL"),
        },
        Err(_) => sh3log!("lock try -> FAIL"),
    }
}

/// SH4: std::process::Command over the kernel spawn pipeline. Four records, each one
/// `-> OK` line the harness asserts (or the failure spelled out): a child fed through its
/// stdin pipe whose exact stdout, stderr and exit status come back through
/// wait_with_output (std's poll-driven two-pipe reader); a spinning child killed and
/// reaped with the kernel's killed status; and the no-grant proof — the child cannot open
/// the file it was loaded from, because no filesystem grant travels with a spawn. — TEAL
#[inline(never)]
fn spawn_parent() {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    match Command::new("/SMOKE.ELF")
        .arg("spawnchild")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            let fed = child
                .stdin
                .take()
                .map(|mut stdin| stdin.write_all(b"ping from parent\n").is_ok())
                .unwrap_or(false);
            match child.wait_with_output() {
                Ok(output) => {
                    if fed && output.stdout == b"child says hi: PING FROM PARENT\n" {
                        sh3log!("spawn output -> OK");
                    } else {
                        sh3log!(
                            "spawn output FAIL fed={} {:?}",
                            fed,
                            String::from_utf8_lossy(&output.stdout)
                        );
                    }
                    if output.stderr == b"child stderr\n" {
                        sh3log!("spawn stderr -> OK");
                    } else {
                        sh3log!(
                            "spawn stderr FAIL {:?}",
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }
                    if output.status.code() == Some(7) {
                        sh3log!("spawn status -> OK");
                    } else {
                        sh3log!("spawn status FAIL {:?}", output.status);
                    }
                }
                Err(e) => sh3log!("spawn output FAIL wait {:?} {:?}", e.kind(), e.raw_os_error()),
            }
        }
        Err(e) => sh3log!("spawn output FAIL spawn {:?} {:?}", e.kind(), e.raw_os_error()),
    }

    match Command::new("/SMOKE.ELF")
        .arg("spawnspin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            let killed = child.kill().is_ok();
            match child.wait() {
                Ok(status) if killed && !status.success() && status.code() == Some(137) => {
                    sh3log!("spawn kill -> OK");
                }
                Ok(status) => sh3log!("spawn kill FAIL killed={} {:?}", killed, status),
                Err(e) => sh3log!("spawn kill FAIL wait {:?}", e.kind()),
            }
        }
        Err(e) => sh3log!("spawn kill FAIL spawn {:?} {:?}", e.kind(), e.raw_os_error()),
    }

    match Command::new("/SMOKE.ELF")
        .arg("spawnnogrant")
        .stdin(Stdio::piped())
        .output()
    {
        Ok(output)
            if output.stdout == b"nogrant: open refused\n" && output.status.code() == Some(0) =>
        {
            sh3log!("spawn nogrant -> OK");
        }
        Ok(output) => sh3log!(
            "spawn nogrant FAIL {:?} {:?}",
            String::from_utf8_lossy(&output.stdout),
            output.status
        ),
        Err(e) => sh3log!("spawn nogrant FAIL {:?} {:?}", e.kind(), e.raw_os_error()),
    }
}

/// Spin (no sleep primitive exists yet) until this process's live thread count drops to
/// `want`: a joined thread has posted its result but may not have reached `ThreadExit` yet;
/// timer preemption runs it. Returns the final (threads, mappings).
fn settle_threads(want: u64) -> (u64, u64) {
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let (_, threads, mappings) = kumo_rt::thread_self();
        if threads <= want || Instant::now() > deadline {
            return (threads, mappings);
        }
        std::hint::spin_loop();
    }
}

/// SH5-a: std::thread over relibc's pthread runtime (rlct_clone -> ThreadSpawn). Proves
/// mutex/condvar handoffs between N threads, per-thread TLS, distinct OS thread ids, join,
/// a flat thread and mapping census across repeated create/join cycles (the stack and the
/// TLS/TCB mapping of every exited thread are released), and exit() from a non-main thread
/// ending the whole process. — TEAL
fn threads_suite() {
    use std::cell::Cell;
    use std::sync::{Arc, Barrier, Condvar, Mutex};

    const N: usize = 4;
    const ROUNDS: usize = 25;

    thread_local! {
        static SLOT: Cell<usize> = const { Cell::new(usize::MAX) };
    }

    // 1. Live census: N workers and main meet at a barrier; main counts N + 1 threads.
    let (main_tid, base_threads, _) = kumo_rt::thread_self();
    let gate = Arc::new(Barrier::new(N + 1));
    let release = Arc::new(Barrier::new(N + 1));
    let mut handles = Vec::new();
    for id in 0..N {
        let (gate, release) = (gate.clone(), release.clone());
        handles.push(std::thread::spawn(move || {
            SLOT.with(|slot| slot.set(id));
            gate.wait();
            release.wait();
            let tid = kumo_rt::thread_self().0;
            (SLOT.with(|slot| slot.get()), tid)
        }));
    }
    gate.wait();
    let (_, live, _) = kumo_rt::thread_self();
    release.wait();
    let mut tids = Vec::new();
    let mut tls_ok = true;
    for (id, handle) in handles.into_iter().enumerate() {
        match handle.join() {
            Ok((slot, tid)) => {
                tls_ok &= slot == id;
                tids.push(tid);
            }
            Err(_) => tls_ok = false,
        }
    }
    tids.push(main_tid);
    tids.sort_unstable();
    tids.dedup();
    if base_threads == 1 && live == (N as u64) + 1 {
        sh5log!("live -> OK ({} threads)", live);
    } else {
        sh5log!("live FAIL base={} live={}", base_threads, live);
    }
    if tls_ok && SLOT.with(|slot| slot.get()) == usize::MAX {
        sh5log!("tls -> OK");
    } else {
        sh5log!("tls FAIL");
    }
    if tids.len() == N + 1 && !tids.contains(&0) {
        sh5log!("tids -> OK");
    } else {
        sh5log!("tids FAIL {:?}", tids);
    }

    // 2. Handoffs: a token passes 0 -> 1 -> ... -> N-1 -> 0 under one mutex and condvar,
    // ROUNDS times around the ring; the log must be the exact round-robin order.
    let ring = Arc::new((Mutex::new((0usize, Vec::with_capacity(N * ROUNDS))), Condvar::new()));
    let mut workers = Vec::new();
    for id in 0..N {
        let ring = ring.clone();
        workers.push(std::thread::spawn(move || {
            let (lock, turn) = &*ring;
            for _ in 0..ROUNDS {
                let mut state = lock.lock().unwrap();
                while state.0 != id {
                    state = turn.wait(state).unwrap();
                }
                state.1.push(id);
                state.0 = (id + 1) % N;
                turn.notify_all();
            }
        }));
    }
    let joined = workers.into_iter().all(|worker| worker.join().is_ok());
    let order = &ring.0.lock().unwrap().1;
    let exact = order.len() == N * ROUNDS && order.iter().enumerate().all(|(i, id)| *id == i % N);
    if joined && exact {
        sh5log!("handoff -> OK ({} handoffs)", order.len());
    } else {
        sh5log!("handoff FAIL joined={} len={}", joined, order.len());
    }
    let (after, _) = settle_threads(1);
    if after == 1 {
        sh5log!("join -> OK");
    } else {
        sh5log!("join FAIL threads={}", after);
    }

    // 3. Census: warm up, then 64 create/join cycles. Each exited thread's stack and its
    // TLS/TCB mapping must be gone: same live thread count, same mapping count.
    let cycle = |round: usize| {
        let worker = std::thread::spawn(move || {
            SLOT.with(|slot| slot.set(round));
            SLOT.with(|slot| slot.get()) == round
        });
        worker.join().unwrap_or(false)
    };
    let mut cycles_ok = (0..4).all(cycle);
    let (t0, m0) = settle_threads(1);
    cycles_ok &= (0..64).all(cycle);
    let (t1, m1) = settle_threads(1);
    if cycles_ok && t0 == 1 && t1 == 1 && m0 == m1 {
        sh5log!("cycles flat -> OK (64 cycles, threads {}, mappings {})", t1, m1);
    } else {
        sh5log!("cycles FAIL ok={} threads {}->{} mappings {}->{}", cycles_ok, t0, t1, m0, m1);
    }

    // 4. exit() from a non-main thread is a group exit: the child's main thread is parked
    // in join and still the process ends with the worker's status.
    match std::process::Command::new("/SMOKE.ELF")
        .arg("threadexit")
        .stdin(std::process::Stdio::piped())
        .status()
    {
        Ok(status) if status.code() == Some(5) => sh5log!("group exit -> OK"),
        Ok(status) => sh5log!("group exit FAIL {:?}", status),
        Err(e) => sh5log!("group exit FAIL spawn {:?} {:?}", e.kind(), e.raw_os_error()),
    }
}

/// SH3-d: the std::fs metadata proof. Every call below lands on a Pal method the KUMO
/// overlay grew in SH3-d2 (mkdirat, fstatat, renameat2, unlinkat); this process's
/// launcher-spawned fatfs server answers them over the image that holds this binary.
/// 8.3 names only, matching the FAT driver's recorded scope. — TIMBERDOODLE
#[inline(never)]
fn sh3_probe() {
    use std::fs;
    use std::io::ErrorKind;
    macro_rules! check {
        ($cond:expr, $msg:literal) => {
            if $cond {
                sh3log!("{} -> OK", $msg);
            } else {
                sh3log!("{} -> FAIL", $msg);
            }
        };
    }

    let dir = "/SH3DIR";
    let a = "/SH3DIR/A.TXT";
    let b = "/SH3DIR/B.TXT";
    check!(fs::create_dir(dir).is_ok(), "mkdir");
    check!(
        fs::metadata("/").map(|m| m.is_dir()).unwrap_or(false),
        "root stat"
    );
    check!(
        fs::metadata(dir).map(|m| m.is_dir()).unwrap_or(false),
        "stat dir"
    );
    check!(fs::write(a, b"kumo").is_ok(), "file write");
    check!(
        fs::metadata(a)
            .map(|m| m.len() == 4 && m.is_file())
            .unwrap_or(false),
        "file stat"
    );
    check!(fs::rename(a, b).is_ok(), "rename");
    check!(
        fs::read_to_string(b).map(|t| t == "kumo").unwrap_or(false),
        "read back"
    );
    // SH3-d2b: advisory locks. Both handles share this launch's fatfs server, whose
    // per-launch lock table conflicts across handles, so contention is observable in
    // one process. The blocking form runs only uncontended: the ABI has no sleep
    // primitive to park a contending caller (recorded deviation).
    let h1 = fs::File::open(b);
    let h2 = fs::File::open(b);
    match (&h1, &h2) {
        (Ok(h1), Ok(h2)) => {
            check!(h1.try_lock().is_ok(), "lock ex");
            check!(
                matches!(h2.try_lock(), Err(fs::TryLockError::WouldBlock)),
                "lock conflict ex"
            );
            check!(
                matches!(h2.try_lock_shared(), Err(fs::TryLockError::WouldBlock)),
                "lock conflict sh"
            );
            check!(h1.unlock().is_ok(), "unlock");
            check!(h2.try_lock_shared().is_ok(), "lock sh");
            check!(
                matches!(h1.try_lock(), Err(fs::TryLockError::WouldBlock)),
                "lock ex vs sh"
            );
            check!(h2.unlock().is_ok(), "shared unlock");
            check!(h1.lock().is_ok() && h1.unlock().is_ok(), "blocking lock free");
            // The fcntl record-lock surface through lockf(3): F_TLOCK/F_ULOCK route to
            // F_SETLK, F_TEST to F_GETLK's whole-file approximation.
            use std::os::unix::io::AsRawFd;
            unsafe extern "C" {
                fn lockf(fd: i32, function: i32, size: i64) -> i32;
            }
            let fd = h1.as_raw_fd();
            check!(unsafe { lockf(fd, 2, 0) } == 0, "fcntl setlk");
            check!(unsafe { lockf(fd, 0, 0) } == 0, "fcntl unlock");
            check!(unsafe { lockf(fd, 3, 0) } == 0, "fcntl getlk");
        }
        _ => sh3log!("lock handles -> FAIL"),
    }

    // SH3-d2c: directory iteration through std::fs::read_dir (opendir -> getdents ->
    // OP_READ_DIR pages). The subdirectory makes the listing mixed-type, and the root
    // listing exercises paging across the image's own entries.
    check!(fs::create_dir("/SH3DIR/SUB").is_ok(), "mkdir sub");
    match fs::read_dir(dir) {
        Ok(entries) => {
            let mut file_ok = false;
            let mut sub_ok = false;
            let mut count = 0usize;
            for entry in entries.flatten() {
                count += 1;
                let name = entry.file_name();
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if name == "B.TXT" && !is_dir {
                    file_ok = true;
                }
                if name == "SUB" && is_dir {
                    sub_ok = true;
                }
            }
            check!(count == 2 && file_ok && sub_ok, "read dir");
        }
        Err(_) => sh3log!("read dir -> FAIL"),
    }
    match fs::read_dir("/") {
        Ok(entries) => {
            let mut have_dir = false;
            let mut have_efi = false;
            for entry in entries.flatten() {
                let name = entry.file_name();
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if name == "SH3DIR" && is_dir {
                    have_dir = true;
                }
                if name == "EFI" && is_dir {
                    have_efi = true;
                }
            }
            check!(have_dir && have_efi, "read dir root");
        }
        Err(_) => sh3log!("read dir root -> FAIL"),
    }
    match fs::read_dir("/SH3DIR/SUB") {
        Ok(entries) => check!(entries.flatten().next().is_none(), "read dir empty"),
        Err(_) => sh3log!("read dir empty -> FAIL"),
    }
    check!(fs::remove_dir("/SH3DIR/SUB").is_ok(), "rmdir sub");

    // SH3-d2d: the rest of std::fs — seek and at-io over the positional wire, O_APPEND,
    // set_len, try_clone, the directory-tree helpers, and copy (whose set_permissions
    // chain rides the fd-chmod no-op).
    {
        use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
        use std::os::unix::fs::{FileExt, PermissionsExt};

        let c = "/SH3DIR/C.TXT";
        let e = "/SH3DIR/E.TXT";
        check!(fs::write(c, b"abcdef").is_ok(), "d2d write");
        match fs::OpenOptions::new().read(true).write(true).open(c) {
            Ok(mut f) => {
                check!(
                    f.seek(SeekFrom::Start(1)).map(|p| p == 1).unwrap_or(false),
                    "seek"
                );
                let mut two = [0u8; 2];
                check!(f.read_exact(&mut two).is_ok() && &two == b"bc", "seek read");
                check!(
                    f.seek(SeekFrom::End(0)).map(|p| p == 6).unwrap_or(false),
                    "seek end"
                );
                let mut at = [0u8; 2];
                check!(
                    f.read_at(&mut at, 4).map(|n| n == 2).unwrap_or(false) && &at == b"ef",
                    "at read"
                );
                check!(
                    f.write_at(b"XY", 0).map(|n| n == 2).unwrap_or(false),
                    "at write"
                );
            }
            Err(_) => sh3log!("seek -> FAIL"),
        }
        check!(
            fs::read(c).map(|d| d == b"XYcdef").unwrap_or(false),
            "at write readback"
        );
        match fs::OpenOptions::new().append(true).open(c) {
            Ok(mut f) => check!(f.write_all(b"!").is_ok(), "append"),
            Err(_) => sh3log!("append -> FAIL"),
        }
        check!(
            fs::read(c).map(|d| d == b"XYcdef!").unwrap_or(false),
            "append readback"
        );
        match fs::OpenOptions::new().write(true).open(c) {
            Ok(f) => {
                check!(f.set_len(3).is_ok(), "set len");
                check!(
                    f.metadata().map(|m| m.len() == 3).unwrap_or(false),
                    "set len stat"
                );
                check!(
                    f.set_permissions(fs::Permissions::from_mode(0o600))
                        .is_ok(),
                    "fchmod fd"
                );
            }
            Err(e) => sh3log!("set len -> FAIL ({:?} {:?})", e.kind(), e.raw_os_error()),
        }
        match (fs::File::open(c), fs::File::open(c)) {
            (Ok(f1), Ok(mut f2)) => match f1.try_clone() {
                Ok(mut c1) => {
                    let mut a = [0u8; 1];
                    let mut b1 = [0u8; 1];
                    let ok = c1.read_exact(&mut a).is_ok()
                        && a == *b"X"
                        && f2.read_exact(&mut b1).is_ok()
                        && b1 == *b"X";
                    check!(ok, "clone");
                }
                Err(e) => sh3log!("clone -> FAIL ({:?} {:?})", e.kind(), e.raw_os_error()),
            },
            _ => sh3log!("clone -> FAIL (open)"),
        }
        check!(fs::create_dir_all("/SH3DIR/D1/D2").is_ok(), "mkdir all");
        check!(
            fs::metadata("/SH3DIR/D1/D2").map(|m| m.is_dir()).unwrap_or(false),
            "mkdir all stat"
        );
        sh3log!("d2d copy begins");
        check!(fs::copy(c, e).map(|n| n == 3).unwrap_or(false), "copy");
        check!(fs::read(e).map(|d| d == b"XYc").unwrap_or(false), "copy readback");
        sh3log!("d2d remove all begins");
        match fs::remove_dir_all("/SH3DIR/D1") {
            Ok(()) => sh3log!("remove all -> OK"),
            Err(e) => sh3log!("remove all -> FAIL ({:?} {:?})", e.kind(), e.raw_os_error()),
        }
        check!(
            fs::metadata("/SH3DIR/D1").map(|_| false).unwrap_or(true),
            "remove all gone"
        );
        check!(fs::remove_file(c).is_ok(), "d2d cleanup c");
        check!(fs::remove_file(e).is_ok(), "d2d cleanup e");
    }
    check!(
        fs::metadata(a)
            .map(|_| false)
            .unwrap_or_else(|e| e.kind() == ErrorKind::NotFound),
        "old name gone"
    );
    check!(fs::remove_file(b).is_ok(), "unlink");
    check!(fs::remove_dir(dir).is_ok(), "rmdir");
    check!(
        fs::metadata(dir)
            .map(|_| false)
            .unwrap_or_else(|e| e.kind() == ErrorKind::NotFound),
        "stat removed dir"
    );
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
