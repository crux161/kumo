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
`svc-health` binary into the initrd. Sora creates a process, grants it one request-channel
handle via async `process_run`, and the child binds that channel to a child-owned `Port`.
The server blocks in `PortWait`, then reads and serves `Ping` and `Status` in separate
scheduling turns. Each write wakes the resident child through the port, and Sora accepts
only `Pong` followed by `Status { served: 2 }`. The scheduler harness now records the
resident child's wait in a small `WaitQueue` indexable by thread koid and object koid
(replacing the single typed slot), so the eventual general per-thread wait queue has one
shape and one set of operations to grow from — it just holds a single entry today.

That proves a service running as its **own process**, not inside Sora — the first
end-to-end Stage-C migration. Verify it with `./scripts/preflight.sh`.

## Next integration step

Let the harness host more than one resident thread in the `WaitQueue` (real per-thread
waits across distinct child koids), then let Sora supervise and reconnect to restarted
services.
