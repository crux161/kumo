# Actor cost baseline (AX0)
<!-- j822 -->

AX0 measures the current implementation before any ASID, IPC, spawn, preemption or capacity
optimization. It changes no scheduling policy or resident limit. Measurements and image
hashes are retained locally with each run; QEMU proves the mechanism, while Orange Pi timings
are the metal baseline. Later changes must use the same method when comparing costs.

## Run it

Build an ordinary AArch64 QEMU image at console loglevel 6, then run:

```sh
KUMO_CONSOLE_LOGLEVEL=6 cargo xtask image --arch aarch64 --hardware qemu-virt-aarch64
python3 scripts/actor-baseline.py --output build/verification/actor-baseline-my-run
```

The command boots two private OVMF copies, appending `actorbench` to the default autoexec
(remaining within its 256-byte read). It preserves every executable and the default script's
commands. No credentials, board access or network device is used. Each process is stopped in
`finally`; the source image is unchanged. `--serial <capture>` applies the same checks to one
metal boot, and `--self-test` checks rejection of incomplete, corrupt and inconsistent data.

On Orange Pi, append the same command to a private copy of the ordinary image's autoexec,
serve it through the existing PXE method, capture serial, and boot the same image twice.
Record kernel and initrd SHA-256 plus the boot profile and device witnesses. An interactive
`actorbench` also works after login on a fresh boot with no background job or prior netstack
launch. `ipcstat actors` reports a resident/frame/counter snapshot without running probes.

## What is measured

The autoexec request schedules the benchmark five seconds after Sora enters its serve loop.
Login services are then resident. Default boot presentation work can still be present; this
is the **at-login profile**, not a claim that every optional boot animation has finished.
Keep this timing and default services when comparing another image. The ordinary AXH suite
(`python3 scripts/netstack-smoke.py`) remains a required regression check.

- **Resident census:** children admitted to the scheduler; Sora and the idle floor excluded.
  `live` excludes done threads, `stored` includes unreaped ones, `free = capacity - live`
  because admission reaps done threads first. State totals and process koids are captured.
  One snapshot precedes any probe/network start; another holds netstack and one real granted
  client (`netprobe hold`, a two-second UDP WAIT ending TimedOut).
- **Spawn to first message:** parent ClockGet before the ordinary initrd ELF loader through
  receipt of the new actor's Ready message. Includes file copy, mapping, admission, first
  scheduling, channel/Port setup and the Ready exchange. Ten spawns in five trials. This is
  not ELF-copy time alone; timer phase and the existing scheduler may dominate its spread.
- **Idle actor frames:** before/after each Ready, count actual physical frames issued by the
  allocator. The probe is one heap-free process with a 16 KiB user stack, two explicit channel
  capabilities, one Port and one deadline. This counter is neither free RAM nor physical-address
  watermark distance. It omits kernel-heap storage, reported separately as a signed system
  delta; reaping prior threads can make that delta negative. Frames currently are not reclaimed
  by this allocator, so no flat physical-memory teardown claim is made. Concurrent device
  bring-up can allocate in the same sampling window: retain and identify those outliers
  rather than attributing every issued frame to the probe.
- **IPC:** two resident processes, 1,024 sequential 64-byte round trips per trial, one outstanding
  request. Every byte and sequence is checked by both peers. Client elapsed time brackets the
  exchange loop; two ClockGet syscalls per trial, rather than per message. S0 syscall/switch
  deltas and root-switch statistics bracket the ping phase plus its fixed diagnostic boundary
  overhead. No payload is processed by a kernel server.
- **Address-space root operation:** opt-in clock brackets around the existing HAL root-switch
  call in the live user-thread dispatcher. The existing flush behavior is unchanged. Report
  count, sum, minimum and maximum raw nanoseconds, including clock-read overhead. A separate
  1,024-pair kernel clock sample reports that overhead; it is not silently subtracted.
  This is the root operation, not a complete trap/context-save/restore cost; end-to-end IPC
  includes the full path. These instrumented timings are not CPU-cycle or frequency claims.
- **Scheduler:** private instances of the actual strict-priority Dispatcher, at 1/2/4/8/12
  runnable tokens (running plus ready). Warm the queue, then 32,768 round-robin yields with
  expected-id checking. Report the raw loop cost including validation/modulo bookkeeping.
  No live thread is admitted and no address space switches in this subtest; it does not
  measure preemption latency or concurrent actor throughput.

The probe checks all 128 bits of q31 across each blocking PortWait SVC/context switch. An
injected post-SVC corruption must fail the benchmark. Each actor has a ten-second deadline;
Sora bounds each diagnostic read to fifteen seconds, closes/terminates owned probes, and
turns root timing off at the end. Probe handles never include hardware or the Sora root.

The fixed AXB1/AXR1 diagnostic exchange uses **Sora's existing root channel**, which ordinary
applications do not hold; it adds no syscall. Snapshotting happens outside SoraState borrows;
channel writes finish and the borrow is dropped before wake. The kernel records no actor
application state. All measurement state is discardable.

## Capacity gate and interpretation

AX0 is allowed to report a missed capacity target. It must not quietly raise the limit.
If netstack plus one client leaves fewer than **two** resident slots, PLAN/021 requires AX4
(or a separately recorded temporary cap change) before further netstack-on-metal work.
The initial Orange Pi run observed 9 live children at login and 11/12 with the client:
**one spare slot, AX4 required**. That boot enumerated no USB2 devices; CRUX confirmed one or
both of ax88 and the USB keyboard were unplugged. This is the attached-device profile, not a
USB enumeration regression claim or a complete NET0 coexistence witness. Recheck capacity and
device witnesses with the intended USB devices before later metal acceptance. A subsequent
boot enumerated ax88 and the keyboard and reached 10 children at login, then 12/12 with the
client: zero spare slots, the same AX4 requirement.

Report each boot's five RTT means, ten spawn latencies, per-actor frame deltas, five raw root
means/ranges, scheduler series, and census. State within-boot min/median/max and the difference
between boot medians. A wide spawn spread is data about this implementation; it is not hidden
by quoting only its fastest launch. QEMU wall/guest timings are not substituted for silicon.
— TIMBERDOODLE 2026-11-06
