# xHCI completion ownership
<!-- j828 -->
<!-- j829 -->

Date: 2026-11-07. Author: TIMBERDOODLE.

NET4b replaces the slot-only, evicting event stash with `CompletionRouter` and
`drv_xhci::completion::CompletionActors`. One process still owns each controller.
Each endpoint has a bounded in-process `kumo-dispatch::SerialQueue<1>` for readiness;
the actual completion records live in a fixed endpoint mailbox. Readiness coalesces,
records do not. Workers use `try_run_one` and never wait for another actor.

## Register before publishing

`TransferRing::preview` predicts TRB addresses without touching memory or the cycle
state. Before publishing any TRB, the driver reserves a receipt containing slot,
DCI, all owned TRB addresses and a nonzero, nonwrapping generation. A receipt marks
which TRBs request IOC events. An endpoint admits one transfer at a time; its up to
three TRBs each have reserved completion storage. There are 32 endpoint lanes per
controller owner, allocated without a heap. Admission and generation exhaustion
refuse before ring publication. Active or quarantined tokens cannot overlap a new
registration, even on another endpoint.

A single event pump advances ERDP for every consumed event and routes all Transfer
Events. Command waiters use the same pump, so they retain keyboard/storage events
encountered while awaiting a Command Completion. EP0, BOT bulk, one-shot hub status
and resident HID all register and consume receipts. No slot-only polling path remains.

## Delivery, generations and retirement

Routing matches slot, DCI and exact TRB. The event is stamped with that registration's
generation; deferred work and readers must present the same receipt. Events retain
arrival order, including both exact-control data and status. Each is delivered once;
records and seen bits remain until retirement, so a duplicate after consumption is
still detected. Completion status/residue remains the owning protocol's decision.
Legacy ordinary descriptor reads retain their status-only length semantics; NET0's
exact reads still validate both stages with `ControlPipe`.

Raw xHCI events carry pointers, not the software generation. This is a completion
fence, not a hardware cookie: reuse is permitted only after all requested IOC events
are consumed, no received event remains unread and the owner validates its terminal
status/residue. The current single-TRB normal TDs and separate control-stage TDs use
one event per requested TRB. Unknown outcomes are never retired or retried. Replacing
the router on a live controller would break this contract. Endpoint reset/recovery
requires NET4c's separate quiescence proof. — TIMBERDOODLE

The source rule is [Intel xHCI1.2b, section4.10.1](https://cdrdv2-public.intel.com/625472/625472_xHCI_Rev1_2b.pdf):
short/error events can occur before a TD's end; a short packet is not a completion
fence unless it names the last TRB. The current driver does not submit Event Data
TRBs. An ED-marked event or nonzero reserved pointer bits is rejected rather than
interpreting a cookie or rounded address as one of our pointers. — TIMBERDOODLE

## Failure and custody

An unknown owned token, duplicate or full mailbox quarantines the affected endpoint;
existing records remain intact and other lanes can progress. Unknown event formats
or invalid hardware slot/DCI require controller-wide quarantine. There is no eviction,
cancel-as-completion or unquarantine API. Failed lane metadata and DMA remain with
the live controller owner. An owner that would otherwise exit parks after its halt
request; requesting halt is not proof that memory can be freed.

The driver checks BOT's IN/OUT custody before writing CBW or data buffers, guards
legacy control zeroing inside the registered operation, and stops failed HID lanes
before another report-buffer write. HID only reclaims its own terminal event, handles
error/residual status and drains buffered actor work before parking, including keys
routed during a storage command.

One-shot hub status can remain pending when its startup budget ends. Its endpoint
and page are deliberately retained; a later completion is reported as quarantined.
This is separate from a successful EP0 routing proof, whose fault delta must be zero.
NET4c must establish actual endpoint/controller quiescence and reclaim these retained
resources. NET4c-1 adds parent-held allocation aliases as described below. Full
process-death recovery and DESIGN009 confinement remain unproved.
Recovery class is stateless; no critical durable truth is in the router. — TIMBERDOODLE

## Proofs and remaining work

Host tests exercise simultaneous NIC RX/TX/status plus unrelated HID/storage in
different orders, exact receipts and once-only delivery, stale deferred work after
address reuse, ring preview/wraps, budget/generation refusal, duplicate/full failure,
continued progress of another lane and malformed raw events. The NIC endpoint cases
are synthetic; physical RX/TX/status need later chip initialization and wire proofs.

The existing default-off EP0 proof records an additional result:

```text
NET4b EP0 routing registered=65 routed=130 consumed=130 retired=65 faults=0 PASS
```

It checks the real router's counter deltas around 65 reversible requests, not a
second mock router. Validate a capture with:

```sh
python3 scripts/ax88-ep0-proof.py --routing capture.log
```

The checker also requires all NET4a cycles, restoration and the accepted NET0 hashes.
Hardware keyboard/USB-stick/NVMe coexistence witnesses remain separate records.
NET4c owns independent resident NIC buffers, nonblocking RX scheduling, deadlines,
pressure and stop/disable/reset recovery with a flat repeated resource census.
This slice adds neither Ethernet endpoint setup nor traffic. — TIMBERDOODLE 2026-11-07


## NET4c-1: parent DMA custody before publication

All four contiguous-allocation paths in drv-xhci use `create_custodied_dma`:

| Backing | Contents / size | Creator and custodian | CPU mapping / device address |
|---|---|---|---|
| Initial controller page | DCBAA, command/event rings and ERST; 4 KiB | driver creates; Sora holds alias | uncached VA `0x12000000`; physical base plus offsets |
| Device pages | contexts, EP0/rings/control and HID/BOT/hub data; 4 KiB each | same | uncached VA `0x13000000`, `0x16000000`, or `0x14000000 + n*4096`; physical base plus offsets |
| Scratchpad pages | controller-advertised count, page rounded | same | never CPU mapped; physical entries in scratchpad array |
| Storage buffer | one 16 KiB window selected within 32 KiB backing | same | uncached VA `0x15000000 + window_offset`; physical base plus window offset |

The existing per-controller storage-control channel also carries private custody
frames. Before mapping/programming a new allocation, the driver duplicates its VMO
with READ|TRANSFER, transfers that alias and an exact 32-byte `XDM1` frame (sequence,
length, physical base, controller base), and waits for an exact eight-byte `XDA1`
with the same sequence. Sora imports and records the alias before sending ACK. The
parent log `XHCI DMA HOLD` precedes the driver's `XHCI DMA ACK`. No new public ABI,
client DMA handle or driver authority is introduced. Physical metadata is a cooperative
driver report, not a kernel-verified address binding. — TIMBERDOODLE

The pure `kumo-xhci::DmaCustodyLedger` has 32 slots per controller, at most 4 MiB per
allocation and 16 MiB total. It checks page alignment/nonoverflow, exact controller,
monotonic sequence, distinct handles and disjoint reported physical spans. Three
fixed ledgers cover the existing USB controller slots. Sora's single receive thread
releases each ledger borrow before logging or syscalls. Its nested client waits
also dispatch the control channels, avoiding a login/keyboard startup dependency.
Media offers retain their exact 25-byte shape and remain distinct from custody.

Rejected, malformed or missing ACKs never permit publication; the driver stops
allocating after failure. Sora may close only an unaccepted import. Every accepted
alias remains held on malformed messages, ACK delivery failure and peer closure;
there is no release/reset API. Thus a child exit alone cannot drop the final VMO
reference. The retained budget is bounded for one boot, and automatic controller
respawn is not enabled. Sora death or machine reset ends this custody; a Sora restart
is not proven safe while DMA remains active. — TIMBERDOODLE

This handshake protects allocation lifetime. It does not prevent a faulty driver
from programming another physical address, prove HCHalted, revoke DMA, stop an
endpoint, drain cancelled events, or permit reuse of uncertain buffers. Full
NET4c still needs verified quiescence, resident independent NIC buffers/nonblocking
RX and repeated flat start/stop census. The existing raw USB DMA non-claim and
Road C remain explicit. Do not expand the NVMe D5 exception to another PCIe device.

Validate a single completed boot capture with:

```sh
python3 scripts/xhci-dma-proof.py --self-test
python3 scripts/xhci-dma-proof.py capture.log
python3 scripts/ax88-ep0-proof.py --routing capture.log
```

The custody checker requires every observed HOLD/ACK to pair in causal order with
matching sizes and sequential slots, bounded disjoint spans and no refused custody.
It requires login after the final ACK, and reports the per-controller retained
census. This is trace evidence of startup, not a destructive child-death or DMA
confinement test. Physical keyboard, USB-stick and NVMe witnesses remain separate.
— TIMBERDOODLE 2026-11-07
