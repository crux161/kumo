#![no_std]
#![no_main]
//j824
//! AX2 call/reply probe: the caller and the three correctness servers for the
//! `ChannelCall` (syscall 6) register fast path (PLAN/021 AX2, PLAN/009 A2).
//!
//! One heap-free binary, one role per argv. It is launched by Sora's `ax2bench` command
//! (or by hand) with exactly two transferred handles: `x0` is the bootstrap channel that
//! carries the argv VMO (`a`) and the control endpoint (`c`); `x1` is one end of the call
//! channel, whose other end belongs to the peer process.
//!
//! Roles:
//! - `ping <rounds> <bytes> [deadline-ms]` — caller. `bytes` is 8 or 32. Issues `rounds`
//!   sequential `ChannelCall`s on `x1` with an absolute deadline and checks every reply byte.
//! - `pong` — echo server. Reads calls, applies the documented transform, replies.
//! - `die` — server. Reads one call, reports, then exits WITHOUT replying.
//! - `never` — server. Reads one call, waits for the first of a long timer and peer close,
//!   then attempts one late reply and reports its status (expected `BadState`).
//! - `hold <ms>` — server. Replies after at least one 20 Hz scheduler tick (ms >= 50).
//!
//! # Wire format (v1)
//!
//! A request of `len` bytes (8 or 32) is:
//!
//! ```text
//! 0..4   sequence, u32 little-endian (1, 2, 3, ... within one ping run)
//! 4..8   nonce, u32 little-endian (derived once from the run's start clock)
//! 8..len pattern byte for each offset (see `pattern_byte`)
//! ```
//!
//! The reply is the request transformed byte-for-byte:
//!
//! ```text
//! reply[i] = request[i].rotate_left(1) ^ 0x3c
//! ```
//!
//! A reply must be exactly `len` bytes. The caller unpacks the four register words with
//! `kumo_abi::unpack_call_payload` and compares all `len` bytes; it never reads padding
//! beyond `len`. A server replies with the same length it received. A malformed request is
//! answered with `len` zero bytes so the caller fails on content rather than hanging.
//!
//! # Deadline semantics (exact)
//!
//! The caller reads the monotonic clock ONCE at the start of the run and derives one
//! absolute deadline for every call: `start + deadline_ms * 1_000_000` (x1). `0` would mean
//! "no deadline" in the ABI, but the probe never uses it. A shared batch deadline keeps the
//! measured path to the three syscalls of call/read/reply; a ClockGet before every call
//! would add a fourth syscall per round trip and inflate the AX2 syscall gate. A run that
//! would exceed its deadline reports the failure it sees (`TimedOut`); 1024 calls on the
//! target are far inside the default 5 s, and `never` deliberately uses 100 ms.
//!
//! # AXP2 control frame (32 bytes; a ready `R` frame first, the role's final frame last)
//!
//! ```text
//! 0..4   "AXP2"
//! 4      role: 'R' ready, 'p' ping, 'o' pong, 'd' die, 'n' never, 'h' hold, '?' unknown
//! 5..8   zero
//! 8..12  rounds: u32 little-endian (calls issued by ping; calls served by a server)
//! 12..16 status: i32 little-endian (last raw syscall status; 0 = Ok; a server's status is
//!        its last reply/setup result)
//! 16..20 errors: u32 little-endian (payload/verification failures; a non-Ok syscall
//!        status is reported in `status`, not counted here)
//! 20..24 simd-errors: u32 little-endian (q31 canary failures across call SVCs)
//! 24..32 total-ns: u64 little-endian (monotonic elapsed for the role's measured span)
//! ```
//!
//! # Start handshake
//!
//! Every child first writes one ready frame (role `R`, all counters zero) on `c`, then
//! enters its role. The caller (`ping`) then blocks on a one-byte read of `c` and starts
//! its clock only after Sora answers with the go byte `G`. Sora reads both ready frames
//! before it snapshots the live set and sends `G` after that snapshot, so spawn and
//! handshake costs never enter the measured window (the same shape as `actorprobe`'s AX0
//! handshake).
//!
//! # SIMD canary
//!
//! Each caller SVC runs inside one asm block that seeds all 128 bits of q31 with 0x5a,
//! executes `svc #0`, then XORs q31 against a freshly seeded q30 and ORs the two 64-bit
//! halves. Any kernel path that clobbers the caller's vector state makes the result
//! nonzero; `ping` counts that as an error and reports it in `simd-errors`. The shape is
//! copied from `actorprobe`'s AX0 `wait` canary; the call deliberately does not go through
//! `kumo_rt::channel_call`, because the canary and the SVC must live in one asm block.
//! — TIMBERDOODLE

#[cfg(target_arch = "aarch64")]
use kumo_abi::Syscall;
use kumo_abi::{
    pack_call_payload, unpack_argv, unpack_call_payload, Errno, Handle, MAX_CALL_BYTES,
};
use kumo_rt::{
    channel_read, channel_read_call, channel_reply, channel_write, clock_get, handle_close,
    handle_koid, port_bind, port_create, port_wait, process_exit, sleep_ns, startup, timer_create,
    vmo_read,
};
kumo_rt::entry!(main);

/// The v1 call payload sizes this probe speaks. Both fit the four register words.
const SMALL_BYTES: usize = 8;
const FULL_BYTES: usize = MAX_CALL_BYTES;
/// The report role tags, also documented in the module header.
const ROLE_PING: u8 = b'p';
const ROLE_PONG: u8 = b'o';
const ROLE_DIE: u8 = b'd';
const ROLE_NEVER: u8 = b'n';
const ROLE_HOLD: u8 = b'h';
const ROLE_UNKNOWN: u8 = b'?';
/// The start-handshake frame every role writes before entering its role.
const ROLE_READY: u8 = b'R';
/// The default absolute deadline for a whole ping run (1024 calls at the target's speed
/// stay far below it). `never` is launched with an explicit 100 ms deadline by Sora.
const DEFAULT_DEADLINE_MS: u64 = 5_000;
const MAX_DEADLINE_MS: u64 = 60_000;
/// One 20 Hz tick is 50 ms; `hold` refuses shorter delays because they would not witness a
/// scheduler tick. Sora launches it at 75 ms.
const MIN_HOLD_MS: u64 = 50;
const MAX_HOLD_MS: u64 = 10_000;
/// The late-reply delay in `never`: long past the caller's 100 ms deadline, short enough
/// for Sora's 15 s bounded read.
const LATE_REPLY_NS: u64 = 500_000_000;
const MAX_ROUNDS: u32 = 1_000_000;

fn parse_u64(text: &[u8]) -> Option<u64> {
    if text.is_empty() || text.len() > 19 {
        return None;
    }
    let mut value = 0u64;
    for &digit in text {
        if !digit.is_ascii_digit() {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add(u64::from(digit - b'0'))?;
    }
    Some(value)
}

fn parse_u32(text: &[u8]) -> Option<u32> {
    parse_u64(text).and_then(|value| u32::try_from(value).ok())
}

fn checked_handle(raw: u64) -> Option<Handle> {
    if raw == 0 || raw > i64::MAX as u64 {
        None
    } else {
        Some(Handle(raw as u32))
    }
}

/// Deterministic pattern byte at `offset` for one (sequence, nonce). Both sides compute it;
/// a mismatch means a payload byte was lost or corrupted in either direction.
fn pattern_byte(seq: u32, nonce: u32, offset: usize) -> u8 {
    let mixed = seq.wrapping_mul(0x9e37_79b1)
        ^ nonce.rotate_left(7)
        ^ (offset as u32).wrapping_mul(0x85eb_ca6b);
    (mixed >> 24) as u8
}

fn fill_request(seq: u32, nonce: u32, out: &mut [u8]) {
    if out.len() < SMALL_BYTES {
        return;
    }
    out[..4].copy_from_slice(&seq.to_le_bytes());
    out[4..8].copy_from_slice(&nonce.to_le_bytes());
    let mut offset = 8;
    while offset < out.len() {
        out[offset] = pattern_byte(seq, nonce, offset);
        offset += 1;
    }
}

/// The documented reply transform: one rotate and one XOR per byte.
fn transform(request: &[u8], out: &mut [u8]) {
    let mut i = 0;
    while i < request.len() {
        out[i] = request[i].rotate_left(1) ^ 0x3c;
        i += 1;
    }
}

fn report(
    control: Handle,
    role: u8,
    rounds: u32,
    status: i64,
    errors: u32,
    simd_errors: u32,
    total_ns: u64,
) -> bool {
    let mut frame = [0u8; 32];
    frame[..4].copy_from_slice(b"AXP2");
    frame[4] = role;
    frame[8..12].copy_from_slice(&rounds.to_le_bytes());
    frame[12..16].copy_from_slice(&(status as i32).to_le_bytes());
    frame[16..20].copy_from_slice(&errors.to_le_bytes());
    frame[20..24].copy_from_slice(&simd_errors.to_le_bytes());
    frame[24..32].copy_from_slice(&total_ns.to_le_bytes());
    channel_write(control, frame.as_ptr(), frame.len()) == 0
}

/// Issue one call and check the q31 canary across the SVC. The canary shape is the AX0
/// `actorprobe` one: seed q31, svc, seed q30, XOR, OR the halves, test zero.
#[cfg(target_arch = "aarch64")]
fn call_checked(peer: Handle, deadline_ns: u64, payload: &[u8]) -> (i64, usize, [u64; 4], bool) {
    let Some(words) = pack_call_payload(payload) else {
        return (i64::from(Errno::InvalidArgs.status()), 0, [0; 4], true);
    };
    let status: u64;
    let reply_len: u64;
    let (w0, w1, w2, w3): (u64, u64, u64, u64);
    let bad: u64;
    unsafe {
        core::arch::asm!(
            "movi v31.16b, #0x5a",
            "svc #0",
            "movi v30.16b, #0x5a",
            "eor v30.16b, v30.16b, v31.16b",
            "umov x9, v30.d[0]",
            "umov x10, v30.d[1]",
            "orr x11, x9, x10",
            in("x8") Syscall::ChannelCall as u64,
            inlateout("x0") peer.0 as u64 => status,
            inlateout("x1") deadline_ns => reply_len,
            inlateout("x2") words[0] => w0,
            inlateout("x3") words[1] => w1,
            inlateout("x4") words[2] => w2,
            inlateout("x5") words[3] => w3,
            in("x6") payload.len() as u64,
            in("x7") 0u64,
            lateout("x9") _,
            lateout("x10") _,
            lateout("x11") bad,
            lateout("v30") _,
            lateout("v31") _,
            options(nostack),
        );
    }
    (
        status as i64,
        reply_len as usize,
        [w0, w1, w2, w3],
        bad == 0,
    )
}

#[cfg(not(target_arch = "aarch64"))]
fn call_checked(_peer: Handle, _deadline_ns: u64, _payload: &[u8]) -> (i64, usize, [u64; 4], bool) {
    (i64::from(Errno::NotSupported.status()), 0, [0; 4], true)
}

/// The caller. Returns true only when every requested call completed with status Ok and
/// the reply bytes, the server's own report, and the SIMD canary all checked out.
fn ping(control: Handle, peer: Handle, rounds: u32, bytes: usize, deadline_ms: u64) -> bool {
    if rounds == 0
        || rounds > MAX_ROUNDS
        || (bytes != SMALL_BYTES && bytes != FULL_BYTES)
        || deadline_ms > MAX_DEADLINE_MS
    {
        let _ = report(
            control,
            ROLE_PING,
            0,
            i64::from(Errno::InvalidArgs.status()),
            0,
            0,
            0,
        );
        return false;
    }
    // Wait for Sora's go byte after the baseline snapshot; the measured window starts here.
    let mut go = [0u8; 1];
    let n = channel_read(control, go.as_mut_ptr(), go.len());
    if n != 1 || go[0] != b'G' {
        let _ = report(
            control,
            ROLE_PING,
            0,
            i64::from(Errno::BadState.status()),
            0,
            0,
            0,
        );
        return false;
    }
    let began = clock_get();
    let deadline_ns = if deadline_ms == 0 {
        0
    } else {
        began.saturating_add(deadline_ms.saturating_mul(1_000_000))
    };
    let nonce = (began as u32) ^ 0xa5a5_0000;
    let mut request = [0u8; MAX_CALL_BYTES];
    let mut expected = [0u8; MAX_CALL_BYTES];
    let mut decoded = [0u8; MAX_CALL_BYTES];
    let mut issued = 0u32;
    let mut errors = 0u32;
    let mut simd_errors = 0u32;
    let mut status = 0i64;
    while issued < rounds {
        let seq = issued.wrapping_add(1);
        fill_request(seq, nonce, &mut request[..bytes]);
        let (call_status, reply_len, words, canary_ok) =
            call_checked(peer, deadline_ns, &request[..bytes]);
        issued = issued.saturating_add(1);
        if !canary_ok {
            simd_errors = simd_errors.saturating_add(1);
            errors = errors.saturating_add(1);
        }
        if call_status != 0 {
            status = call_status;
            break;
        }
        transform(&request[..bytes], &mut expected[..bytes]);
        let verified = reply_len == bytes
            && unpack_call_payload(words, reply_len, &mut decoded[..]) == Some(reply_len)
            && decoded[..bytes] == expected[..bytes];
        if !verified {
            errors = errors.saturating_add(1);
        }
    }
    let total_ns = clock_get().saturating_sub(began);
    let ok = report(
        control,
        ROLE_PING,
        issued,
        status,
        errors,
        simd_errors,
        total_ns,
    ) && status == 0
        && errors == 0
        && simd_errors == 0
        && issued == rounds;
    ok
}

/// Validate a delivered request on both paths (register words and the byte copy the kernel
/// made into the read buffer). Returns true only when they agree and the pattern is exact.
fn validate_request(
    request: &[u8],
    words: [u64; 4],
    decoded: &mut [u8],
    expected: &mut [u8],
) -> bool {
    if request.len() != SMALL_BYTES && request.len() != FULL_BYTES {
        return false;
    }
    let Some(n) = unpack_call_payload(words, request.len(), decoded) else {
        return false;
    };
    if n != request.len() || &decoded[..n] != request {
        return false;
    }
    let seq = u32::from_le_bytes([request[0], request[1], request[2], request[3]]);
    let nonce = u32::from_le_bytes([request[4], request[5], request[6], request[7]]);
    fill_request(seq, nonce, &mut expected[..request.len()]);
    &expected[..request.len()] == request
}

/// The echo path shared by `pong` and `hold`. `delay_ns == 0` replies immediately; `hold`
/// passes a delay longer than one 20 Hz tick. A malformed request is answered with zeros so
/// the caller fails on content instead of parking until its deadline.
fn serve(control: Handle, peer: Handle, role: u8, delay_ns: u64) -> bool {
    let began = clock_get();
    let mut request = [0u8; MAX_CALL_BYTES];
    let mut decoded = [0u8; MAX_CALL_BYTES];
    let mut expected = [0u8; MAX_CALL_BYTES];
    let mut reply = [0u8; MAX_CALL_BYTES];
    let mut rounds = 0u32;
    let mut errors = 0u32;
    let mut status = 0i64;
    loop {
        let (n, _attached, words) = channel_read_call(peer, &mut request[..]);
        if n < 0 {
            status = n;
            break;
        }
        // v1 has no distinct empty-message signal: the AX2 call protocol treats a zero-length
        // read as peer close and stops. A call always carries 8 or 32 bytes.
        let len = n as usize;
        if len == 0 {
            break;
        }
        rounds = rounds.saturating_add(1);
        let len = len.min(MAX_CALL_BYTES);
        if !validate_request(&request[..len], words, &mut decoded[..], &mut expected[..]) {
            errors = errors.saturating_add(1);
            let reply_status = channel_reply(len, [0; 4]);
            if reply_status != 0 {
                status = reply_status;
                break;
            }
            continue;
        }
        if delay_ns != 0 && !sleep_ns(delay_ns) {
            errors = errors.saturating_add(1);
            status = i64::from(Errno::Internal.status());
            break;
        }
        transform(&request[..len], &mut reply[..len]);
        let Some(reply_words) = pack_call_payload(&reply[..len]) else {
            errors = errors.saturating_add(1);
            status = i64::from(Errno::InvalidArgs.status());
            break;
        };
        let reply_status = channel_reply(len, reply_words);
        if reply_status != 0 {
            status = reply_status;
            break;
        }
    }
    let total_ns = clock_get().saturating_sub(began);
    report(control, role, rounds, status, errors, 0, total_ns) && status == 0 && errors == 0
}

/// Peer death mid-call: receive exactly one call, report, and exit without replying. The
/// parked caller is expected to observe `PeerClosed`.
fn die(control: Handle, peer: Handle) -> bool {
    let began = clock_get();
    let mut request = [0u8; MAX_CALL_BYTES];
    let (n, _attached, _words) = channel_read_call(peer, &mut request[..]);
    let rounds = if n > 0 { 1 } else { 0 };
    let status = if n < 0 {
        n
    } else if n == 0 {
        i64::from(Errno::PeerClosed.status())
    } else {
        0
    };
    let total_ns = clock_get().saturating_sub(began);
    report(control, ROLE_DIE, rounds, status, 0, 0, total_ns) && status == 0 && rounds == 1
}

/// Deadline: receive one call, then wait for the first of a long one-shot timer and peer
/// close WITHOUT replying. On either wake, attempt exactly one late reply and report its
/// status; the kernel must answer `BadState` because the caller's deadline has retired the
/// call. A wake that is neither the timer nor the peer (or a failed bind) falls back to a
/// plain sleep so the late reply is still attempted late.
fn never(control: Handle, peer: Handle) -> bool {
    let began = clock_get();
    let mut request = [0u8; MAX_CALL_BYTES];
    let mut reply = [0u8; MAX_CALL_BYTES];
    let (n, _attached, _words) = channel_read_call(peer, &mut request[..]);
    if n <= 0 {
        let status = if n < 0 {
            n
        } else {
            i64::from(Errno::PeerClosed.status())
        };
        let total_ns = clock_get().saturating_sub(began);
        let _ = report(control, ROLE_NEVER, 0, status, 0, 0, total_ns);
        return false;
    }
    let len = (n as usize).min(MAX_CALL_BYTES);
    let port_raw = port_create();
    let timer_raw = timer_create(LATE_REPLY_NS);
    let mut woke = false;
    if port_raw != u64::MAX && timer_raw != u64::MAX {
        let port = Handle(port_raw as u32);
        let timer = Handle(timer_raw as u32);
        let timer_koid = handle_koid(timer);
        let peer_koid = handle_koid(peer);
        let timer_bound = port_bind(port, timer) == 0;
        let peer_bound = port_bind(port, peer) == 0;
        if timer_bound || peer_bound {
            let source = port_wait(port);
            woke = source == timer_koid || source == peer_koid;
        }
        let _ = handle_close(timer);
        let _ = handle_close(port);
    }
    if !woke {
        let _ = sleep_ns(LATE_REPLY_NS);
    }
    transform(&request[..len], &mut reply[..len]);
    let clean_reply_words = pack_call_payload(&reply[..len]).unwrap_or([0; 4]);
    let late_status = channel_reply(len, clean_reply_words);
    let total_ns = clock_get().saturating_sub(began);
    let ok = report(control, ROLE_NEVER, 1, late_status, 0, 0, total_ns)
        && late_status == i64::from(Errno::BadState.status());
    ok
}

fn usage(control: Handle, role: u8) -> bool {
    let _ = report(
        control,
        role,
        0,
        i64::from(Errno::InvalidArgs.status()),
        0,
        0,
        0,
    );
    false
}

#[no_mangle]
extern "C" fn main(
    bootstrap: u64,
    peer: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
    _a7: u64,
    _a8: u64,
) -> ! {
    let boot = startup(Handle(bootstrap as u32));
    let Some(control) = boot.cap0 else {
        process_exit(2)
    };
    let Some(peer) = checked_handle(peer) else {
        process_exit(2)
    };
    // Announce that the role has started; the caller then blocks on the go byte. Sora reads
    // both announcements before its baseline snapshot.
    if !report(control, ROLE_READY, 0, 0, 0, 0, 0) {
        process_exit(0);
    }
    let mut argv_buf = [0u8; 256];
    let read = boot
        .argv
        .is_some_and(|argv| vmo_read(argv, 0, argv_buf.as_mut_ptr(), argv_buf.len()) == 0);
    let mut args = unpack_argv(if read { &argv_buf[..] } else { &[] }).skip(1);
    let role: &[u8] = args.next().unwrap_or(b"");
    // The verdict lives in the AXP2 frame. Expected nonzero statuses (`die`'s PeerClosed,
    // `never`'s TimedOut) must not become ABEND towers, so the process always exits 0.
    let _ok = if role == b"ping" {
        match (
            args.next().and_then(parse_u32),
            args.next().and_then(parse_u32),
        ) {
            (Some(rounds), Some(bytes)) => {
                let deadline_ms = match args.next() {
                    None => Some(DEFAULT_DEADLINE_MS),
                    Some(text) => parse_u64(text),
                };
                match deadline_ms {
                    Some(deadline_ms) => ping(control, peer, rounds, bytes as usize, deadline_ms),
                    None => usage(control, ROLE_PING),
                }
            }
            _ => usage(control, ROLE_PING),
        }
    } else if role == b"pong" {
        serve(control, peer, ROLE_PONG, 0)
    } else if role == b"die" {
        die(control, peer)
    } else if role == b"never" {
        never(control, peer)
    } else if role == b"hold" {
        match args.next().and_then(parse_u64) {
            Some(ms) if (MIN_HOLD_MS..=MAX_HOLD_MS).contains(&ms) => {
                serve(control, peer, ROLE_HOLD, ms.saturating_mul(1_000_000))
            }
            _ => usage(control, ROLE_HOLD),
        }
    } else {
        usage(control, ROLE_UNKNOWN)
    };
    process_exit(0)
}
