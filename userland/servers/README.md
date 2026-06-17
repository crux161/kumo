# userland/servers — the Siyu service plane

This tree is the home of KUMO's userspace **servers** — the services that, per the
microkernel thesis (`PLAN §5.2`, §5.4 Stage C), live *outside* the kernel as isolated,
capability-confined, supervised processes. It is the concrete realization of the **Siyu**,
the four cardinal domain wardens (`PLAN §12`, Appendix Γ): **Houtu** (storage), **Gouchen**
(devmgr/hardware), **Taiyi** (network), **Nanji** (graphics/HID).

> **Status:** this tree is *newly opened.* Today most services still run through Sora /
> kernel bootstrap (Stage B). Closing the plan-vs-code gap means migrating them out here,
> one server at a time, each with its own crate, protocol, tests, and minimal capability set.

## The first server — and the template

[`svc-health`](svc-health/) is the first real extraction and the **template every future
server clones**:

- a typed `Request`/`Response` protocol with a byte wire-format (IPC messages are bytes,
  `PLAN §19.2`);
- a pure, fully host-tested `dispatch(raw) -> reply` (per `PLAN §9`, server logic is
  arch-neutral and tested before it ever runs on metal — `cargo test -p svc-health`);
- a freestanding `svc-health` binary that Sora spawns as a separate process, running the
  same `serve` loop that host tests exercise.

To add a server: copy `svc-health`'s shape, change the protocol and the granted
capabilities, add it to the workspace `members`, and host-test the dispatch logic. The
serve loop is identical; only the authority differs.

## The rules every server here obeys

- **No ambient authority.** A server holds only the handles Sora grants it (`PLAN §5.1`).
- **A recovery class.** Declare it (`DESIGN/002`): stateless / disk-backed / VMO-checkpointed.
  `svc-health` is *stateless* — a restart just resets its advisory counters.
- **Sterile names in code.** The warden names (Houtu, …) are VEIL — docs/logs only, never
  Rust identifiers (`PLAN §20`; the `xtask preflight` tripwire enforces this).
- **Run the ritual before you submit.** See [`GUIDANCE/006`](../../GUIDANCE/006-contributor-harmony-preflight.md).

## Current integration

The serve loop (`svc_health::serve` over a `Transport`) is host-tested end-to-end
(`cargo test -p svc-health`'s `end_to_end_*`). The image path now also builds a freestanding
`svc-health` binary into the initrd. Sora spawns **two** `svc-health` servers as independent
resident processes, each granted its own request channel via async `process_run` and binding
it to its own `Port`. Both block in `PortWait`; the scheduler harness holds them in a
bounded `children` collection and parks each in a `WaitQueue` keyed by object koid. Each
client write wakes **only** the server that owns that port — never the other — and each
server's own `served` counter independently reaches 2 from its own `Ping` + `Status`,
proving separate per-process state (Journal 134).

That proves two services running as their **own processes**, not inside Sora, woken
independently — Stage-C migration past a single service. Sora then drives each to terminate
with a `Shutdown` request: the serve loop exits, the process `process_exit`s, and the harness
reaps it. Shutting down one resident leaves the other serving (independent lifecycle), and
once both are down `ProcessWait` reports no resident remaining — the detection half of
`DESIGN/002` supervised restart (Journal 135). Sora then performs the **first supervised
restart** (Journal 136): it holds each server's construction recipe (`SvcRecipe` — for the
stateless `svc-health`, just where to find its ELF), and after a server is shut down and
reaped it respawns a fresh instance from that recipe and re-verifies it serves with reset
state (`served: 1`, not continuing the dead instance's count). And a server that *faults* no
longer halts the kernel: an EL0 fault is **contained** — the kernel terminates just that
process and the supervisor lives on (Journal 137, `DESIGN/002 §5.6`). Verify with
`./scripts/preflight.sh`.

## Next integration step

Restart-on-crash: wire `TERMINATED`/`PEER_CLOSED` so a server that faults wakes Sora via
`object_wait_many` (not just the reap poll), then respawn the crashed server from its recipe
on that signal — followed by per-`Job` restart policy + backoff (`DESIGN/002 §5`).
