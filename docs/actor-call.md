# Actor call fast path (AX2)
<!-- j824 -->

AX2 is PLAN/021's implementation of PLAN/009 **A2**: `ChannelCall` (syscall 6) hands a
32-byte request to a parked receiver in registers, switches to it directly, and donates the
caller's remaining time slice; `ChannelReply` (syscall 56) carries the reply back the same
way. The campaign must either beat the AX0/S0 message path on metal or not land. This
document is the method record for the probe, the two-boot harness and the metal criterion.
The policy decisions behind the ABI (depth 1, no new errnos, legacy single-outstanding
`ChannelWrite` fallback, deadline sweep) are in `build/verification/ax2/DECISIONS.md`;
the kernel mechanism is the kernel half of j824.

## Run it

QEMU proof (builds its own private proof image, two boots, restores the default image):

```sh
python3 scripts/actor-call.py
python3 scripts/actor-call.py --output build/verification/ax2/run-qemu
```

The harness strips proof controls from the environment, builds
`build/images/qemu-virt-aarch64` with `KUMO_CONSOLE_LOGLEVEL=6`, checks the initrd against
`target/aarch64-unknown-none/release/{sora,actorcall}`, copies the ESP twice, and appends
`ax2bench` to a private `etc/autoexec` (staying within Sora's 256-byte read). Every other
initrd entry must stay byte-identical. Each boot is one OVMF `qemu-system-aarch64` run with
no NIC; both must parse. On every exit, success or failure, the harness rebuilds the default
image with the proof controls removed, requires the default autoexec back (no `ax2bench`,
`echo kumo autoexec online` present) and records its SHA-256 in `manifest.json`. Artifacts,
per-boot serial logs, `command.json`, `report.json` and the manifest stay under
`build/verification/ax2/run-*`.

One metal capture (enforces the metal criterion on that boot):

```sh
python3 scripts/actor-call.py --serial build/verification/ax2/metal1/serial.log
```

On Orange Pi, append `ax2bench` to a private copy of the ordinary image's autoexec, serve
and boot it through the existing PXE method, capture serial, and run `--serial` on each of
the two boots of the same image. Do not boot `actorbench` and `ax2bench` from one autoexec:
each arms its own five-second timer and each pair of probe children would put four children
on the machine, against the metal two-child rule. AX0's `actorbench`, `actorprobe` and
statics are unchanged; the AX2 command has its own `AX2_PENDING`/`AX2_TIMER`.

```sh
python3 scripts/actor-call.py --self-test
```

checks the transcript rules without QEMU: order, the ten trial records, matrix results,
census accounting, capacity gate, autoexec staging, and the metal median threshold.

## What is measured

`ax2bench` is opt-in exactly like AX0. It runs five seconds after Sora enters its serve
loop, after login services are resident: the **at-login profile**, not a claim that every
optional boot animation finished. Keep the default services when comparing another image.
One benchmark per boot; a boot with a background job refuses with
`AX2 FAIL needs a fresh boot and no background job`.

Before the baseline census and the recorded series, the driver waits for quiescence: it
takes two cost snapshots 100 ms apart and requires their IPC syscall delta to be at most
100, retrying for up to ten seconds. Each sample is logged as
`AX2 settle sample=<n> syscalls=<n> delta=<n>`; success adds `AX2 settle ok ...`, and a
boot that never drains logs `AX2 settle gave up delta=<n>` followed by `AX2 FAIL settle`.
This is a driver-side guard only: the kernel is untouched, and a genuinely busy peer or a
broken call path still fails the trials and matrices. Metal boot 1 needed it: its first
trial took 3,254 syscalls while GPU-client, xHCI-hub and display probes were still running,
which tripped the 3*rounds+64 gate; trials after the background work drained were clean.

Each trial spawns exactly two `actorcall` children (server first, caller second), uses its
own channel pair and control endpoint, and reads both AXP2 frames. Each child first writes a
ready frame (role `R`) on its control endpoint; Sora reads both before the baseline
snapshot, then releases the caller with a one-byte go (`G`) after that snapshot. The caller
starts its clock only after the go byte. This keeps spawn and handshake costs out of the
measured window and makes the live-count bracket deterministic: the caller is blocked in
the go read and the server in its call read when the baseline is taken, so neither can
exit between the snapshot and the calls (the same shape as AX0's `actor_trial`). Ten
trials: indexes 1-5
at 8 bytes, then 1-5 at 32 bytes, 1,024 sequential round trips each, one outstanding call.
The caller reads the monotonic clock once per run and derives one absolute deadline for the
whole batch, so the measured path stays at call/read/reply rather than adding a ClockGet
per round trip. Sora brackets each trial with `actor_snapshot` calls: one before spawn, one
after both children are admitted (`live` must rise by exactly two), one after the frames
(actor costs), and a clean snapshot after kill/close. Trial PASS requires:

- both AXP2 frames exact: roles, 1,024 rounds, status 0, zero payload errors, zero canary
  errors; the caller's total time positive;
- `frames` delta 0 (no allocator frames issued by the call exchange), SIMD canary errors 0;
- the resident live count back to the pre-spawn value (no leaked probe);
- the syscall and switch deltas inside the gates below.

The four matrices exercise correctness, not speed:

| matrix | children | required witness |
|---|---|---|
| `die` | `ping 1 8 5000` + `die` | caller status `PeerClosed`; the server reports one call, status 0, exits without replying |
| `never` | `ping 1 8 100` + `never` | caller `TimedOut` no earlier than the 100 ms deadline; the late reply is `BadState` |
| `hold` | `ping 3 8 5000` + `hold 75` | three exact replies, caller total >= 3 x 75 ms (a tick is inside each hold), switch delta > 0 |
| `repeat` | `ping 100 8 5000` + `pong` | 100 exact round trips, frame delta 0, live back to the pre-spawn value |

`never` arms a 500 ms timer and binds both the timer and the peer: the caller's 100 ms
deadline fires first, so the server wakes on peer close and still attempts its single late
`ChannelReply` to witness `BadState`. Every child writes a ready frame (role `R`, zero
counters) before entering its role, then one final 32-byte AXP2 record before exit:

```text
0..4  "AXP2"        4 role ('R','p','o','d','n','h','?')        5..8 zero
8..12 rounds u32   12..16 status i32   16..20 errors u32
20..24 simd-errors u32   24..32 total-ns u64
```

## Probe roles and wire transform

`userland/actorcall` is one `no_std`, heap-free binary; argv selects the role:

- `ping <rounds> <bytes> [deadline-ms]` — caller; `bytes` 8 or 32, `rounds` 1..1,000,000,
  default deadline 5,000 ms, maximum 60,000 ms. Deadline 0 means no deadline in the ABI and
  is supported, but the benchmark never uses it.
- `pong` — echo server: read the call, transform, reply with the same length.
- `die` — server: read one call, report, exit without replying.
- `never` — server: read one call, wait for the first of a long timer and peer close, then
  attempt exactly one late reply and report its status.
- `hold <ms>` — server: reply after sleeping `ms` before each reply (Sora uses 75).

Every role writes its ready frame before starting; `ping` then blocks on `c` for the go byte
and reports `BadState` (and a zero-round frame) if Sora closes the control endpoint instead.
The verdict travels in the AXP2 frame, not the exit code: `die`'s expected `PeerClosed` and
`never`'s expected `TimedOut` are nonzero statuses, and the probe still exits 0 so the
kernel supervisor does not log an ABEND tower for a correct run.

A request of `len` bytes (8 or 32) is `seq u32 LE | nonce u32 LE | pattern`. `seq` counts
calls from 1 within a ping run; `nonce` is derived once from the run's start clock. Each
pattern byte mixes sequence, nonce and offset (`pattern_byte`). The reply is byte-for-byte
`reply[i] = request[i].rotate_left(1) ^ 0x3c`. The caller unpacks the four register words
with `kumo_abi::unpack_call_payload`, compares all `len` bytes and never reads padding. A
malformed request is answered with zero bytes so the caller fails on content rather than
hanging. Servers validate both delivery paths — register words and the kernel's copy into
the read buffer — against the pattern, so a missed byte-copy fails loudly.

Every caller SVC runs in one asm block that seeds all 128 bits of q31, executes `svc #0`,
then XORs q31 against a freshly seeded q30 and ORs the halves; any vector clobber makes the
result nonzero. This is the AX0 `actorprobe` canary shape; the call does not go through
`kumo_rt::channel_call` for this reason.

The delivered platform wrapper is `kumo_rt::channel_read_call(handle, &mut [u8]) ->
(i64, usize, [u64; 4])`, returning x0 (byte count; 0 means empty/closed, negative an
error status), x1 (attached handle or 0) and the four register words. The kernel also
copies up to 32 payload bytes into the buffer when one is supplied, so a legacy reader
sees bytes; bytes past the count are undefined. Non-aarch64 builds return
`NotSupported`. `channel_call` and `channel_reply` are the caller/reply wrappers.

## Cost accounting and the metal criterion

PLAN/021 requires the metal median RTT over the ten trials to be **strictly below
53.000 us** (AX0's lower metal boot median), with mean syscalls per RTT <= 3 and mean
switches per RTT <= 2, frame delta 0, SIMD errors 0, the correctness matrices and census
above. The intended mechanism cost is three syscalls (call, read, reply) and two switches
per round trip.

The trial bracket also contains fixed observer work: Sora's two ready reads and two
bounded frame reads, three snapshot calls and the go write, plus the caller's go read — a
constant tens of syscalls and a few switches that do not scale with the message count
(ready frames and spawn are before the baseline snapshot; the go read is after it). Sora and
this harness therefore gate each trial with the identical arithmetic:

```text
syscalls <= 3 * rounds + 64
switches <= 2 * rounds + 64
```

The 64-event allowance is the budget for that observer work; a literal mean of exactly 3
and 2 per RTT over 1,024 rounds would otherwise charge the supervisor's fixed reads as
per-message cost. Reports print the raw `syscalls/RTT` and `switches/RTT` means, and the
manifest records the allowance, so the interpretation is visible rather than hidden. On
metal, quote both the raw means and the median RTT. Do not promise a factor versus
AX0/S0; the harness records `ax0_reference_ns` and `factor_vs_ax0` for the reader.

A missed capacity target must be loud, never adapted around: probes use at most two
concurrent children, and if the login profile leaves fewer than two free resident slots
the benchmark logs `AX2 capacity-gate free=<n> requires=2 AX4-REQUIRED`, then
`AX2 FAIL capacity (AX4-REQUIRED)`. The harness rejects such a boot. QEMU proves the
mechanism, the matrix, the census and the staging; metal proves the RTT criterion,
preemption during a hold, peer death mid-call and the never-answering receiver. Backlog,
handle transfers and forbiddances remain QEMU + kernel-host-test evidence.

## Log and artifact contract

Each trial is one line, and the harness requires exactly ten in this order:

```text
AX2 trial=<i> kind=<8|32> rounds=1024 rtt-total-ns=<n> syscalls=<n> switches=<n> frames=<n> simd-errors=<n> PASS|FAIL
```

The matrices and census follow (`die`, `never`, then `hold` with its census, then `repeat`
with its census), then exactly one final `AX2 PASS` or `AX2 FAIL`. The harness also
requires `autoexec: done` before `INDRA login:` before `AX2 begins:`, no `AX2 FAIL`,
`TOWER` or `PANIC`, the capacity gate before the first trial, and a clean census whose
`free = capacity - live` and state totals match. `report.json` keeps the parsed trials,
matrix results, census and per-boot summary; `manifest.json` records the script, HEAD,
QEMU and firmware hashes, the sora/actorcall ELF hashes, the proof and restored-default
initrd hashes, per-boot hashes and runtimes, and whether the default image was restored.
— TIMBERDOODLE 2026-11-06
