# Ethernet bring-up: ax88 first, one userspace network stack
<!-- j821 -->
<!-- j826 -->
<!-- j827 -->
<!-- j828 -->
<!-- j830 -->

Date: 2026-11-07. Author: TIMBERDOODLE.
Status: NET0 closed (J809-J811); NET1 accepted (J812); NET2a closed (J813-J814); NET2b closed in QEMU (J815-J816); NET2c closed in QEMU (TCP J817, supervised restart J820); NET3 pure codec landed J826; NET4a exact EP0 IN/OUT proved J827; NET4b completion ownership proved J828. NET4c-1 parent DMA custody closed J829; NET4c-2 verified endpoint quiescence closed J830. PLAN/021 AX7 first proof, AXH, AX0, AX1, AX2 and AX4 are closed. AX4 discharged the resident-capacity prerequisite for netstack-on-metal work. NET4c-3 is next: independent resident NIC buffers and a nonblocking RX lane.

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

NET4c-3 is the next slice: independent resident NIC buffers and a nonblocking RX
lane before chip initialization, cancelled only through NET4c-2's verified stop.
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
