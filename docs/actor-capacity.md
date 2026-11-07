# Resident actor capacity (AX4)
<!-- j825 -->

AX4 is PLAN/021's capacity slice: a Job policy bound that starts at the compile-time ceiling
(`RESIDENT_MAX = 288`) and can be configured within that ceiling, per-space admission against that bound, slot
reuse and frame reclamation at teardown, and a wide census view (`AXB2`/`AXR2`) that reports
the true resident set. It proves the bound with 256 heap-free actors per cycle, five cycles
over, a scheduler series at the ceiling, a first-wake latency series, and one over-capacity
refusal followed by recovery.

## Workload

`userland/ax4idle` is the measured actor (AXP3). It is deliberately heap-free: a 16 KiB
stack, exactly two transferred channels and one Port, one 120 s deadline timer, and a q31 SIMD
canary held across the blocking wait. The deadline is deliberately far beyond any measured
window: the kernel may run admitted children while the fill is still spawning, so a short
deadline would let fill children self-exit before the boundary census. It sends a 32-byte `AXP3` ready frame on start, parks in
`PortWait`, and on a wake byte immediately performs `ClockGet` and reports the clock it
observed. Only the first wake produces a `W` frame; later wake bytes are absorbed so a
broadcast does not multiply reports. `Q` or a peer close ends the actor.

`ax4idle` is the `ax4idle` binary in the proof initrd; `sora` runs `ax4bench`, which is opt-in
(login's autoexec appends the command, exactly one benchmark per boot). Sora preallocates every
frame and vector before the first measured window so Sora-heap growth cannot issue kernel
frames inside a cycle.

## Wire views

`AXB1`/`AXR1` stay frozen and byte-identical for AX0's `actorbench`. AX4 adds `AXB2` on the
same five-byte query (`AXB2` + op byte), answered on Sora's root channel:

- `AXR2` is a fixed 2400-byte frame: `AXR2` magic, seven u32 (`enabled`, `capacity`, `live`,
  `stored`, `blocked`, `ready`, `running`), eight u64 in AXR1 order (`frames_issued`,
  `heap_used`, `roots`, `root_ns`, `root_min_ns`, `root_max_ns`, `syscalls`, `switches`), then
  all 288 process-koid slots, zero after `live`.
- The decoder enforces the exact length, `1 <= capacity <= 288`, `live <= stored <= capacity`,
  blocked/ready/running partitioning `live`, and exactly `live` koids packed into `[0, live)`;
  anything else is refused rather than half-adopted.
- Ops 0..=8 keep AX0's meanings. Ops 9..=13 sample the private dispatcher with
  `16/32/64/128/256` runnables over 32768 decisions each and report the elapsed total.

`JobSetPolicy` (syscall 57) takes an explicit Job handle in `x0` and a full-width
limit in `x1`. Sora receives the non-transferable root Job MANAGE handle in bootstrap
`x10`, preserved by `_start` before scratch-register use. The engine requires Job type,
MANAGE rights and ownership by the caller before reading or changing policy. Missing,
wrong-type, narrowed and foreign Job capabilities are rejected. `Ok` accepts `1..=288`;
`InvalidArgs` (-1) rejects zero, above 288 and values with high bits set; `BadState` (-11)
rejects a bound below the already-live count. Only Sora's root syscall chain supports this
first policy mechanism. The root Job passes the value to later children, and the live
admission gate is mirrored into `UserSched`.

## Settle and census-stable quiescence

Before entering either `ax4bench` or `ax4gate`, Sora's timer callback waits for the
boot GPU client and any pending GPU restart timer to finish. It rearms a one-second timer
and returns to the ordinary serve loop, which continues serving the display broker and
retirement replies. The wait fails loudly after 360 s; interactive commands use the same
path. `AX4 boot-services ready` records the transition. This avoids having the synchronous
benchmark itself block display retirement and force the client's 120 s timeout.

The benchmark is measured only after login services quiesce. The AX2-derived gate samples the
AXR2 syscall counter twice 100 ms apart and requires a delta of at most 100, retrying for
10 s. A quiet counter is not enough on metal: the display/GPU bring-up is still executing its
own work during the first seconds after login and can change the resident set or the kernel
heap while the benchmark is running. The settle therefore adds a census-stable window: every
250 ms it samples the AXR2 snapshot and requires `live`, `stored`, `blocked`, `ready`,
`running` and the live process-koid list to be unchanged for 150 s, restarting the window on
any change and giving up loudly after 360 s. The last quiescence sample is the default-login
census, so the heap anchor for the final 64 KiB gate is taken only after the resident set has
held still; no extra census query sits between the window and the anchor. The harness
independently requires the quiescence samples, their consecutive order, a final stable window
of at least 150000 ms with a wait within the 360000 ms budget, and an `AX4 settle ok` after
them.

Metal boot A (2026-11-06) shows the failure mode. The first bring-up boot took the default
census at 19:54:38.695, before the display R1/R2 work (through 19:54:50) and the drv-gpu
session switch at 19:55:10; cycle 1 absorbed +185,664 B of bring-up kernel heap, and cycle 2
failed state/koid equality when the switch retired a process mid-cycle. That switch is
31.7 s after the settle began. A later boot exposed the longer-latency hazard: the boot
gpu-client exits ~120 s after the settle starts, at 19:56:38 and 20:19:29 on the two metal
boots. A 35 s window anchored the default census before that exit; the boundary phase then
saw the client leave after a clean 279/279 ready fill (the census read live=287 stored=288),
so the over-limit spawn was legally admitted with status 0, state/koid equality broke, and
recovery/final read live=9/8. The window is 150 s: it restarts at the ~120 s exit and
completes ~270 s after the settle began, inside the 360 s retry budget, so the anchor is the
steady post-bring-up set. The conservative 150 s window is retained after explicit boot-service readiness;
these earlier failed captures are evidence of baseline contamination, not passing proofs.

## Heap-stable clean settle

A matching process census is necessary but does not account for retained storage in live
IPC queues. Metal initially showed positive cycle heap deltas of roughly 16–376 KiB even
with exact state/koid equality. The initial deferred-stack explanation did not survive a
rerun: the settled heap grew cumulatively. Long-lived channel inbox vectors retained burst
capacity after messages drained; reclaiming only dead channel pairs could not release it.
`ChannelPair::read` now releases sparse inbox capacity with the same geometric slack band
as the registry. A 1024-message regression verifies ordering, drain, bounded retention and
reuse of the live channel. The heap allowance remains 64 KiB.

The clean settle now runs after every teardown. It waits until the resident set (capacity,
live, stored, the three state counts and the live koid list) equals the pre-spawn baseline,
then samples AXR2 every 100 ms and requires `heap_used` to stay unchanged within 4 KiB for
500 ms, retrying for up to 10 s. The settled sample is the cycle's clean census and the
source of its heap-delta; a settle that never reaches the window fails loudly and fails the
cycle. The line is printed immediately before each cycle line:

    AX4 clean-settle cycle=<n> stable-ms=<n> waited-ms=<n> heap=<n> change=<0|1>

The harness requires one such line per cycle, in consecutive per-cycle order, immediately
before the corresponding `AX4 cycle=` line, with `stable-ms >= 500` and `change=0`. The
boundary's final census uses the same settle with the default-login census as the baseline,
prints `AX4 clean-settle final stable-ms=<n> waited-ms=<n> heap=<n> change=<0|1>` before
`AX4 census final`, and the 64 KiB per-cycle and final allowances and the clean-census rule
are unchanged. The settle does not hide a leak: a residual that is still above the allowance
at the settled sample fails exactly as before.

## Flat-census rules

Each cycle spawns one batch of `AX4_BATCH = 256` actors, collects their ready frames through a
single Port binding (one 15 s timer for the whole batch), snapshots the peak, tears down
(`Q` to all, bounded reads to `PEER_CLOSED`, `ProcessKill` for stragglers, `ProcessWait`
pumps), and snapshots the cleaned census.

The clean snapshot must equal the pre-spawn snapshot exactly: capacity, live, stored, the
three state counts, and the full process-koid list. `frames_issued` must not move. AX0's
contamination rule is preserved unchanged: only cycle 1 may absorb a non-zero frame delta
(login-service teardown can free frames), and only when the kernel census is otherwise clean.
Such a cycle prints `contaminated=1`; cycles 2..5 must have `frames-delta=0`.

Kernel-heap drift is gated, not merely recorded. The kernel heap is a fixed 8 MiB first-fit
arena, and teardown must return every Process/Thread/Channel/Port/Timer allocation the cycle
created. Every cycle's `heap-delta` must stay within a documented 64 KiB allowance. Measured
with exact reclamation after the heap-stable clean settle: QEMU shows cycle 1 carrying
~32 KiB of one-time Vec capacity (children, wait queue, VMOs, handle tables) and later
cycles within 2 KiB. The pre-fix leak was 213-535 KiB *every* cycle, far above the bound. A larger residual means kernel objects
outlived their actors; the cycle prints `FAIL`, the harness independently rejects the
transcript, and the run cannot pass even if the state and koid comparisons are clean.

The final census applies the same 64 KiB allowance end to end: after the boundary fill,
refusal and recovery are torn down, the kernel heap must be back within 64 KiB of its
default-login level. The boundary's one-time capacities are released by the same teardown
path; an intermediate build left 68,408 B and correctly failed this gate until the kernel's
exact-capacity reclamation landed.

## Refusal

The boundary phase fills the policy capacity (spawn `capacity - live` more actors), confirms
the full census, then attempts exactly one more spawn. The raw `ProcessRun` status is printed
additively by `run_elf_threads_status`; the expected status is `BadState` (-11), not
`ShouldWait`, and the refused spawn must leave the census and koid list unchanged. Teardown
follows, then a later spawn must succeed and show `live + 1`, proving the policy cap is the
gate rather than leaked slots. The refusal phase does not require a zero frame delta because
the ELF loader may issue frames before admission is decided. A refused or partial load
now invokes creator-authorized teardown before closing its handles, including the
zero-thread case, so its process record, mappings, ASID and backing do not remain resident.

The phase prints diagnostic progress markers (`AX4 boundary spawn-done`, `AX4 boundary
ready`, `AX4 boundary census`, `AX4 boundary over-limit attempt`) so a wedge can be localized
to the spawn, the collection, or the census. They carry no gate; the gated lines remain
`boundary fill`, `refusal` and `boundary recover`. `sora ax4bench fill` runs only this phase
after the settle gate — a development diagnostic for capacity wedges, never the proof.

## Scheduler and wake series

The scheduler series prints one line per runnable size:

    AX4 scheduler runnable=<n> decisions=32768 total-ns=<n>

The wake series measures `ClockGet` bracket to child-reported clock for `N` in
`{16,32,64,128,256}`. Each mode gets one bracket: first a single target (`child[0]`), then a
broadcast to the still-parked `N-1` children. Reported are samples, min, median and max; the
broadcast line also reports whether the census returned to the pre-wake state. Latencies are
diagnostic measurements, not policy gates. A sentinel (`u64::MAX`) or a minute-long wake is a
failure. The `clean` flag is an anchor check, not a wake-loss check. On the earlier metal
boot A every broadcast reported `clean=0` because `base` was the pre-bring-up default census
(`live=10`) while the drv-gpu session switch left the steady set at `live=9`; every broadcast
still collected all `N-1` wake frames and passed. The harness correctly rejected that
transcript; the post-bring-up 150 s settle now makes the anchor the steady set, so a metal
capture must report `clean=1` rather than the flag being waived.

## Harness

`scripts/actor-capacity.py` clones the AX0/AX2 harness shape: it builds a private proof image
with `KUMO_CONSOLE_LOGLEVEL=6`, checks the `sora` and `ax4idle` ELFs against their initrd
entries, appends `ax4bench` to a private ESP copy's `etc/autoexec` (the append must stay within
Sora's 256-byte autoexec read), and boots OVMF/QEMU twice. The parser rejects a transcript that
lacks or duplicates any required line, any `AX4 FAIL`, a TOWER, a panic, a measurement before
login, or a phase out of order. The manifest records the image/ELF hashes, per-cycle rows,
capacity rule and both boot reports. The default image is rebuilt, checked and hashed on every
exit, success or failure.

Every wait is bounded twice: `--timeout` (default 420 s) caps a boot, and `--stall` (default
90 s) fails a boot whose serial has stopped growing. The 420 s budget covers a boot whose
150 s settle restarts once at the ~120 s gpu-client exit. Both paths terminate QEMU in `finally`
and report the serial path, so a silent 99%-CPU spin costs the stall bound rather than the
boot budget.

`--serial PATH` parses one metal capture and additionally enforces the metal-capacity rule:
the reported policy capacity must fit the single 256 batch, the login census, and two spare
slots (`capacity - live >= 256 + 2`). A login profile that cannot satisfy this is rejected
rather than reported as a capacity result. `--self-test` exercises the parser's rejection
rules without QEMU, including transcripts with kernel-heap leaks.

## Post-AX4 gate (J819)

The capacity gate that motivated AX4 is `sora ax4gate`: it samples the default-login census,
starts `netstack`, starts one `netprobe hold` client, and samples again. The gate requires
`free >= 2` after the client is resident. After `netprobe hold` completes, `gate-clean`
must show the default count plus the still-supervised netstack (one extra actor), with
that service's process koid present:

    AX4 capacity-gate free=<f> requires=2 PASS|AX4-REQUIRED

Before AX4 this sequence failed because the resident limit was the AX0 ceiling minus hand
reserves; with the policy bound and true census it must pass. The default-login census is also
where the metal rule is applied, so an oversized login profile cannot masquerade as a capacity
result.

## Limits and integration assumptions

- One benchmark per boot; the command runs after login services settle: the AX2 syscall gate
  (two samples 100 ms apart, IPC syscall delta <= 100, 10 s retry), then the census-stable
  quiescence window (150 s unchanged, 250 ms samples, 360 s retry). Both fail loudly, and
  the window's last sample is the default-login census.
- Actor admission count is per address space (`UserSched`), so the 256 batch is per-Sora.
- `AXB1` remains the AX0 view and still answers `BadState` when the true census does not fit
  its 12 slots; AX0's `actorbench` and `actor-call`'s AX2 path must stay green unchanged.
- The AXR2 process array is fixed 288 slots; a future change to `RESIDENT_MAX` changes the
  frame length, so the decoder's const assert must move with the ABI.

## Stable IPC identities

Channel reclamation compacts the registry. Kernel-retained root-channel IDs and pending
AX2 calls therefore use endpoint koids, never vector positions. A regression closes an
older unrelated pair while a call is pending, then proves both root routing and the
request/reply completion survive the move. This keeps capacity cleanup from orphaning a
call or routing console/storage traffic to another pair.

## Complete VMO ownership view

Sora lives outside the engine's child process table. A child dispatch therefore cannot
safely retire VMOs: it cannot see an argv grant still held by Sora while she starts a
network session. That pass is deferred until a root dispatch or teardown includes Sora's
handle table and mappings. A regression proves a child syscall preserves the root-held
VMO, and that the final root close still reclaims it. AXH exercises the actual launch.

At the capacity boundary, the ready frame precedes an actor's park and proof logging can
wake the framebuffer driver. Both refusal snapshots therefore wait, for at most 10 s,
until all counted actors are idle. Exact state and koid equality is still required.

## Measurement console traffic

Metal tracing identified the main residual: the live console inbox held over 5,000 small
spawn diagnostics after five batches. Per-process logging used four channel messages, and
the framebuffer consumes each with a presentation/frame wait. Those queued payloads and
inbox slots were real live memory, not actor stacks awaiting reclamation. The capacity
probe now suppresses successful per-actor koid chatter through the loader's explicit
`log_spawn` option; all aggregate AX4 proof lines and all loader failures remain visible.
Ordinary launches keep their existing diagnostics. The 64 KiB heap gate is unchanged.

Parse each fresh-boot gate capture independently with
`python3 scripts/actor-capacity.py --gate-serial <serial.log>`. It checks the census
sequence, a successful hold client, the two-spare threshold and the retained netstack.

## Closing evidence (J825, 2026-11-06)

The final Orange Pi proof passed five 256-actor cycles. Heap deltas were
34872 / 1808 / -384 / -288 / -272 B; only the first cycle issued frames (2539),
with zero new frames in cycles 2–5. Final heap was 650784 -> 683600 B (+32816).
The fill reached 288/288, the extra spawn returned BadState (-11), and recovery
succeeded. Two final QEMU boots passed the same contract. Two independent metal
network censuses each saw 9 default actors, 11 with netstack/client and 277 spare
slots; after client completion, 10 live actors retained the supervised netstack.
Artifacts: `.claude/worktrees/ax1-int/build/verification/ax4/` in the main checkout.
