# xHCI completion ownership
<!-- j828 -->
<!-- j829 -->
<!-- j830 -->
<!-- j831 -->

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
cancel-as-completion or unquarantine API. NET4c-2 adds one stop-fenced cancel, below;
it never treats a stopped event as a completion and never revives a faulted lane. Failed lane metadata and DMA remain with
the live controller owner. An owner that would otherwise exit parks after its halt
request; requesting halt is not proof that memory can be freed.

The driver checks BOT's IN/OUT custody before writing CBW or data buffers, guards
legacy control zeroing inside the registered operation, and stops failed HID lanes
before another report-buffer write. HID only reclaims its own terminal event, handles
error/residual status and drains buffered actor work before parking, including keys
routed during a storage command.

One-shot hub status can remain pending when its startup budget ends. Until NET4c-2
its endpoint and page were retained and a later completion was reported as
quarantined. NET4c-2 now stops and repoints that endpoint and retires the lane; only
an unproven stop keeps the old retention. A successful EP0 routing proof still needs
a zero fault delta. NET4c-1 adds parent-held allocation aliases as described below. Full
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


## NET4c-2: verified endpoint quiescence before reuse

A stopped device is not a stopped xHCI transfer, and a deadline is not a release.
`quiesce_endpoint` in drv-xhci is now the only path that ends a transfer without
its completion. Each step must succeed before the next one runs:

1. Arm the router's stop fence at the ring's `RingRestart`, the next producer TRB
   and its cycle. A live transfer uses `begin_stop`. An idle endpoint uses
   `register_stop`, a fresh generation with no owned TRB.
2. Issue Stop Endpoint. The single event pump routes every Transfer Event while the
   command waits. xHCI 1.2 section 4.6.9 orders the Stopped Transfer Event before
   the Command Completion Event, so the fence must already hold it.
3. `EndpointQuiesce` judges the completion against the Output Endpoint Context. On
   Success it needs the fenced event, EP State Stopped, and a TR Dequeue Pointer
   equal to the event's TRB, which must be owned or the restart. Context State
   Error while Running is the doorbell race Linux also retries, at most twice.
   Halted or Error needs Reset Endpoint, which this slice does not issue.
4. Issue Set TR Dequeue to the restart with DCS equal to the producer cycle. Read
   the context back. It must be Stopped at exactly that pointer and cycle.
5. Hand any completion that won the race to the owner. Then `cancel` the lane,
   which needs the stop event and every routed event consumed. Last, `abandon`
   the ring, which refuses if the producer moved.

The router accepts exactly one stopped-code event per fence. Codes 26, 27 and 28
may name an owned TRB that has no event yet. Code 27 may also name the restart,
or the Link TRB right after a wrap. Any event after the stop faults the lane. The
proof sorts the accepted events into four shapes from section 4.6.9 and table 4-2:

| Shape | Event | Buffer |
|---|---|---|
| interrupted | Stopped (26) at the TD, residue = full length | canary intact |
| partial | Stopped (26) at the TD with a smaller residue, or Stopped - Short Packet (28) with its EDTLA | bytes past the moved count intact |
| unstarted | Stopped - Length Invalid (27) at the TD | canary intact |
| raced | a completion, then 27 at the restart or Link TRB | owner receives the completion |

On the opi5 the USB2 hub showed the partial shape. One status byte landed as the
stop hit, and the controller reported Stopped with residue 3, not Stopped - Short
Packet. HCCPARAMS1.SPC was not logged, so the reason is not claimed. The
command wait and all failures retain the lane and its DMA, as NET4b did.

Ordinary images apply this to the hub status transfer that is still pending at the
end of its startup budget. The INFO record `hub status idle; ... lane retired`
replaces `lane/DMA retained`. A failed proof prints a NOTICE record and keeps the
old retention. — TIMBERDOODLE

### Proof mode and census

`KUMO_XHCI_QUIESCE_PROOF=1` is off by default. Once the ordinary cancel succeeds, it
runs 40 cycles on that hub's status endpoint. A cycle writes a canary, posts one
interrupt-IN TD and rings the doorbell. After 2 ms it cancels the TD through
`quiesce_endpoint` and checks the shape and the canary bytes the controller did
not report writing. It then rings the
doorbell again with no new TD and stops a second time. That probe must be a Force
Stopped Event at the restart. If the repoint were wrong, the controller would
resume the cancelled TRB. The probe fence owns no TRB, so the router would refuse
that event and the proof would fail.

Each cycle requires an empty transfer ring and command ring, an unchanged count of
router lanes and an unchanged custody sequence, so no allocation happened. The
final record compares router deltas: 40 registered, 80 stopped, 80 cancelled, no
completion retirements and no faults. Forty TDs on fifteen usable slots wrap the
ring at least twice, which exercises the Link alias and the DCS toggle.

```sh
python3 scripts/xhci-quiesce-proof.py --self-test
python3 scripts/xhci-quiesce-proof.py capture.log             # proof image
python3 scripts/xhci-quiesce-proof.py --ordinary capture.log  # loglevel-6 ordinary image
```

The proof delays hub-downstream enumeration by about 0.6 s, so device pages can be
allocated after the login prompt appears. J829's custody checker requires login
after the last ACK. On a proof capture that ordering rule fails while its pairing,
budget and span rules pass. The ordinary image keeps the J829 ordering.

The checker recomputes every cycle's TD, restart and DCS from the start record,
classifies each stop shape, and compares the final census. It rejects any
`NET4b completion fault`, retained-lane or FAIL record, and any TOWER, ABEND or
PANIC. It ignores replayed kernel-timestamped lines.

Not claimed: Reset Endpoint, Disable Slot or controller reset, and stream endpoints.
The hub page is not freed, because J829's parent custody has no release API. There
is no NIC endpoint, resident RX lane, DMA confinement or process-death recovery yet.
Those remain NET4c-3 and later. — TIMBERDOODLE 2026-11-07

NET4c-3 (J831) uses this stop for the ax88's resident lanes, then disables and
reclaims. Configure Endpoint with Deconfigure (`Trb::deconfigure_endpoints`) is
issued only after every pending lane was cancelled here. The Output Slot and Endpoint
Contexts must then read back Addressed and Disabled (`SlotSnapshot`) before a ring
is rewritten. `drv_xhci::nic::NicLifecycle` enforces that order. Details and metal
numbers are in [Ethernet bring-up](ethernet-bringup.md). — TIMBERDOODLE 2026-11-07


NET4c-3 checks a stop-racing completion's code, exact token and residual before
reading DMA, using the same checked-length helper as normal completion. Repoint
proves the controller stopped; it does not validate payload metadata. The backing
remains held on either kind of failure. Its repeated census covers owned lanes and
DMA allocations; no whole-kernel resource or physical wire-pressure claim follows.
— TIMBERDOODLE 2026-11-07
