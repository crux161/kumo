# Orange Pi 5 Plus NVMe: bring-up and DMA debt
<!-- j800 -->
<!-- j801 -->
<!-- j802 -->
<!-- j803 -->
<!-- j804 -->

Date: 2026-11-05. Author: TIMBERDOODLE.

## Current state

NV0–NV6 are complete; NV7a adds read-only FAT volume discovery and service. Firmware's PCIe3_4L link and enumeration lead to a
Samsung NVMe 1.3 function at 01:00.0. The userspace driver proved admin queue
configuration and graceful restart with two distinct owners, each reaching
RDY=1/CFS=0 then RDY=0/EN=0 before buffer release (J798). The guarded SMMU
observation is disabled nonsecure bypass; containment remains unimplemented.

`lib/kumo-pcie`, its HAL observers and the early boot report discover the inherited
route. `drv-nvme` owns controller operations behind Sora's narrowed Resource and
DMA custody protocol. NV4 added polled Identify, NV5 polled reads and NV6
INTx completions; the block service and filesystems follow.
The detailed local plan is PLAN/019; each journal records its own acceptance.

## D5: cooperative DMA for NV3–NV9

CRUX selected this path on 2026-11-05:

- Use raw physical addresses from contiguous DMA VMOs for NV3–NV9. Prove queue
  setup, Identify, block reads, GPT, filesystems, then writes and persistence in
  successive slices, with each slice's own acceptance evidence.
- The NVMe driver and device are trusted for this bring-up path. Physical
  contiguity allocates usable buffers; it does not restrict the device to them.
  There is no claim of hostile-driver isolation or DESIGN/009 DMA confinement.
- Preserve the allocation/cache/order/lifetime rules: stop inherited firmware
  queues before replacing their addresses, retain buffers while the device may
  access them, and prove quiescence before reuse. A timeout is not that proof.
  Retain/quarantine memory when safe release cannot be established.
- Recheck the current link, controller and SMMU preconditions before taking
  ownership. If bypass is no longer observed, refuse this raw-physical path;
  do not silently disable a translating SMMU or adopt unknown stream tables.
- Restate the non-claim in each NV3–NV9 journal and keep it visible in the driver
  documentation. Storage correctness and successful reboot tests do not close
  the DMA isolation debt.

This decision selects the DMA trust model. Controller/resource/interrupt choices
and filesystem write acceptance still follow their separate milestone gates.
— TIMBERDOODLE 2026-11-05

## Open debt: PCIe SMMUv3 enforcement (Road C)

**Status: OPEN.** Track this independently of NV3–NV9 completion. The governing
architecture is DESIGN/009: a device reaches only VMOs mapped in its DeviceCtx.
The existing SMMUv3 hooks are scaffolding, not an enforcing PCIe backend.
— TIMBERDOODLE 2026-11-05

After storage and upper file services stabilize, implement the RK3588 PCIe
MMU600 backend and migrate NVMe from raw physical addresses to DeviceCtx IOVAs.
Review the debt at NV9 acceptance so that "stable" has an explicit checkpoint;
NV9 filesystem success alone leaves this item open.

Close the debt only with evidence that:

1. The endpoint's requester/stream identity is bound to its own enforcing
   DeviceCtx, with DMA rights and ranges derived from granted VMOs.
2. Valid NVMe reads/writes still work through those mappings.
3. Deliberate out-of-range, unmapped, read-only-target and cross-context DMA
   attempts fault without modifying guarded memory or panicking the kernel.
4. Faults identify the responsible context and reach its owner; reset/restart,
   invalidation and revocation prevent stale DMA before memory is reused.
5. Bypass and unimplemented backend hooks cannot report successful containment.

Source of the sequencing decision: CRUX's 2026-11-05 NV* continuation instruction.
The local debt ledger is DEFERRED/011; J796 records ratification and rollback.

## NV3a inherit-only admission

The HAL's status-only unit is named by the observed BAR0 prefix at `0xf0000000`,
length 4096. It has no power-up or shared-control write sequence. Every status/mint
observation repeats NV0's guarded root reads, the stable firmware CFG0/identity-MEM
route checks, and the guarded SMMU disabled-bypass observation. Endpoint config
and CAP/VS still need their own probe before treating the page as NVMe.

For the enabled Orange Pi root, only these ranges are admitted by the mint guard:

| Range | Raw access |
| --- | --- |
| BAR0 first page, `0xf0000000` + 4 KiB | READ/WRITE if the Resource permits it |
| Client, `0xfe150000` + 4 KiB | READ only |
| DBI header, alias `0xf5000000` + 4 KiB | READ only |
| Unrolled iATU, `0xf5300000` + 8 KiB | READ only |
| Current function page selected from firmware CFG0 | READ only |

Unknown, oversized or alternate root/outbound windows are refused. Raw writes to
root/config windows remain reserved independently of power/DT discovery, matching
the existing display reservation model. This slice does not grant interrupts or
doorbell access, allocate queues, write a device register or start DMA.

The existing 64-byte `DevicePowerStatus` record keeps version 1. `POWERED` and
`APERTURE_MINTABLE` describe the observed path; `REPAIR_DONE` and
`CLOCK_AT_BOOT_RATE` stay clear because this unit neither observes BISR completion
nor selects/verifies a boot frequency. Its diagnostic layout is:

- `pmu[0..6]`: root power status, SMMU IDR0, CR0, CR0ACK, GBPA, MMU Q-channel status.
  An unread/invalid policy has IDR0 zero and the remaining policy words all-ones;
  it never means bypass and never sets `APERTURE_MINTABLE`.
- `cru[0..8]`: GATE32/33/34/39, SOFTRST32/33/34, PHPTOP GATE0. The complete guarded
  NV0 report also checks PHPTOP reset; the fixed record is selected diagnostics.

Admission and disabled bypass remain a cooperative non-claim, not enforcement.
— TIMBERDOODLE 2026-11-05

## NV3b userspace admin queue proof

`drv-nvme` has a pure, host-tested register sequencer and a userspace runtime.
Sora discovers the enabled root from its read-only live DTB capability, observes
root/config/CAP state through read-only mappings, and gives the child only the
first BAR page as a Resource with no IRQ grant. The driver repeats power/mint
admission and verifies CAP/VS. This first path supports the observed legacy
NVMe 1.x NVM/4 KiB case, with 32-entry admin queues.

One 8 KiB contiguous DMA VMO supplies separate 4 KiB SQ/CQ pages below 4 GiB.
The kernel zeroes and cleans its previous WB alias; the child maps Normal-NC and
uses volatile CPU accesses. Before any controller write, it transfers a read-only
custody alias to Sora and waits for acknowledgment. No shared-control, root/config
write, SMMU mapping, command or doorbell is granted or issued by this proof.

The sequencer settles any inherited EN/RDY transition, clears EN only from a
ready enabled state, waits for RDY=0, masks interrupts for polling with MSI-X
disabled, programs AQA/ASQ/ACQ, verifies readback, enables with the legacy NVM
entry/page sizes and waits for RDY=1/CFS=0. It then disables and reads back both
RDY=0 and EN=0 before reporting quiescence. Polling has a CAP.TO-derived deadline,
an extra 500 ms margin and bounded backoff; a timeout never authorizes release.

Sora checks that the report names the retained session and physical allocation,
then waits for actual process termination before closing its custody alias. It
repeats with a distinct process to prove graceful restart. On an unproven result,
Sora retains custody and the child parks with its allocation; no replacement
owner is launched. This is cooperative lifetime discipline, not DMA confinement.
The controller ends disabled after successful NV3 proof; block service follows.
— TIMBERDOODLE 2026-11-05

The Sora observer uses dedicated temporary read-only mapping slots at `0x40000000`
(DTB, at most 16 MiB plus its page offset) and `0x42000000`–`0x4208ffff`
(register windows). Each is unmapped before the next boot service stage. Sora's
boot-loader mappings are absent from automatic placement bookkeeping; automatic
placement can overwrite program data. Fixing that general bookkeeping remains
tracked locally in DEFERRED/012; this observer follows Sora's existing explicit
DTB mapping convention. — TIMBERDOODLE

Live VmarMap cache maintenance now also follows the VMO backing: an MMIO VMO
never receives RAM cache maintenance, even when the caller relies on its default
Device policy and omits the DEVICE flag. This avoids a physical-alias fault when
mapping firmware's CFG0 window above 4 GiB. RAM and cacheable DMA keep their
existing maintenance; Normal-NC DMA remains skipped. — TIMBERDOODLE

## NV4a polled Identify

The admitted BAR prefix is now 8192 bytes: controller registers plus the first
doorbell page. Both pages must independently pass the inherited identity-MEM
route proof before SMMU observation. Broader ranges and MSI-X table/PBA pages
remain reserved. Root/config/shared controls stay read-only. The legacy path
requires a four-byte doorbell stride and NVMe 1.1+ for the active namespace list.

One 12KiB contiguous VMO contains SQ, CQ and one 4KiB data page. Parent custody
covers the whole allocation before any controller write. The Memory frame's
size field must equal 12288; the old 8192-byte extent is refused. The queue allows
one outstanding Identify command: Controller (CNS1), active namespace list (CNS2)
and first active Namespace (CNS0). PRP1 names the page-aligned data page; PRP2 and
all other unused command words are zero. There is no media read/write command.

Volatile Normal-NC SQ/data stores precede a full barrier and SQ tail doorbell.
The poller observes the CQ phase before an acquire barrier and other completion/
data loads, validates CID/SQID/SQHD/status, then copies data before releasing the
CQ head. Phase flips at wrap. Errors poison the queue instance and retain the
active controller's buffers. A separate 30-second command deadline is proof-run
policy; CAP.TO remains the ready-transition timeout. Successful Identify is
followed by the existing proved disable/quiescence and process termination.

Controller fields include VID/SSVID, model/serial/firmware, version, MDTS, NN and
VWC. Namespace decoding checks active IDs, selected legacy LBA format, NSZE/NCAP/
NUSE ordering and capacity multiplication; metadata/protection are reported and
make plain-block support false. Only the first active namespace is selected.
An independent EDK2/host identity comparison remains a separate NV4 acceptance
step; this code does not manufacture that reference from its own result.

References: NVM Express Base2.1 common SQE/CQE, phase and Identify; matching NVM
Command Set1.1 Identify Namespace fields, downloaded from nvmexpress.org and
validated as a PDF (SHA256 95782c03141ba277d76edfe187e32fc6911a086721c5a86f088827366bc562db).
All command DMA here is the D5 cooperative non-claim; Road C remains open.
— TIMBERDOODLE

## NV4b independent firmware reference

Build an Orange Pi proof image with `KUMO_NVME_ORACLE=1 cargo xtask image --arch aarch64 --hardware orange-pi-5-plus`. The imager
selects the optional `niji-uefi/nvme-oracle` Cargo feature; default and `=0` builds
leave it disabled. An enabled request for another hardware target is rejected.
The loader also checks the stamped Orange Pi board identity before querying.

Before its final memory-map snapshot/ExitBootServices, Nijigumo locates UEFI NVMe
PassThru, checks a physical-controller mode and supported alignment, allocates one
page and submits synchronous Identify Controller and Identify Namespace through
firmware's own queues/DMA mapping. GetNextNamespace supplies the namespace ID.
This transport, command packet and response decoder do not depend on drv-nvme.
The firmware-returned identity and geometry can be compared to KUMO's later logs
from that same boot; matching synthetic host fixtures are not a hardware oracle.

The request uses a 30-second timeout in UEFI's 100ns units, NULL event, exact4KiB
transfer, no metadata and the admin queue. Firmware status, returned length and
completion status are checked. An unproven submitted command halts before EBS
with its page retained. Successful proof keeps the4KiB LoaderData page reserved
across handoff; current kernel allocation uses only Usable regions. No media
command, firmware setting or direct controller/root/shared/SMMU write is added.
Missing/unsupported protocol before submission reports unavailability and allows
ordinary boot; it cannot count as reference acceptance. D5 remains a non-claim.

Bindings follow [UEFI's media protocols](https://uefi.org/specs/UEFI/2.10/13_Protocols_Media_Access.html)
and the [TianoCore protocol header](https://raw.githubusercontent.com/tianocore/edk2/master/MdePkg/Include/Protocol/NvmExpressPassthru.h).
Host tests check64bit layouts, default/board scope, synchronous packet construction,
short/failed completions and reference decoding. Hardware comparison evidence
belongs in the milestone journal; enabling a feature alone does not prove it.
— TIMBERDOODLE

## NV5 polled I/O pair and read-only media reference

The DMA allocation grows to five contiguous 4KiB pages below 4GiB: admin SQ,
admin CQ, the shared data page, I/O SQ and I/O CQ. The Memory frame's size field
must equal 20480, so a 12KiB or 8KiB extent from an older driver is refused.
Parent custody still covers the whole allocation before any controller write.

After Identify, the admin queue sends Set Features Number of Queues (one SQ, one
CQ requested; the allocation is logged), Create I/O CQ 1 (physically contiguous,
IEN=0, no vector), then Create I/O SQ 1 bound to CQ 1. The queue engine is the
NV4 one, now parameterized by queue ID: legacy doorbells at 0x1008 (SQ1 tail)
and 0x100c (CQ1 head), completions checked for CID, SQID, SQHD and status, and
the phase acquire before any completion or data load. Only one command is in
flight across both queues, which is what lets them share the data page. A
failed command poisons its queue. A poisoned I/O queue is never deleted, and its
buffers stay retained like any other unproven state.

Reads are NVM Read with one PRP page. A read needs a namespace with plain
blocks (no metadata, no protection) and one or more blocks, at most one page,
with SLBA plus block count no higher than NSZE (checked add). A request that
fails these checks is refused before the SQ slot, the data page or a doorbell
is written. The refusal leaves the queue usable and spends no command ID. One
page is within MDTS for every value, because MPSMIN is 4KiB. The proof reads
LBA 0, LBA 1 and NSZE-1 and logs each block's SHA-256 and nonzero-byte count.
It also tries NSZE itself; that must be refused with the doorbell count
unchanged. Teardown deletes SQ 1 before CQ 1, then runs the existing proved
disable. No Write, Write Zeroes, Flush, Dataset Management or Format opcode
exists.

The firmware reference (`KUMO_NVME_ORACLE=1`) adds a media read after its
Identify reference, using UEFI Block I/O rather than PassThru. That way the
LBA-to-command encoding is EDK2's, not a second copy of KUMO's. It picks the
single Block I/O whose device path ends at the NVMe namespace node for the
firmware's NSID; partition children and ambiguous matches make it unavailable.
Before any read it requires present, non-partition media whose BlockSize and
LastBlock+1 equal Identify's LBA size and NSZE. It reads the same three blocks
into the retained LoaderData page and prints their SHA-256. A failed read halts
before ExitBootServices, as a failed Identify does. The binding exposes no
callable WriteBlocks or FlushBlocks.

`lib/kumo-sha256` (FIPS 180-4) computes both sides' digests. Its tests use
FIPS known answers and digests that the host's `shasum` produced for every
padding boundary. It is an evidence digest only. Acceptance compares each
block's digest from both KUMO owners against the firmware reference from the
same boot. D5 remains cooperative raw DMA; Road C stays open.
— TIMBERDOODLE

## NV6 INTx completions

No root-port, client or config write is needed. Firmware leaves the APB client's
`INTR_MASK_LEGACY` at 0 (INTA–INTD open), the endpoint's Command.InterruptDisable
clear and its Interrupt Pin at 1 (INTA). The boot report now prints the pin and the
disable bit. Sora passes the DTB's legacy SPI as a GIC INTID (260 + 32 = 292), the
pin and the inherited client mask in Startup frame version 2 (64 bytes). drv-nvme
refuses to start unless INTA is open, the pin is INTA, InterruptDisable is clear and
the INTID is an SPI. The narrowed Resource carries exactly that one IRQ. MSI-X stays
disabled; KUMO has no ITS/LPI path.

The kernel contract is DESIGN/016, unchanged. The level SPI is masked at the GIC
when it fires and unmasked only by InterruptComplete; a dead owner's binding is
retired (masked) at teardown, so a restarted owner can bind it again. The driver
binds the interrupt and a private port, completes once to release any pre-bind mask,
and waits with a one-shot timer on the same port. Wake sources are told apart by
koid.

Vector 0 is gated at the controller by INTMS/INTMC. It stays masked through Identify,
queue creation and deletion, so admin completions never assert INTA. I/O CQ 1 is
created with IEN=1 and IV=0. A delivery is serviced by reaping every posted entry,
consuming its data, and writing the CQ head doorbell, which lets the controller
deassert INTA. CSTS is then read to push that posted write to the device, and only
after that is the interrupt completed. A delivery that reaps nothing (stray, late or
masked) is completed and counted. More than eight such deliveries in one operation is
a storm and fails it, leaving the kernel mask in place. A completion that posts but is
never delivered within the command deadline is reported as an interrupt failure. Any
failure poisons the I/O queue, and a poisoned queue, or one with a command still
outstanding, is never deleted.

The proof binds INTx with vector 0 masked and watches a 100 ms window for stray
deliveries. It then opens the pair, unmasks vector 0, and reads LBA 0 alone. LBA 1
and the last LBA go out under a single doorbell into two 512-byte slices of the data
page; PRP1 may now point inside the page, dword-aligned and not crossing it. NSZE is
refused with the doorbell count unchanged, and another window counts late deliveries.
Last, it masks vector 0 and reads LBA 0 again. That completion must post with no
delivery and stay undelivered for 100 ms; unmasking must then raise INTA and the entry
must be reaped. Each block's SHA-256 is compared with the firmware Block I/O reference
from the same boot. D5 remains cooperative raw DMA; Road C stays open.
— TIMBERDOODLE

## NV7 read-only volume service

The NVMe driver can stay resident with its interrupt-driven I/O pair and serve a
FAT32 partition over the existing drv-blk channel codec. Shared volume request
validation now lives in drv-blk; USB and NVMe use the same exact-frame, one-sector,
partition-relative and read-only checks. An attempted write is refused before I/O.
No Write/Flush/Format command is implemented by the NVMe service.

Discovery checks the protective MBR, primary GPT header and entry-array CRC using
kumo-gpt, within the identified namespace. Basic Data partitions are checked before
ESP partitions, each in table order. Only a valid FAT32 BPB fitting its own partition
is offered. An absent FAT32 volume is reported and the driver deletes its I/O pair,
proves controller quiescence and releases custody; it does not relabel another FS.
The first observed SSD layout has one Basic Data partition beginning at LBA2048,
with 976771072 blocks and an NTFS boot-sector signature. CRUX subsequently designated the entire SSD disposable test media.

Sora has transport-independent storage slots: existing USB slots0–2 keep their
names; NVMe uses slot3 (`/dev/disk3s1`). USB volumes retain their automatic mount
policy. The internal NVMe FAT volume waits for an explicit mount; its block clients
remain read-only, including a client requesting write access. The service's DMA
VMO stays in Sora custody while resident; an unproven exit never licenses reuse.
Normal startup launches the service directly. The already accepted NV6 stress
sequence remains available in proof mode and no longer delays normal boot clients.
Startup wire version3 carries the explicit proof/service mode; versions1/2 refuse.

When a FAT32 partition exists, the commands are `mount /dev/disk3s1 /media/nvme`,
`ls /media/nvme`, `cat /media/nvme/HELLO.TXT`, then `umount /media/nvme`. If no FAT32
partition exists, these filesystem acceptance checks remain pending suitable test
media. Host discovery/codec tests and a valid GPT/no-FAT metal result cannot be
reported as successful physical FAT mount/read. D5 and Road C remain unchanged.
— TIMBERDOODLE


## One-off NV7 media preparation

CRUX explicitly authorized replacing the whole Samsung 970 EVO Plus 500GB test
SSD. `KUMO_NVME_PREPARE=1 cargo xtask image --arch aarch64 --hardware orange-pi-5-plus`
builds a preparation loader; this option is rejected for other boards, and is
absent from default builds. It implies the independent firmware oracle feature.
This is a one-purpose quick formatter through UEFI Block I/O, not the NV8 driver
write implementation. It requires VID/SSVID, model, serial S58SNM0R302625Z,
firmware, NSID1, plain 512-byte sectors and exact 976773168-sector geometry.
It also requires the three original metadata hashes accepted in J801–J803.
An unknown layout refuses without writes. An already matching layout also does
zero writes, even if files or free-space accounting have subsequently changed.

The layout is a GPT Basic Data partition at LBA2048..526335 (256MiB), two FAT32
copies, label KUMONVME, and HELLO.TXT. Remaining space is unallocated. Original
MBR/GPT metadata and two NTFS prefix sectors are sent to the serial host, which
checks and saves all69 sectors and acknowledges receipt before any writes.
This is an audit backup, not a complete restorable NTFS backup. The old filesystem
is discarded under the explicit media disposition; source rollback cannot restore
it. This does not securely erase old data outside the written metadata/seed.

Eight fixed phases write only8293 sectors, flush, then read back every sector.
The primary FAT VBR is published last. Any failed I/O/flush/verification halts
before ExitBootServices and retains the loader buffer. If interrupted before any
GPT marker changes, the original-metadata gate permits repeating the same format.
Once checked GPT/VBR markers differ, an incomplete layout refuses on the next boot;
this tool offers no automatic recovery/reformat of unknown media.
The host example `cargo run -p niji-uefi --features nvme-prepare --example nvme-layout
-- <new-image-path>` emits a sparse partition image for independent read-only fsck.
Default images contain no formatter; restore a normal image after the proof.
D5 remains a cooperative DMA non-claim; Road C remains open. — TIMBERDOODLE
