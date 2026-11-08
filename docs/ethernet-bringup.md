# Ethernet bring-up: ax88 first, one userspace network stack
<!-- j828 -->
<!-- j830 -->
<!-- j831 -->
<!-- j832 -->
<!-- j833 -->

Date: 2026-11-07. Author: TIMBERDOODLE.
Status: NET0 closed (J809-J811); NET1 accepted (J812); NET2a closed (J813-J814); NET2b closed in QEMU (J815-J816); NET2c closed in QEMU (TCP J817, supervised restart J820); NET3 pure codec landed J826; NET4a exact EP0 IN/OUT proved J827; NET4b completion ownership proved J828. NET4c-1 parent DMA custody closed J829; NET4c-2 verified endpoint quiescence closed J830; NET4c-3 resident ax88 lanes closed J831 on separate keyboard+ax88 and USB-stick+ax88 cold boots. PLAN/021 AX7 first proof, AXH, AX0, AX1, AX2 and AX4 are closed. AX4 discharged the resident-capacity prerequisite for netstack-on-metal work. NET5a read-only identity closed J832; NET5b volatile initialization and cable up/down closed J833. NET6 passive RX is next.

## First controller

Use **ax88** for the AX88179/AX88178A family, as selected by CRUX.
Start with the available ASIX AX88179 USB adapter. KUMO already has xHCI enumeration,
control IN and bulk-transfer mechanisms. Moving this adapter between hosts will let
us exercise a common chip driver and network interface on different platforms.
Each host's USB route still needs its own proof.

The onboard RTL8125BG remains the next backend. Its chip driver can also be portable,
but Orange Pi bring-up needs additional PCIe controller/config/BAR, PHY, DMA-ring and
interrupt evidence. The M.2 NVMe route does not prove either onboard Ethernet route.
The first goal is correct packets and usable sockets; 2.5GbE throughput comes later.

Use one userspace `netstack` with loopback first, as PLAN §9 and DESIGN/008 specify.
Evaluate and pin [smoltcp](https://github.com/smoltcp-rs/smoltcp) for its protocol engine;
check the exact release/features with KUMO's pinned toolchains before adoption.
Its [frame device interface](https://docs.rs/smoltcp/0.14.0/smoltcp/phy/trait.Device.html)
provides the adapter boundary. No dependency is added by this planning slice.
— TIMBERDOODLE

## Boundaries

Applications hold explicitly granted socket-service channels. The network server holds
an Ethernet-frame channel; it gets no xHCI MMIO, IRQ or physical DMA addresses. Initially,
the AX protocol module runs inside the existing `drv-xhci` owner with pure codec logic
in a separate library. One process controls the controller. A USB broker or separate
chip process can follow once interface capabilities exist.

Copied frames fit the existing IPC model. Begin with MTU 1500, one interface, software
checksums, bounded RX/TX queues and per-client socket quotas. NET1 fixes versioned
messages, exact size/handle limits, readiness, backpressure and restart errors before
implementation. Socket state is soft state: server restart fails old sockets; fresh
connections are explicit. Never silently replay application data.

The USB adapter is not a direct physical-memory DMA owner; xHCI is. Its transfer buffers
still require bounds, correct cache/order rules and retained custody until quiescence.
Timeout or process death alone does not allow reuse. Existing raw-DMA operation does
not establish DESIGN/009 confinement. NVMe's D5 approval does not cover a new PCIe NIC;
Road C remains open as described in [the NVMe debt record](orange-pi-5-plus-nvme.md).

Keep loopback/socket work independent of physical link bring-up. The existing minimal
Rust PAL I-b gate uses fs/io/collections/time. External Ethernet extends `std::net`;
it does not change that gate or replace target/sysroot, TLS/thread/runtime work.

## Prerequisites found in the current USB code

The shared controller currently dispatches HID, hubs and storage. Its SuperSpeed side
path leaves other classes alone; there is no Ethernet service yet. Storage's synchronous
bulk poller cannot become a permanent network RX loop. Keyboard, storage and control
service must remain responsive while RX waits for a packet.

Before Ethernet RX, add bounded EP0 data OUT, endpoint-aware completion routing and a
resident NIC lane. Current completion dispatch/stashing matches USB slot only and may
drop the oldest stashed event under pressure. RX, TX and interrupt-status transfers on
one device need slot/DCI/TRB/generation ownership and explicit overflow handling. Prove
quiescent cancellation and buffer custody before automatic owner replacement.

The supplied AX note also needs two corrections from its bundled driver reference:
RX metadata is reached through a trailing aggregate header; TX has an 8-byte prefix.
Pure framing tests must cover padded bounds, dummy entries, alignment/status and exact
USB max-packet boundaries. Use observed descriptors and silicon revision, not product
strings, to select a protocol. The [upstream Linux driver](https://github.com/torvalds/linux/blob/master/drivers/net/usb/ax88179_178a.c)
is a cross-check; no C implementation is imported here.

## Acceptance ladder

Each row is a future gate, split further whenever necessary. Local PLAN/020 contains
the exact dependencies, mutation boundaries and negative proofs; RESEARCH/019 indexes
all supplied `resources/net` files and records the conflicting note. J808 plans work;
it does not claim any of these gates passed.

| Gate | Required result |
|---|---|
| **NET0** | Exact adapter/port/interface/endpoints/revision inventory, raw descriptors matched to an independent host; unknown devices stay unbound |
| **NET1** | Capability, frame/socket wire, resource limits, readiness/restart design and exact stack dependency choice |
| **NET2a–c** | Pinned no_std protocol engine, then native UDP and TCP loopback through real IPC, including no-grant/error/restart tests |
| **NET3** | Pure AX descriptors, vendor requests and aggregate RX/TX framing pass bounds/short/error fixtures |
| **NET4a–c** | Correct control IN/OUT, endpoint completion ownership, resident scheduling and DMA custody preserve HID/storage |
| **NET5a–b** | Read-only identity/MAC/status first; then minimal volatile initialization and measured link up/down |
| **NET6** | Passive RX matches independently captured peer frames, with malformed aggregates and queue pressure bounded |
| **NET7** | TX matches peer packet capture, including max-packet boundaries; frame-channel isolation/backpressure pass |
| **NET8** | Static IPv4 ARP and bidirectional ICMP on an isolated test subnet |
| **NET9** | Native UDP exact datagrams in both directions with size/error/timeout behavior |
| **NET10** | Native TCP exact stream hashes, partial I/O, backpressure, orderly close/reset and induced-loss handling |
| **NET11** | Separate DHCP grant/renew/expiry/error proof, isolated from the existing PXE DHCP path |
| **NET12** | Bounded DNS resolution and invalid-reply/compression/timeout tests against a controlled server |
| **NET13a–b** | Maintained relibc overlay gains socket FDs, UDP/TCP and the readiness/error subset required by the actual PAL |
| **NET14** | Pinned build-std programs use UdpSocket and TcpListener/TcpStream over loopback; server failure wakes clients |
| **NET15** | Same std programs over AX Ethernet; safe reconnect/restart, HID/USB/NVMe coexistence and a second-host portability review |

NET4c landed as three signed slices (J829 parent custody, J830 verified stop, J831
resident lanes); see the NET4c-3 section below. Both physical arrangements are proven separately; RX pressure under real frames is
NET6's gate. NET5a is next: read-only identity.
[Completion ownership](xhci-completions.md) records NET4b's endpoint actors and
receipts, NET4c-1's parent custody and NET4c-2's quiescence proof. [Exact EP0 data transfers](xhci-ep0.md) records NET4a's
transport and reversible metal proof. NET3 supplies the pure codec described in
[ax88 protocol](ax88-protocol.md). Loopback work can advance
independently of the adapter.
NET13/14 can use loopback before external DHCP/DNS; numeric-IP tests stay independent
of name resolution. A second-host proof remains pending until actually exercised.

## Additional chips and final evidence

For RTL8125BG, begin a separate guarded inventory for each onboard NIC and physical jack.
Establish revision-specific initialization, actual firmware requirements, minimum grants
and a DMA trust decision before rings are enabled. Prove polling RX/TX and quiescent
restart, then wired INTx if available. Chip MSI/MSI-X capability does not provide a KUMO
ITS implementation. If wired delivery is unavailable, track that as a separate gate.
Reuse the accepted frame/socket service and its tests; do not fork the network stack.

The supplied RTL8153/8153B PDFs and Linux `r8153_ecm.c` support a later USB backend.
Select vendor, ECM or NCM by actual descriptors/revision; those are different protocols.
Empty ECM/NCM marker files provide hints, not implementation evidence. ASIX variants
also require their own match; no blanket CDC compatibility is assumed.

Every hardware gate records peer captures/hashes, topology, packet/drop/error counts,
bounded idle/timeout behavior and continued keyboard/storage service. Host fixtures and
QEMU loopback prove logic; physical packet capture proves the wire. Keep original boot
files intact, restore default images and stop task PXE/capture processes before signing.
IPv6, complete POSIX sockets, TLS, zero-copy, multiqueue and throughput tuning follow as
separate slices. Link LEDs, ping and PXE success are individual observations, not a
complete socket/PAL acceptance. — TIMBERDOODLE 2026-11-06


## NET0a descriptor inventory

The drv-xhci ax88 module recognizes the two explicit ASIX IDs and checks the complete
configuration/interface/endpoint/SS-companion structure. It logs raw descriptors and
root/route/slot/speed observations before any class configuration. A private standard
GET_DESCRIPTOR IN path requires successful data AND status TRB completions with zero
residual; short/error/unknown completions do not prove a descriptor read. The adapter
stays unconfigured, with no vendor command, bulk endpoint setup or Ethernet TX.

An owner that observed ax88 retains its process and DMA until reset, including failed
reads. Nonresident controllers request halt and park; that request is not a buffer-release
proof. A shared resident HID owner keeps its normal service and the observed slot.
Independent host/firmware descriptor comparison is the separate NET0b gate. These are
implementation boundaries; the journal records the actual physical result.
— TIMBERDOODLE 2026-11-06


NET0a metal observation (2026-11-06): ax88 matched 0b95:1790, bcdDevice0100,
bcdUSB0210, USB2 high speed on controller0xfc400000/root1/hub route4/slot5.
Exact device18B/config39B reads passed. Status EP81 has MPS8/interval11; bulk IN82
and OUT03 each MPS512, no SuperSpeed companion on this connection. Login and both
NVMe test-file reads passed, then NVMe was unmounted. No USB storage was offered;
USB-storage coexistence and independent firmware comparison remain future proofs.
— TIMBERDOODLE 2026-11-06


## NET0b independent firmware reference

`KUMO_AX88_ORACLE=1 cargo xtask image --arch aarch64 --hardware orange-pi-5-plus`
adds an explicit, default-off pre-EBS reference. It reads firmware's USB_IO cached
device/configuration/interface/endpoint descriptors, independently of drv-xhci.
Only the observed simple USB2 layout (9+9+3x7 =39 configuration bytes) can be fully
reconstructed; longer/SS/other layouts are unavailable rather than guessed. Exactly
one matched interface and complete reference are required for an unambiguous comparison.
The helper calls no USB transfer/reset method, configures no device and allocates no DMA.
Default images omit the helper. Host comparison with KUMO's raw bytes supplies the actual
acceptance; enabling the switch alone does not prove a match. — TIMBERDOODLE 2026-11-06


NET0b metal acceptance (2026-11-06): firmware and KUMO device18/config39 bytes match
exactly on two normal-reboot boots, also matching J809's capture. Each firmware pass
found exactly one matching complete interface. KUMO topology/endpoints remain unchanged;
login, legacy FAT and both NVMe file reads pass after each boot. The first boot reproduced
the separate contained GPU MMU70000 fault; the identical image's second boot completed
J0c graphics with no TOWER. DEFERRED014 stays open. USB storage was not offered and a
physical keyboard witness remains pending, so those coexistence checks are unclaimed.
No ax88 link/MAC/packet/socket functionality is accepted yet. — TIMBERDOODLE 2026-11-06


NET0c coexistence (J811): two USB-A ports cannot hold a keyboard, a USB stick and the
adapter together, so each witness used its own cold boot with the ax88 attached. The
keyboard worked at the shell; NVMe test files read on every boot. The stick exposed a
drv-xhci gap: a probed volume was offered only from the HID service loop, so without a
keyboard it was never served. drv-xhci now runs that loop with zero HID devices when it
keeps a volume; the stick then mounted at /media/KUMO and a file read back. The ax88
descriptors matched the NET0a capture on every boot. NET0 is closed. — TIMBERDOODLE


## NET1 contracts and protocol engine

The network server and its frame provider talk over one versioned frame channel: a fixed
32-byte header, one Ethernet frame per message, an interface generation that changes on
every provider reset, per-direction sequence numbers, and explicit RX/TX credits. A
completed USB transmit is reported as handed to the bus, never as delivered. Clients use a
separate session channel granted per program: fixed 32-byte request/reply headers with
request ids, session-local socket ids, a cap of eight unanswered requests, and a WAIT
operation for readiness, so no call blocks the server loop.

KUMO channel messages hold up to 4096 bytes and 64 handles, but a channel's queue is not
bounded by the kernel. Every queue limit is therefore enforced by these protocols, and a
peer that exceeds one is disconnected. Sessions, sockets (16 per client, 32 total), buffers
and per-turn work are fixed at start; a restarted server fails old sockets and replays
nothing.

The protocol engine is pinned to smoltcp `=0.14.0` (0BSD, Rust 1.91 minimum; KUMO's stable
toolchain is newer) with default features off and only Ethernet/IP media, IPv4, UDP, TCP and
ICMP sockets enabled. Its eight-crate dependency closure and checksums are recorded, and a
probe built it without an allocator for both bare-metal targets and the host. smoltcp's own
loopback device needs an allocator, so the first implementation slice provides a bounded
loopback device instead. Dependencies are vendored only when that slice lands.
— TIMBERDOODLE 2026-11-06



## NET2b-1 first network service

`net <program> [args]` launches a program exactly like `run`, plus one session with the
network service; `run` grants none, and a program cannot ask for one. The service starts on
the first `net` launch, owns only a loopback interface (127.0.0.1/8), and serves UDP over the
session protocol. In QEMU a probe moved exact datagrams of 0, 28, 1472 and 14 bytes between
two of its sockets; oversize datagrams, unknown sockets, remote addresses, TCP (not yet
served) and the per-client socket limit are refused with specific errors, and a malformed
request ends the session. Ending a session frees its sockets. Two separately launched
clients exchanging datagrams at the same time is the next step. — TIMBERDOODLE 2026-11-06


NET2b-2 (J816): the shell gained `bg run|net <program> [args]`, which starts one program
without waiting for it, and `wait`, which collects its output when it finishes. A receiving
probe started with `bg net` and a sending probe started with `net` ran at the same time as two
separate programs with two network sessions: datagrams of 13, 0, 1472 and 512 bytes went out
and came back exact, with matching checksums printed by both sides. Both sessions were freed
when the programs ended. TCP over loopback is next. — TIMBERDOODLE 2026-11-06


## NET2c TCP over loopback

The network service now serves TCP on 127.0.0.1 alongside UDP. A server probe started in the
background accepted a connection from a separately launched client; the client streamed 12000
bytes, some of them only partly accepted at a time, and received every byte back in order,
with the same checksum printed by both programs. The client then closed its sending side; the
server saw end-of-stream, closed, and the client saw end-of-stream too. Connection refusal,
operations that would block, wait deadlines, a full send buffer, and a peer that resets are
all reported with specific errors. J820 adds the supervised restart proof below. — TIMBERDOODLE 2026-11-06

## Supervised netstack restart (NET2c-2 / AX7, J820)

Sora retains netstack's process and control capabilities. A process-death event invalidates
all old session channels and socket state. A timer starts a new instance after 50/100/200 ms,
with at most three restart attempts in any ten seconds; failed replacement spawns count too.
Exhaustion gives up visibly until reboot. A new launch cannot reset that limit.

Each instance receives a nonzero epoch in a three-byte control bootstrap (`I`, little-endian
u16); it owns a disjoint 65535-session generation range. Neither epoch nor session generation
wraps. Epochs cross the control channel because the historical ProcessRun ABI may interpret
an integer matching a live handle as a capability. Only new explicit `net` launches get new
session channels. Old sockets never reattach, and application sends are never replayed.

QEMU proved a live TCP connection's pending WAIT ends with PeerClosed, a fresh launch sees a
new generation and empty socket state, its old TCP bind is reusable, and UDP/TCP still exchange
exact bytes. A fourth crash inside the intensity window gives up; later launches are refused.
The proof also exercises the idle supervisor at login. Launch arguments now use a separate
read-only VMO snapshot per child: a background launch may read after another launch starts.

Fault injection is default-off, QEMU-only: build the image with KUMO_NET_RESTART_PROOF=1 and
KUMO_CONSOLE_LOGLEVEL=6; `netstack-crash-proof`, `... burst`, and `... idle` are private Sora
proof commands, absent from ordinary images. No client receives kill or supervision authority.
The repeatable tracked harness is the next, separate AXH slice. Sora's general child-output
blocking remains PLAN/021 AX9 / DEFERRED/015; this slice dispatches netstack death/timer events
in both existing wait contexts. No netstack-on-metal proof precedes AX0's resident census.
— TIMBERDOODLE 2026-11-06

## Repeatable NET2 regression (AXH, J821)

Run `python3 scripts/netstack-smoke.py`. It builds the loglevel-6 QEMU proof image, boots
five private ESP copies under OVMF, and restores the ordinary QEMU image even on failure.
Each autoexec is at most 256 bytes. No board, login credentials or Ethernet traffic is used.
Use `--output <new-directory>` to choose the evidence directory, `--firmware <directory>`
for another QEMU EDK2 installation, or `--self-test` to check transcript rejection rules.

The cases check explicit authority refusal; UDP size/quota/malformed/deadline errors; TCP
refusal/buffering/EOF; separate TCP and UDP peers; pending TCP WAIT across server death and
fresh generation/state; restart-intensity give-up; and replacement while idle at login.
Only the no-grant case permits one contained exit-2 TOWER. All unexpected faults, probe
failures and missing/duplicate expectations fail. The five known blocking-autoexec POST
failures (DEFERRED/015) are reported separately; unknown or partial failure sets fail, and
the idle case requires green POST. A green result also requires default-image restoration.
The manifest records HEAD, tool/firmware identity, source/private/default image and ELF
hashes, per-case runtimes and POST results; serial and build logs remain beside it.

### NET0 manual metal regression witnesses

Record commit, image SHA-256, serial capture and witness time. Keep ax88 attached for both
cold boots (the board has two USB-A ports; no three-device or hot-plug claim):

1. Keyboard + ax88: match VID/PID `0b95:1790`, revision `0100`, current route/speed and
   endpoint inventory. Compare raw descriptors to the accepted capture: device18 SHA-256
   `8f3a364dd0edc2cfc2e00e0e2a0ee8f2c81d177876c822e1cac280ca2085e99d`, config39 SHA-256
   `32a0f0cb1f97ae5ada11ed06c9064ea159bb75ec57479b6ff461d46f4d6b7de1` (J809/J811).
   Log in and physically type `echo ax88-keyboard`; require the exact echoed text.
2. Mount the currently enumerated NVMe FAT partition at `/media/nvme` (J811 used
   `/dev/disk3s1`), read `HELLO.TXT` and `NV9J807.TXT`, and compare their accepted contents.
   Unmount it. Do not infer a pass from an initrd file with the same name.
3. Power off before swapping keyboard for the FAT USB stick; cold-boot stick + ax88.
   Match descriptors again. Require storage offering with no HID lanes, `/media/KUMO`,
   and `cat /media/KUMO/J549.TXT` yielding `KUMO j549 USB persistence proof`.
   Repeat the two NVMe reads; unmount both volumes cleanly before shutdown.
4. Require no unexpected TOWER/ABEND, no lost keyboard/storage service, and successful
   shutdown/flush. A missed USB2 root-port enumeration is a failed boot witness, not a
   coexistence pass. Capture and repeat it explicitly (J811 recorded this timing miss).

This checklist records the manual set every later AX kernel/NET metal slice cites. It does
not itself rerun the witnesses or prove packets, PHY behavior, SuperSpeed or another host.
Netstack-on-metal testing still waits for AX0's resident census and capacity gate.
— TIMBERDOODLE 2026-11-06


## NET4c-3 resident ax88 lanes (J831)

`KUMO_AX88_RESIDENT_PROOF=1` is off by default. Without it the adapter stays
unconfigured, as since NET0. With it, enumeration hands the adapter to the one
resident loop that already serves HID and storage, instead of dropping it.

- **Buffers.** One parent-custodied 64 KiB backing at VA `0x17000000` holds a 32 KiB RX
  window chosen by `boundary_window` so it crosses no 64 KiB boundary. The other
  half holds both 16-TRB rings and the 8-byte status buffer. Nothing is shared with
  EP0, HID, storage or the device page. TX gets its buffer in NET7.
- **Configuration.** SET_CONFIGURATION(1), then one Configure Endpoint for the status
  interrupt-IN (DCI 3) and bulk-IN RX (DCI 5) endpoints, read back Configured/Running
  at the fresh rings. CLEAR_FEATURE(ENDPOINT_HALT) then resets both device-side
  toggles. No vendor command; the chip is not initialized (NET5b).
- **Lanes.** The loop arms one 32 KiB RX transfer and one 8-byte status transfer and
  never waits on either. Routed completions are consumed when they arrive: status
  bytes go through `kumo_ax88::status`, RX aggregates through the NET3 parser. With
  the chip uninitialized, RX stayed pending and no status report arrived on metal.
- **Lifecycle.** `drv_xhci::nic::NicLifecycle` fixes the order: verified stop of every
  pending lane (NET4c-2), Configure Endpoint with Deconfigure, a readback of
  Addressed/Disabled/Disabled, and only then ring reinitialization and a fresh
  configure. Any refused or failed step is sticky and retains everything. xHCI 4.6.6
  drops a Running endpoint with no stopped event, so stop must come first.

The proof binds a one-shot 100 ms timer to the loop's existing port. It runs:

1. thirty idle ticks, each with one exact EP0 device-descriptor read;
2. twenty stop/deconfigure/reclaim/reconfigure cycles, one per tick;
3. thirty settled idle ticks.

Proof images set the boot keyboard's HID idle to 100 ms, which boot keyboards must
support. HID liveness is then counted rather than inferred, and the decoder ignores
repeated reports. Each window records:

- the loop's longest busy stretch after a wake;
- the slowest EP0 read;
- the latest tick;
- the longest gap between HID reports.

These are wall-clock numbers. There is no per-thread CPU clock, so time the
scheduler gave other processes is included. Every cycle record splits its time into
stop, deconfigure, configure-with-readback and clear-halt steps.

Metal, keyboard + ax88 behind the fc400000 hub pair, J831's final proof boot:

| Window | Wall ms | Slowest EP0 read | Busy max | HID gap max | Tick late max |
|---|---|---|---|---|---|
| idle, during boot | 6392 | 290 ms | 292 ms | 1448 ms | 1353 ms |
| 20 cycles | 3547 | n/a | 60.0 ms | 157 ms | 50 ms |
| settled | 3692 | 6.3 ms | 10.3 ms | 140 ms | 67 ms |

| Cycle step, max over 20 | Time |
|---|---|
| two verified stops | 5.7 ms; usually about 0.1 ms |
| Configure Endpoint with Deconfigure | 22.9 ms; 0.7–23 ms |
| Configure Endpoint + readback | 32.0 ms, every cycle |
| two CLEAR_FEATURE requests | 0.68 ms |

The census was exact:

- configures 21, cancels 40, stops, disables and reclaims 20 each;
- router stopped and cancelled 40 each, zero faults;
- lanes 9/9 and next custody sequence 8/8 (seven held VMOs on this controller);
- both lanes still pending at the end.

All 60 EP0 reads matched the NET0 descriptor, and 118 HID reports arrived. CRUX confirmed a physical USB keyboard echo after
the proof completed.

The boot-window stall coincides with the display driver's HDMI PHY re-lock and
scanout rebuild. The console went silent for 2.6 s there. The same EP0 read takes at
most 6.3 ms once settled, but without CPU accounting this is not attributed beyond
that coincidence. A full stop/disable/restart costs the loop about 33–56 ms, nearly
all controller command latency. The command waiter blocks the owner for that long,
and HID reports waited up to 157 ms then. Idle RX/status are not polled; HID and timer wakes continue to service the owner.
No per-thread CPU-use measurement is available.

Not claimed:

- chip initialization, link, RX or TX traffic, or RX pressure under real frames
  (NET5b to NET7);
- Reset Endpoint for halted lanes, or controller reset (HID shares the controller);
- a nonblocking command waiter;
- real RX pressure, a whole-kernel resource census or DMA confinement.

The same guarded image also passed a separate USB-stick+ax88 cold boot. During
its initial idle window, serial login read `J549.TXT` exactly (six blocks, zero
writes) and unmounted `/media/KUMO`; both NIC lanes stayed pending throughout.
Both NVMe persistence files were read and unmounted on both arrangements. The
stick run passed 60 EP0 reads and 20 cycles, with 40 cancelled/stopped transfers,
zero router faults, lanes 10/10 and next custody sequence 9/9 (eight held VMOs).
Its slowest cycle was 64.8 ms; settled EP0 max 4.1 ms and busy max 8.0 ms. The
larger 1.17 s boot busy maximum includes scheduling and concurrent display work;
it is not a bounded latency promise. No link/status/RX completion was observed.

Normal and stop-racing completions share a pure checked length validator. Invalid
completion codes or residuals above the requested span expose no DMA bytes; a
failed cancellation step stays failed with backing held. The flat census measures
owned rings, receipts and DMA allocations, not global OS heap/frame stability.

Validate with:

```sh
python3 scripts/ax88-resident-proof.py --self-test
python3 scripts/ax88-resident-proof.py capture.log
```

— TIMBERDOODLE 2026-11-07


## NET5a read-only identity (J832)

The default-off `KUMO_AX88_IDENTITY_PROOF=1` path reads only the ten named MAC/PHY
fields in [the identity proof](ax88-identity.md). `KUMO_AX88_ORACLE=1` also enables
an independent firmware USB_IO reference before handoff. KUMO requires exact data
and status completions; firmware reports status but has no actual-length output.
Neither path writes a PHY page, powers or initializes the chip, reads EEPROM/eFuse,
sets up Ethernet endpoints or transmits packets. Identity mode takes precedence
over the older mutating EP0/resident proof flags. Ordinary images omit the probe.

The observed node address is locally administered, not an attestation of a factory
MAC. Zero PHY IDs are unavailable status, not a cable-down result. NET5b must
establish the volatile PHY/clock/queue state before physical RX/TX proofs.
— TIMBERDOODLE 2026-11-07


## NET5b volatile initialization and link control (J833)

`KUMO_AX88_LINK_PROOF=1` runs `kumo-ax88::link` on the resident owner's EP0. It
releases PHY reset, waits 500 ms, enables clocks and waits 200 ms, then reads a
page-zero PHY identity. It turns checksum offload off and leaves MAC receive off,
and starts unicast+broadcast RX mode with the two-byte IP header alignment. On every
link up it programs the reference queue and speed/duplex medium. Each write is read
back. Each link change is cross-checked against the interrupt endpoint and an
independent autonegotiation resolution. At the end it parks the chip as `unbind()`
does. It writes nothing to the PHY and nothing to EEPROM/eFuse, transmits nothing,
and receives no frames. [The link proof](ax88-link.md) records every value and wait,
and what was left out.

On metal (keyboard + ax88, LAN switch peer), all 16 writes read back exactly. The
PHY (ID 001cc915) linked 3.0 s after release at 1000/full, the resolved mode. CRUX
completed two physical unplug/replug cycles; all five transitions were confirmed by
the status endpoint within 197 ms. The census stayed flat with 0 faults and 0 RX
completions, and a physical keyboard echo passed. The first boot exposed an EP0
deadline judged before draining completions; J833 fixes it
([Exact EP0 data transfers](xhci-ep0.md)). The display's D1d holder-death check
failed during both corrected proof boots and remains an open observation. The
switch's own port reading, frames, pause/EEE and a second host are unclaimed.
— TIMBERDOODLE 2026-11-07
