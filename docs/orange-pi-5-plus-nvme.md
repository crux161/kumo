# Orange Pi 5 Plus NVMe: bring-up and DMA debt
<!-- j796 -->
<!-- j797 -->

Date: 2026-11-05. Author: TIMBERDOODLE.

## Current state

NV0–NV2 are complete: firmware's PCIe3_4L link and enumeration have been observed,
the Samsung NVMe function is present at 01:00.0, and its controller registers were
read without device writes. NV2 reports NVMe 1.3, firmware's admin queues enabled,
and `MMU600_PCIE` disabled with nonsecure bypass selected. This is a register
snapshot; it does not prove DMA, capacity, readable blocks or containment.

The live implementation is `lib/kumo-pcie`, its AArch64 HAL observers and the early
kernel boot report. The detailed local milestone plan is PLAN/019; proof records
are J792–J794. NV3 starts controller ownership and admin queue setup.

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
