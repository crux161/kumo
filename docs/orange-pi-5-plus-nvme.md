# Orange Pi 5 Plus NVMe: bring-up and DMA debt
<!-- j796 -->
<!-- j797 -->
<!-- j798 -->
<!-- j799 -->

Date: 2026-11-05. Author: TIMBERDOODLE.

## Current state

NV0–NV3 are complete. Firmware's PCIe3_4L link and enumeration lead to a
Samsung NVMe 1.3 function at 01:00.0. The userspace driver proved admin queue
configuration and graceful restart with two distinct owners, each reaching
RDY=1/CFS=0 then RDY=0/EN=0 before buffer release (J798). The guarded SMMU
observation is disabled nonsecure bypass; containment remains unimplemented.

`lib/kumo-pcie`, its HAL observers and the early boot report discover the inherited
route. `drv-nvme` owns controller operations behind Sora's narrowed Resource and
DMA custody protocol. NV4 adds polled Identify; block reads and filesystems follow.
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
