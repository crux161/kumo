#![no_std]
#![no_main]
//j823
//! AX1 address-space stress probe (PLAN/021).
//!
//! Every role maps a VMO at the same fixed user VA (`0x1800_0000`), fills it with a pattern
//! derived from its own nonce, and byte-checks every read against that pattern.
//!
//! `conc <a|b> <nonce>`: two separately spawned processes. Role `a` verifies once, sleeps
//! 1 s so Sora can admit the peer, then loops for six seconds; role `b` loops for 3.5 s.
//! The two live windows overlap, so the 20 Hz scheduler rotates two address spaces that map
//! *different* physical pages at the *same* VA. A stale translation shows up as a byte that
//! does not match this process's pattern, and is counted, never ignored.
//! `remap <nonce>`: one process maps, checks, unmaps and remaps the same VA three times with
//! fresh VMOs and fresh contents.
//! `reuse <nonce>`: one fresh process maps the VA and checks. The autoexec runs it after its
//! predecessor has exited, so the address space (and any ASID the kernel would assign) is
//! reused.
//!
//! Output is deliberately small and exact: the harness parses `stale=` counters, the
//! per-role checksums (which must differ), and one `asidpair: PASS` marker per role.
//! — TIMBERDOODLE

use kumo_abi::{unpack_argv, Handle, VmarFlags};
use kumo_rt::{
    channel_write, clock_get, debug_write, handle_close, process_exit, sleep_ns, startup, vmar_map,
    vmar_unmap, vmo_create, vmo_read, vmo_write,
};

kumo_rt::entry!(main);

/// The one user VA every role maps. A child's VMAR is 512 MiB at 0; the image and the
/// 16 KiB main stack live far below this, and the heap is never used by this probe.
const FIXED_VA: u64 = 0x1800_0000;
const PAGE: usize = 4096;
const CHUNK: usize = 256;
const GEN_STEP: u32 = 1000;

/// The kernel gives every child the same initial stack VAs, so under the injected AX1 defect
/// a peer's stack writes can land on the other process's frames through stale translations.
/// Each role switches to its own stack mapping; the only same-VA collision under test is then
/// the data page, which is what the proof is about.
const PRIVATE_STACK_BASE: u64 = 0x1900_0000;
const PRIVATE_STACK_SIZE: usize = 0x4000;
const PRIVATE_STACK_STRIDE: u64 = 0x1_0000;

/// Role `a` must block long enough for Sora to admit role `b`; the strict-priority
/// scheduler otherwise lets a runnable child starve Sora (63 outranks 64).
const SLEEP_A_NS: u64 = 1_000_000_000;
const STRESS_A_NS: u64 = 6_000_000_000;
const STRESS_B_NS: u64 = 3_500_000_000;
/// Read the clock this often inside the hot loop; a gap larger than `GAP_NS` between two
/// of this process's own reads means it was descheduled, so gaps witness real switches.
const CLOCK_EVERY: u64 = 256;
const GAP_NS: u64 = 20_000_000;

struct Out(Handle);

impl Out {
    fn write(&self, bytes: &[u8]) {
        if self.0 .0 == 0 {
            debug_write(bytes.as_ptr(), bytes.len());
        } else {
            let _ = channel_write(self.0, bytes.as_ptr(), bytes.len());
        }
    }

    fn line(&self, parts: &[&[u8]]) {
        let mut buf = [0u8; 192];
        let mut at = 0;
        for part in parts.iter().chain([&b"\n"[..]].iter()) {
            let take = part.len().min(buf.len() - at);
            buf[at..at + take].copy_from_slice(&part[..take]);
            at += take;
        }
        self.write(&buf[..at]);
    }
}

struct Dec([u8; 20], usize);

fn dec(mut value: u64) -> Dec {
    let mut digits = [0u8; 20];
    let mut len = 0;
    loop {
        digits[19 - len] = b'0' + (value % 10) as u8;
        value /= 10;
        len += 1;
        if value == 0 {
            break;
        }
    }
    Dec(digits, len)
}

impl Dec {
    fn bytes(&self) -> &[u8] {
        &self.0[20 - self.1..]
    }
}

fn hex32(value: u32) -> [u8; 8] {
    let mut out = [0u8; 8];
    for (i, byte) in out.iter_mut().enumerate() {
        let nibble = (value >> (28 - 4 * i)) & 0xF;
        *byte = b"0123456789abcdef"[nibble as usize];
    }
    out
}

fn parse_u32(text: &[u8]) -> Option<u32> {
    if text.is_empty() || text.len() > 10 {
        return None;
    }
    let mut value = 0u32;
    for &digit in text {
        if !digit.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((digit - b'0') as u32)?;
    }
    Some(value)
}

/// The expected byte at `offset` for this nonce. Different roles/nonces pick different
/// bytes at the same VA, so a stale translation cannot pass a byte check by luck.
fn pattern(nonce: u32, offset: usize) -> u8 {
    (nonce
        .wrapping_mul(131)
        .wrapping_add((offset as u32).wrapping_mul(17))
        ^ 0x5a5a_00ff) as u8
}

/// FNV-1a over the whole expected page, printed so the harness can prove the two live
/// processes expect different data at the same VA.
fn expected_sum(nonce: u32) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    let mut i = 0usize;
    while i < PAGE {
        hash = (hash ^ pattern(nonce, i) as u32).wrapping_mul(0x0100_0193);
        i += 1;
    }
    hash
}

fn fill_vmo(vmo: Handle, nonce: u32) -> bool {
    let mut buf = [0u8; CHUNK];
    let mut offset = 0usize;
    while offset < PAGE {
        for (i, byte) in buf.iter_mut().enumerate() {
            *byte = pattern(nonce, offset + i);
        }
        if vmo_write(vmo, offset as u64, buf.as_ptr(), CHUNK) != 0 {
            return false;
        }
        offset += CHUNK;
    }
    true
}

/// Map a role-private stack (`a` at 0x1900_0000, `b` at 0x1901_0000) and return its top.
fn map_private_stack(role: u8) -> Option<u64> {
    let base = PRIVATE_STACK_BASE
        + if role == b'a' {
            0
        } else {
            PRIVATE_STACK_STRIDE
        };
    let raw = vmo_create(PRIVATE_STACK_SIZE as u64);
    if raw == 0 || raw > i32::MAX as u64 {
        return None;
    }
    let vmo = Handle(raw as u32);
    let status = vmar_map(
        Handle(0),
        vmo,
        0,
        base,
        PRIVATE_STACK_SIZE as u64,
        (VmarFlags::READ | VmarFlags::WRITE).0,
    );
    let _ = handle_close(vmo);
    (status == 0).then_some(base + PRIVATE_STACK_SIZE as u64)
}

#[cfg(target_arch = "aarch64")]
unsafe fn jump_to_stack(
    entry: extern "C" fn(u64, u64, u64) -> !,
    sp: u64,
    role: u64,
    nonce: u64,
    stdout: u64,
) -> ! {
    unsafe {
        core::arch::asm!(
            "mov sp, {sp}",
            "br {entry}",
            sp = in(reg) sp,
            entry = in(reg) entry as usize,
            in("x0") role,
            in("x1") nonce,
            in("x2") stdout,
            options(noreturn, nostack),
        );
    }
}

#[cfg(target_arch = "aarch64")]
extern "C" fn conc_entry(role: u64, nonce: u64, stdout: u64) -> ! {
    let out = Out(Handle(stdout as u32));
    let ok = conc(&out, role as u8, nonce as u32);
    process_exit(if ok { 0 } else { 1 });
}

/// Create a fresh anonymous VMO, write this nonce's pattern, and map it at `FIXED_VA` in
/// this process. The VMO handle is closed on every failure path.
fn map_page(out: &Out, what: &[u8], nonce: u32) -> Option<Handle> {
    let raw = vmo_create(PAGE as u64);
    if raw == 0 || raw > i32::MAX as u64 {
        out.line(&[b"asidpair: FAIL ", what, b" detail=vmo"]);
        return None;
    }
    let vmo = Handle(raw as u32);
    if !fill_vmo(vmo, nonce) {
        let _ = handle_close(vmo);
        out.line(&[b"asidpair: FAIL ", what, b" detail=vmo-write"]);
        return None;
    }
    let status = vmar_map(
        Handle(0),
        vmo,
        0,
        FIXED_VA,
        PAGE as u64,
        (VmarFlags::READ | VmarFlags::WRITE).0,
    );
    if status != 0 {
        let _ = handle_close(vmo);
        out.line(&[
            b"asidpair: FAIL ",
            what,
            b" detail=vmar-map status=",
            dec(status).bytes(),
        ]);
        return None;
    }
    Some(vmo)
}

/// Read all 4096 bytes through the mapping and compare each to this nonce's pattern.
/// Returns (stale bytes, first offset, first got, first want).
fn check_page(nonce: u32) -> (u64, u64, u8, u8) {
    let base = FIXED_VA as *const u8;
    let mut stale = 0u64;
    let mut first_at = u64::MAX;
    let mut got = 0u8;
    let mut want = 0u8;
    let mut i = 0usize;
    while i < PAGE {
        // Volatile: every iteration must touch the mapped page, not a cached register.
        let value = unsafe { core::ptr::read_volatile(base.add(i)) };
        let expected = pattern(nonce, i);
        if value != expected {
            if stale == 0 {
                first_at = i as u64;
                got = value;
                want = expected;
            }
            stale = stale.saturating_add(1);
        }
        i += 1;
    }
    (stale, first_at, got, want)
}

fn conc(out: &Out, role: u8, nonce: u32) -> bool {
    let what: &[u8] = if role == b'a' { b"conc-a" } else { b"conc-b" };
    let Some(vmo) = map_page(out, what, nonce) else {
        return false;
    };
    let sum = expected_sum(nonce);
    out.line(&[
        b"asidpair: begin conc role=",
        &[role],
        b" nonce=",
        dec(nonce as u64).bytes(),
        b" va=0x18000000 sum=0x",
        &hex32(sum),
    ]);
    let (pre_stale, pre_at, pre_got, pre_want) = check_page(nonce);
    if pre_stale != 0 {
        out.line(&[
            b"asidpair: FAIL ",
            what,
            b" detail=premapping stale=",
            dec(pre_stale).bytes(),
            b" at=",
            dec(pre_at).bytes(),
            b" got=",
            dec(pre_got as u64).bytes(),
            b" want=",
            dec(pre_want as u64).bytes(),
        ]);
        let _ = handle_close(vmo);
        return false;
    }
    if role == b'a' && !sleep_ns(SLEEP_A_NS) {
        out.line(&[b"asidpair: FAIL ", what, b" detail=sleep"]);
        let _ = handle_close(vmo);
        return false;
    }
    let budget = if role == b'a' {
        STRESS_A_NS
    } else {
        STRESS_B_NS
    };
    let begin = clock_get();
    let deadline = begin.saturating_add(budget);
    let mut rounds = 0u64;
    let mut stale = 0u64;
    let mut gaps = 0u64;
    let mut first_at = u64::MAX;
    let mut first_got = 0u8;
    let mut first_want = 0u8;
    let mut last_seen = begin;
    loop {
        let (mismatches, at, got, want) = check_page(nonce);
        if mismatches != 0 {
            if stale == 0 {
                first_at = at;
                first_got = got;
                first_want = want;
                // Report the very first stale read at once: a defective kernel may wedge the
                // machine before the loop's end line, and the harness must see the mismatch.
                out.line(&[
                    b"asidpair: FAIL ",
                    what,
                    b" detail=stale stale=",
                    dec(mismatches).bytes(),
                    b" at=",
                    dec(at).bytes(),
                    b" got=",
                    dec(got as u64).bytes(),
                    b" want=",
                    dec(want as u64).bytes(),
                ]);
            }
            stale = stale.saturating_add(mismatches);
        }
        rounds = rounds.saturating_add(1);
        if rounds % CLOCK_EVERY == 0 {
            let now = clock_get();
            if now.saturating_sub(last_seen) > GAP_NS {
                gaps = gaps.saturating_add(1);
            }
            last_seen = now;
            if now >= deadline {
                break;
            }
        }
    }
    let end = clock_get();
    out.line(&[
        b"asidpair: end conc role=",
        &[role],
        b" rounds=",
        dec(rounds).bytes(),
        b" stale=",
        dec(stale).bytes(),
        b" gaps=",
        dec(gaps).bytes(),
        b" begin-ns=",
        dec(begin).bytes(),
        b" end-ns=",
        dec(end).bytes(),
    ]);
    if stale != 0 {
        out.line(&[
            b"asidpair: FAIL ",
            what,
            b" detail=stale stale=",
            dec(stale).bytes(),
            b" at=",
            dec(first_at).bytes(),
            b" got=",
            dec(first_got as u64).bytes(),
            b" want=",
            dec(first_want as u64).bytes(),
        ]);
        let _ = handle_close(vmo);
        return false;
    }
    let _ = handle_close(vmo);
    out.line(&[b"asidpair: PASS conc role=", &[role]]);
    true
}

fn remap(out: &Out, nonce: u32) -> bool {
    let mut total = 0u64;
    let mut generation = 1u32;
    let mut current = nonce;
    while generation <= 3 {
        let Some(vmo) = map_page(out, b"remap", current) else {
            return false;
        };
        let (stale, at, got, want) = check_page(current);
        total = total.saturating_add(stale);
        out.line(&[
            b"asidpair: remap gen=",
            dec(generation as u64).bytes(),
            b" nonce=",
            dec(current as u64).bytes(),
            b" stale=",
            dec(stale).bytes(),
            b" sum=0x",
            &hex32(expected_sum(current)),
        ]);
        if stale != 0 {
            out.line(&[
                b"asidpair: FAIL remap detail=stale gen=",
                dec(generation as u64).bytes(),
                b" at=",
                dec(at).bytes(),
                b" got=",
                dec(got as u64).bytes(),
                b" want=",
                dec(want as u64).bytes(),
            ]);
        }
        let status = vmar_unmap(FIXED_VA, PAGE as u64);
        let _ = handle_close(vmo);
        if status != 0 {
            out.line(&[
                b"asidpair: FAIL remap detail=unmap gen=",
                dec(generation as u64).bytes(),
                b" status=",
                dec(status).bytes(),
            ]);
            return false;
        }
        generation += 1;
        current = current.wrapping_add(GEN_STEP);
    }
    if total != 0 {
        out.line(&[
            b"asidpair: FAIL remap generations=3 stale=",
            dec(total).bytes(),
        ]);
        return false;
    }
    out.line(&[b"asidpair: PASS remap generations=3 stale=0"]);
    true
}

fn reuse(out: &Out, nonce: u32) -> bool {
    let Some(vmo) = map_page(out, b"reuse", nonce) else {
        return false;
    };
    out.line(&[
        b"asidpair: reuse begin nonce=",
        dec(nonce as u64).bytes(),
        b" va=0x18000000",
    ]);
    let (stale, at, got, want) = check_page(nonce);
    let sum = expected_sum(nonce);
    out.line(&[
        b"asidpair: reuse end nonce=",
        dec(nonce as u64).bytes(),
        b" stale=",
        dec(stale).bytes(),
        b" sum=0x",
        &hex32(sum),
    ]);
    let _ = handle_close(vmo);
    if stale != 0 {
        out.line(&[
            b"asidpair: FAIL reuse nonce=",
            dec(nonce as u64).bytes(),
            b" at=",
            dec(at).bytes(),
            b" got=",
            dec(got as u64).bytes(),
            b" want=",
            dec(want as u64).bytes(),
        ]);
        return false;
    }
    out.line(&[b"asidpair: PASS reuse nonce=", dec(nonce as u64).bytes()]);
    true
}

#[no_mangle]
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
    let boot = startup(Handle(bootstrap as u32));
    let out = Out(boot.stdout.unwrap_or(Handle(0)));
    let mut argv_buf = [0u8; 256];
    let read = boot
        .argv
        .is_some_and(|argv| vmo_read(argv, 0, argv_buf.as_mut_ptr(), argv_buf.len()) == 0);
    let mut args = unpack_argv(if read { &argv_buf[..] } else { &[] }).skip(1);
    let mode: &[u8] = args.next().unwrap_or(b"");
    let ok = if mode == b"conc" {
        match (args.next(), args.next().and_then(parse_u32)) {
            (Some(role), Some(nonce)) if role == b"a" || role == b"b" => {
                match map_private_stack(role[0]) {
                    Some(sp) => unsafe {
                        jump_to_stack(
                            conc_entry,
                            sp,
                            role[0] as u64,
                            nonce as u64,
                            out.0 .0 as u64,
                        )
                    },
                    None => {
                        out.line(&[b"asidpair: FAIL conc detail=private-stack"]);
                        false
                    }
                }
            }
            _ => {
                out.line(&[b"asidpair: FAIL usage conc <a|b> <nonce>"]);
                false
            }
        }
    } else if mode == b"remap" {
        match args.next().and_then(parse_u32) {
            Some(nonce) => remap(&out, nonce),
            None => {
                out.line(&[b"asidpair: FAIL usage remap <nonce>"]);
                false
            }
        }
    } else if mode == b"reuse" {
        match args.next().and_then(parse_u32) {
            Some(nonce) => reuse(&out, nonce),
            None => {
                out.line(&[b"asidpair: FAIL usage reuse <nonce>"]);
                false
            }
        }
    } else {
        out.line(&[b"asidpair: FAIL usage <conc|remap|reuse>"]);
        false
    };
    process_exit(if ok { 0 } else { 1 });
}
