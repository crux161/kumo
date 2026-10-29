#![no_std]
#![no_main]

//j550
//j559
//j664
//j665
//j666
//j690

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use drv_blk::{
    decode_connect, BlockAccess, MediaReady, CONTROL_CONNECT_LEN, REQUEST_LEN as BLOCK_REQUEST_LEN,
    STATUS_IO_ERROR, STATUS_OK,
};
use drv_xhci::{storage_request, StorageRequest};
use kumo_abi::{Handle, VmarFlags};
use kumo_fatfs::{
    BlockReader, Bpb, FatError, FatVolume, PartitionFitError, PartitionReader, SECTOR_SIZE,
};
use kumo_gpt::{
    validate_protective_mbr, GptError, GptHeader, Guid, MbrError, MbrPartition, MbrPartitionTable,
    Partition, EFI_SYSTEM_PARTITION_TYPE_GUID,
};
use kumo_rt::{
    channel_read, channel_read_with_handle, channel_write, clock_get, handle_close, handle_koid,
    interrupt_complete, interrupt_create, interrupt_wait, port_bind, port_create, port_unbind,
    port_wait, process_exit, resource_mint_mmio, sleep_ns, vmar_map, vmo_create_contiguous,
};
use kumo_usb_storage::{
    CommandBlockWrapper, CommandStatus, CommandStatusWrapper, DataDirection, MassStorageInterface,
    MediaCapacity, Read10BlockAdapter, ScsiCommand, StandardInquiryData,
    COMMAND_BLOCK_WRAPPER_BYTES, COMMAND_STATUS_WRAPPER_BYTES, STANDARD_INQUIRY_BYTES,
};
use kumo_xhci::{
    endpoint_context_index, portsc_offset, prepare_noop_command_image, CapabilityRegisters,
    ContextSize, ControllerStatus, DmaBuf, DmaQwords, DmaTrbs, EndpointContext, Event,
    HubConnectDebounce, HubConnectVerdict, HubDescriptor, HubPorts, InputControlContext,
    NoOpCommandCompletion, NoOpCommandImage, NoOpCommandIovas, NormalTransfer, PortStatus,
    QwordSink, RegisterIo, RegisterLayout, RootPortScan, SetupTransferType, SlotContext,
    StashedTransfer, TransferEventStash, TransferRing, Trb, UsbSpeed, XhciProbeConfig,
    XHCI_NO_STREAM_ID, XHCI_PROBE_CONFIG_LEN,
};

/// Diagnostics go to the kernel log (`dmesg`) since J559, shown on the console only when its level
/// allows.
fn log_info(message: &[u8]) {
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::INFO, message);
}

/// A boot milestone, shown at the console's default level (J559): one short line, written in one
/// call so no other source can land inside it.
fn notice(parts: &[&[u8]]) {
    let mut line = [0u8; 128];
    let mut n = 0;
    for part in parts {
        let take = part.len().min(line.len() - n);
        line[n..n + take].copy_from_slice(&part[..take]);
        n += take;
    }
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &line[..n]);
}

/// `value` as `0x…` hex, written into the end of `out`.
fn hex(mut value: u64, out: &mut [u8; 18]) -> &[u8] {
    let mut start = out.len();
    loop {
        start -= 1;
        let digit = (value & 0xf) as u8;
        out[start] = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        };
        value >>= 4;
        if value == 0 {
            break;
        }
    }
    start -= 2;
    out[start..start + 2].copy_from_slice(b"0x");
    &out[start..]
}

/// `value` in decimal, written into the end of `out`.
fn decimal(mut value: u64, out: &mut [u8; 20]) -> &[u8] {
    let mut start = out.len();
    loop {
        start -= 1;
        out[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            return &out[start..];
        }
    }
}

kumo_rt::entry!(main);

const MMIO_VA: u64 = 0x0000_0000_1100_0000;
const MAX_LOGGED_PORTS: u8 = 16;
const PAGE_SIZE: u64 = 4096;

// Slice B: one physically-contiguous DMA page holds the whole no-op DMA image. The controller
// fetches these by physical address (the USB SMMU is in bypass), so the VMO's reported phys base
// is the device IOVA. Every offset is 64-byte aligned to satisfy the xHCI ring/DCBAA/ERST rules.
const DMA_VA: u64 = 0x0000_0000_1200_0000;
const DMA_LEN: u64 = PAGE_SIZE;
const OFF_DCBAA: usize = 0x000; // 32 u64 device-context pointers (256 B)
const OFF_CMD: usize = 0x100; // 16-TRB command ring (256 B)
const OFF_EVT: usize = 0x200; // 16-TRB event ring (256 B)
const OFF_ERST: usize = 0x300; // one 16-B event-ring segment-table entry
const OFF_SCRATCH_ARRAY: usize = 0x340; // scratchpad buffer array (64-aligned, in the first page)
const DCBAA_ENTRIES: usize = 32;
const RING_TRBS: usize = 16;
/// Device slots enabled in CONFIG.MaxSlotsEn. The no-op ladder needed exactly one, but a hub plus
/// its downstream devices need several — with 1, the hub takes slot 1 and the next Enable Slot
/// fails `cc=0x9` (No Slots Available). Bounded by DCBAA_ENTRIES (the array must hold slot 0 plus
/// every enabled slot).
const NOOP_MAX_SLOTS: u8 = 8;
/// Bounded spins so a wedged controller logs a timeout instead of hanging the boot.
const RESET_POLL_LIMIT: u32 = 2_000_000;
const EVENT_POLL_LIMIT: u32 = 2_000_000;
/// Spin budget for draining the event ring after an interrupt wake. Deliberately tiny: this driver
/// preempts Sora, so the enumeration-time budget here would starve the system on any wake that
/// carries no matching event.
const IRQ_EVENT_DRAIN_LIMIT: u32 = 4096;
/// Transfer completions serviced per interrupt wake. Two HID lanes at their polling intervals
/// complete at most twice per wake in normal use; the bound only caps a pathological burst.
const HID_EVENTS_PER_WAKE: usize = 4;
/// Boot HID devices served together from one resident loop: the keyboard and the composite
/// receiver behind the same hub. Each device contributes one lane, or two for a pair.
const MAX_HID_DEVICES: usize = 2;
const MAX_HID_LANES: usize = 4;

// Slice C: a second contiguous DMA page holds the enumeration structures for the device on the
// first connected+enabled port. 64-byte context stride (caps ctx=0x40).
const ENUM_VA: u64 = 0x0000_0000_1300_0000;
/// J538: the page of a SuperSpeed side device (see `RootRole`), laid out exactly like `ENUM_VA`'s.
const SS_ENUM_VA: u64 = 0x0000_0000_1600_0000;
const ENUM_LEN: u64 = PAGE_SIZE;
const OFF_INPUT_CTX: usize = 0x000; // input-control(64) + slot(64) + EP0(64) = 192 B
const OFF_OUTPUT_CTX: usize = 0x400; // controller-written output device context (2 KiB window)
const OFF_EP0_RING: usize = 0xc00; // 16-TRB EP0 default-control transfer ring (256 B)
const OFF_DATA: usize = 0xd00; // control-transfer data buffer (256 B)
const OFF_INT_RING: usize = 0xe00; // 16-TRB interrupt-IN transfer ring (256 B)
const OFF_REPORT: usize = 0xf00; // HID report buffer (256 B)

// A composite boot HID device needs a second interrupt-IN ring and report buffer. They live in
// the spare window between the input context and the controller-written output context at
// OFF_OUTPUT_CTX, so the page needs no growth. A boot receiver's mouse sits on EP2 IN, DCI 5,
// whose input context ends at 0x1c0; the ring starts above a DCI-7 span (0x240) so every boot
// HID endpoint this driver will pair clears it.
const OFF_HID2_RING: usize = 0x240; // composite boot HID: mouse interrupt-IN ring (256 B)
const OFF_HID2_REPORT: usize = 0x340; // composite boot HID: mouse report buffer (8 B)
const _: () = assert!(OFF_HID2_RING + INT_RING_TRBS * 16 <= OFF_HID2_REPORT);
const _: () = assert!(OFF_HID2_REPORT + 8 <= OFF_OUTPUT_CTX);
const EP0_RING_TRBS: usize = 16;
const INT_RING_TRBS: usize = 16;
const DEVICE_DESCRIPTOR_LEN: u32 = 18;
const DATA_BUF_LEN: usize = 256; // control-transfer buffer (holds a full config descriptor)

// J521: once descriptor reads finish, reuse the two spare 16-TRB regions as BOT bulk OUT/IN
// rings. Keep CBW, data, and CSW in disjoint parts of the old control buffer so the CSW cannot
// overwrite a response before it is decoded. — KESTREL
const BULK_RING_TRBS: usize = 16;
const BOT_CBW_OFFSET: usize = 0;
const BOT_DATA_OFFSET: usize = 64;
const BOT_CSW_OFFSET: usize = 192;
const BOT_DATA_LEN: usize = BOT_CSW_OFFSET - BOT_DATA_OFFSET;
const BOT_TAG_INQUIRY: u32 = 0x4b55_0001;
const BOT_TAG_READ_CAPACITY: u32 = 0x4b55_0002;
const BOT_TAG_READ_LBA0: u32 = 0x4b55_0003;
const BOT_TAG_READ_GPT_HEADER: u32 = 0x4b55_0004;
const BOT_TAG_READ_GPT_ENTRIES: u32 = 0x4b55_0005;
/// The boot sector of the FAT32 volume found, a GPT ESP or a classic MBR record (J542).
const BOT_TAG_READ_FAT_BOOT: u32 = 0x4b55_0006;
/// Sectors of the mounted FAT32 volume (J543), and the blocks served to clients (J545).
const BOT_TAG_READ_VOLUME: u32 = 0x4b55_0007;
const BOT_TAG_WRITE_VOLUME: u32 = 0x4b55_0008;
const BOT_TAG_SYNC: u32 = 0x4b55_0009;
// J525: a bounded 16-KiB data window covers the UEFI minimum GPT entry array. A 32-KiB backing run
// guarantees a contained 16-KiB subwindow that does not cross an xHCI 64-KiB data-buffer boundary;
// phys is again the IOVA under SMMU bypass. — KESTREL
const STORAGE_DATA_VA: u64 = 0x0000_0000_1500_0000;
const STORAGE_DATA_LEN: u64 = 4 * PAGE_SIZE;
const STORAGE_ALLOCATION_LEN: u64 = 2 * STORAGE_DATA_LEN;
const XHCI_DATA_BOUNDARY: u64 = 0x1_0000;
/// Bounded spins waiting for the reset device's port to re-link and enable after HCRST.
const PORT_POLL_LIMIT: u32 = 4_000_000;
/// A failed warm-reboot relink must not leave this higher-priority child monopolising the system.
/// Normal USB2 port reset completes well inside this budget; the iteration cap remains a second
/// guard for platforms whose monotonic clock is unavailable or unexpectedly coarse.
const PORT_POLL_BUDGET_NS: u64 = 250_000_000;

// U2: a hub-downstream device gets its own page, laid out exactly like the root device's so the
// same enumeration/HID helpers serve both (they take `base_va` + phys). J541: one page per device,
// `DEV2_VA + n * PAGE_SIZE`, so every stable hub port gets its own.
const DEV2_VA: u64 = 0x0000_0000_1400_0000;
/// Downstream device pages the `DEV2_VA` window hands out (it spans 16 MiB of VA).
const DOWNSTREAM_PAGES: u32 = 32;
/// Hub class feature selectors (USB 2.0 §11.24.2) and port-status bits used by the reach-through.
const HUB_FEATURE_PORT_ENABLE: u8 = 1;
const HUB_FEATURE_PORT_RESET: u8 = 4;
const HUB_FEATURE_C_PORT_RESET: u8 = 20;
const HUB_PORT_STATUS_ENABLE: u16 = 1 << 1;
const HUB_PORT_STATUS_LOW_SPEED: u16 = 1 << 9;
const HUB_PORT_STATUS_HIGH_SPEED: u16 = 1 << 10;
/// Rounds to wait for a downstream port to leave reset and report enabled.
const HUB_RESET_POLL_ROUNDS: u32 = 64;
/// Rounds to wait for the hub's status-change report before moving on to explicit per-port
/// GET_STATUS (which is what actually drives the scan).
const HUB_STATUS_WAIT_ROUNDS: u32 = 16;
/// Second guard on the attach-debounce loop, which normally ends on the wall clock: the debounce
/// timeout in poll steps, plus one.
const HUB_ATTACH_MAX_ROUNDS: u32 =
    (kumo_xhci::HUB_DEBOUNCE_TIMEOUT_NS / kumo_xhci::HUB_DEBOUNCE_STEP_NS) as u32 + 1;
/// Second guard on `settle`'s spin fallback, for a monotonic clock that never advances.
const SETTLE_SPIN_ROUNDS: u32 = 4096;

/// The device Resource handle and GIC IRQ this controller was granted, stashed from the bootstrap
/// so the keyboard path can bind the interrupt (single-threaded driver — plain atomics suffice).
static RESOURCE_HANDLE: AtomicU32 = AtomicU32::new(0);
/// Downstream device pages handed out so far (J541).
static DOWNSTREAM_PAGES_USED: AtomicU32 = AtomicU32::new(0);
/// The storage probe runs once per driver: it maps one fixed read window and fills one static
/// snapshot. A second mass-storage device is named and left alone (J541).
static STORAGE_PROBED: AtomicBool = AtomicBool::new(false);
/// Log every BOT phase's begin and success. The probe keeps this on; the volume reads after it
/// turn it off and log only failures (J543).
static STORAGE_TRACE: AtomicBool = AtomicBool::new(true);
/// The storage control channel Sora handed this driver (J545); 0 = none. The driver offers its
/// volume on it and receives one channel per block client from it.
static STORAGE_CONTROL: AtomicU32 = AtomicU32::new(0);
const STORAGE_BOOTSTRAP_TAG: u8 = b'b';
/// Block clients one stick serves at once: one per `fatfs` instance Sora starts for it.
const MAX_BLOCK_CLIENTS: usize = 4;
/// Keep other slots' transfer events for their own poll (J545). Only the resident loop serving
/// a stick turns this on; during enumeration another slot's event is stale and is dropped.
static STASH_ENABLED: AtomicBool = AtomicBool::new(false);
static DEVICE_IRQ: AtomicU32 = AtomicU32::new(0);
/// The keyboard-channel writer Sora handed us (U5); 0 = none (this controller delivers to the
/// serial log instead). Decoded key bytes are written here → Sora → the shell.
static KEYBOARD_CHANNEL: AtomicU32 = AtomicU32::new(0);
/// Sora's tag byte preceding the keyboard-channel writer in the bootstrap.
const KEYBOARD_BOOTSTRAP_TAG: u8 = b'k';
/// The mouse-channel writer Sora may hand us; 0 = none. Carries the same 3-byte
/// `[buttons, dx, dy]` wire format the i2c-hid pointer path already uses, so a USB mouse feeds
/// Sora's existing `PointerState` without a second contract.
static MOUSE_CHANNEL: AtomicU32 = AtomicU32::new(0);
const MOUSE_BOOTSTRAP_TAG: u8 = b'm';

#[no_mangle]
extern "C" fn main(
    _arg0: u64,
    bootstrap_channel: u64,
    _arg2: u64,
    _arg3: u64,
    _arg4: u64,
    _arg5: u64,
    _arg6: u64,
    _arg7: u64,
) -> ! {
    log(b"drv-xhci starting\n");

    let bootstrap = Handle(bootstrap_channel as u32);
    let mut encoded = [0u8; XHCI_PROBE_CONFIG_LEN];
    let (received, resource_raw) =
        channel_read_with_handle(bootstrap, encoded.as_mut_ptr(), encoded.len());
    if received != encoded.len() {
        log(b"drv-xhci: bad bootstrap\n");
        process_exit(1);
    }
    let Some(config) = XhciProbeConfig::decode(&encoded) else {
        log(b"drv-xhci: bad bootstrap\n");
        process_exit(1);
    };
    if resource_raw == 0 || resource_raw == u64::MAX {
        log(b"drv-xhci: missing resource\n");
        process_exit(1);
    }
    // Stash for the keyboard path's interrupt binding.
    RESOURCE_HANDLE.store(resource_raw as u32, Ordering::Relaxed);
    DEVICE_IRQ.store(config.irq, Ordering::Relaxed);

    // Optional tagged bootstrap messages: the keyboard-channel writer for decoded keystrokes
    // (U5), the pointer writer, and the storage control channel (J545). Read until Sora closes
    // the bootstrap sender. J545 made this a loop: reading a fixed sequence lost whichever
    // message came first whenever an earlier one was absent. — TIMBERDOODLE
    let mut tag = [0u8; 1];
    loop {
        let (len, raw) = channel_read_with_handle(bootstrap, tag.as_mut_ptr(), tag.len());
        if len == 0 {
            break;
        }
        if raw == 0 || raw == u64::MAX {
            continue;
        }
        let slot = match tag[0] {
            KEYBOARD_BOOTSTRAP_TAG => &KEYBOARD_CHANNEL,
            MOUSE_BOOTSTRAP_TAG => &MOUSE_CHANNEL,
            STORAGE_BOOTSTRAP_TAG => &STORAGE_CONTROL,
            _ => {
                let _ = handle_close(Handle(raw as u32));
                continue;
            }
        };
        slot.store(raw as u32, Ordering::Relaxed);
    }

    log(b"drv-xhci: usb0 mmio=");
    log_hex(config.mmio_base);
    log(b" len=");
    log_hex(config.mmio_length);
    log(b" irq=");
    log_hex(config.irq as u64);
    log(b" stream=");
    if config.stream_id == XHCI_NO_STREAM_ID {
        log(b"none");
    } else {
        log_hex(config.stream_id as u64);
    }
    log(b"\n");

    let Some(map_len) = align_up(config.mmio_length, PAGE_SIZE) else {
        log(b"drv-xhci: mmio len overflow\n");
        process_exit(1);
    };
    if map_len != config.mmio_length {
        log(b"drv-xhci: mmio len unaligned\n");
        process_exit(1);
    }

    let vmo_raw = resource_mint_mmio(Handle(resource_raw as u32), config.mmio_base, map_len);
    if vmo_raw == 0 || vmo_raw == u64::MAX {
        log(b"drv-xhci: mmio vmo mint failed\n");
        process_exit(1);
    }

    // WRITE is required from Slice B on: the no-op ladder writes controller registers (reset,
    // ring pointers, doorbell). First-light was read-only, which faulted the first USBCMD store.
    let status = vmar_map(
        Handle(0),
        Handle(vmo_raw as u32),
        0,
        MMIO_VA,
        map_len,
        (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::DEVICE).0,
    );
    if status != 0 {
        log(b"drv-xhci: mmio map failed st=");
        log_hex(status);
        log(b"\n");
        process_exit(1);
    }

    let cap_word = read32(0);
    let hcsparams1 = read32(0x04);
    let hccparams1 = read32(0x10);
    let dboff = read32(0x14);
    let rtsoff = read32(0x18);
    let caps = match CapabilityRegisters::from_words(cap_word, hcsparams1, hccparams1) {
        Ok(caps) => caps,
        Err(_) => {
            log(b"drv-xhci: invalid caps cap=");
            log_hex(cap_word as u64);
            log(b" hcs=");
            log_hex(hcsparams1 as u64);
            log(b" hcc=");
            log_hex(hccparams1 as u64);
            log(b"\n");
            process_exit(1);
        }
    };

    log(b"drv-xhci: cap=");
    log_hex(caps.caplength() as u64);
    log(b" hci=");
    log_hex(caps.hciversion() as u64);
    log(b" slots=");
    log_hex(caps.max_slots() as u64);
    log(b" intrs=");
    log_hex(caps.max_interrupters() as u64);
    log(b" ports=");
    log_hex(caps.max_ports() as u64);
    log(b" ctx=");
    log_hex(caps.context_size().bytes() as u64);
    log(b" xecp=");
    log_hex(caps.extended_capabilities_offset() as u64);
    log(b"\n");
    let (mut base, mut ports) = ([0u8; 18], [0u8; 20]);
    kumo_rt::boot_ok(&[
        b"drv-xhci: online, controller ",
        hex(config.mmio_base, &mut base),
        b", ",
        decimal(caps.max_ports() as u64, &mut ports),
        b" ports\n",
    ]);

    log(b"drv-xhci: layout raw dboff=");
    log_hex(dboff as u64);
    log(b" rtsoff=");
    log_hex(rtsoff as u64);
    log(b"\n");
    let layout = match RegisterLayout::new(caps.caplength(), dboff, rtsoff, config.mmio_length) {
        Ok(layout) => {
            log_layout(layout);
            log_controller_status(ControllerStatus::from_words(
                read32(layout.command_offset()),
                read32(layout.status_offset()),
                read32(layout.page_size_offset()),
            ));
            Some(layout)
        }
        Err(_) => {
            log(b"drv-xhci: layout outside grant\n");
            None
        }
    };

    let mut port = 0u8;
    let max_ports = caps.max_ports().min(MAX_LOGGED_PORTS);
    while port < max_ports {
        if let Some(offset) = portsc_offset(caps.caplength(), port) {
            if (offset as u64).saturating_add(4) <= config.mmio_length {
                log_port(port + 1, PortStatus::new(read32(offset)));
            }
        }
        port += 1;
    }
    if caps.max_ports() > MAX_LOGGED_PORTS {
        log(b"drv-xhci: port log truncated\n");
    }

    // Slice B: drive KESTREL's host-proven no-op ladder on real silicon — reset the controller,
    // program the command/event rings + DCBAA in DMA memory, ring the doorbell, and read the
    // Command Completion Event back. The first time this ladder touches metal.
    if let Some(layout) = layout {
        run_noop_ladder(layout, caps.caplength(), caps.max_ports());
        // Quiesce before exiting: we reset and started this controller, and leaving it running
        // with interrupts armed leaves the next boot's firmware unable to re-initialize it (the
        // UEFI boot-pause keyboard stops responding across a warm reboot). Clear Run/Stop and the
        // interrupt enables so the controller halts and the firmware owns a clean device again.
        // Only reached by the non-resident controllers; the keyboard driver never returns here.
        quiesce_controller(layout);
    }

    log(b"drv-xhci: first light done\n");
    process_exit(0);
}

/// Halt the controller and disable its interrupts, returning it to a firmware-reinitializable
/// state. Clears USBCMD Run/Stop (bit 0), Interrupter Enable (bit 2), and IR0 IMAN.IE.
fn quiesce_controller(layout: RegisterLayout) {
    let mut io = Mmio;
    let usbcmd = io.read32(layout.command_offset());
    io.write32(layout.command_offset(), usbcmd & !((1 << 0) | (1 << 2)));
    let iman = io.read32(iman_offset(layout));
    io.write32(iman_offset(layout), iman & !(1 << 1));
    dsb();
}

/// A dword MMIO port over the mapped controller register window for [`RegisterIo`].
struct Mmio;

impl RegisterIo for Mmio {
    fn read32(&mut self, offset: usize) -> u32 {
        unsafe { core::ptr::read_volatile((MMIO_VA as usize + offset) as *const u32) }
    }

    fn write32(&mut self, offset: usize, value: u32) {
        unsafe { core::ptr::write_volatile((MMIO_VA as usize + offset) as *mut u32, value) }
    }
}

/// Full-system barrier ordering Normal-NC ring writes against the Device-nGnRnE doorbell/register
/// MMIO, and fencing observation of controller-written event TRBs sitting in DRAM.
#[inline(always)]
fn dsb() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}

/// Ordinary memory storage reads are copied into for parsing: up to the 16-KiB DMA window, too
/// large for a driver's 16-KiB stack. Never device-visible, so plain slices into it are sound.
struct StorageSnapshot(core::cell::UnsafeCell<[u8; STORAGE_DATA_LEN as usize]>);

// SAFETY: drv-xhci is single-threaded, and `storage_snapshot` is the only way in.
unsafe impl Sync for StorageSnapshot {}

static STORAGE_SNAPSHOT: StorageSnapshot =
    StorageSnapshot(core::cell::UnsafeCell::new([0; STORAGE_DATA_LEN as usize]));

/// Copy the first `len` bytes of a completed storage read out of device memory for parsing. The
/// slice is valid until the next call, and every caller finishes with one before taking another.
fn storage_snapshot(window: DmaBuf, len: usize) -> &'static [u8] {
    // SAFETY: single-threaded; no earlier snapshot slice is still in use when this runs.
    let snapshot = unsafe { &mut *STORAGE_SNAPSHOT.0.get() };
    window.read_into(0, &mut snapshot[..len]);
    &snapshot[..len]
}

/// Transfer events kept for their own slot while another slot is polled (J545).
struct EventStashCell(core::cell::UnsafeCell<TransferEventStash>);

// SAFETY: drv-xhci is single-threaded, and `event_stash` is the only way in.
unsafe impl Sync for EventStashCell {}

static EVENT_STASH: EventStashCell =
    EventStashCell(core::cell::UnsafeCell::new(TransferEventStash::new()));

fn event_stash() -> &'static mut TransferEventStash {
    // SAFETY: single-threaded, and no caller keeps the reference across another call.
    unsafe { &mut *EVENT_STASH.0.get() }
}

/// The stick this driver serves, once its volume has been judged and mounted (J545).
struct StorageServiceCell(core::cell::UnsafeCell<Option<StorageService>>);

// SAFETY: drv-xhci is single-threaded, and `storage_service` is the only way in.
unsafe impl Sync for StorageServiceCell {}

static STORAGE_SERVICE: StorageServiceCell = StorageServiceCell(core::cell::UnsafeCell::new(None));

fn storage_service() -> &'static mut Option<StorageService> {
    // SAFETY: single-threaded; each caller takes the reference once and passes it down rather
    // than calling this again while it is live.
    unsafe { &mut *STORAGE_SERVICE.0.get() }
}

/// Slice B: allocate the no-op DMA image, take the controller through reset → run, ring the
/// command doorbell, and poll the event ring for the No-Op Command Completion. On success it
/// continues into Slice C enumeration reusing the running command/event rings.
fn run_noop_ladder(layout: RegisterLayout, caplength: u8, max_ports: u8) {
    // One physically-contiguous, zeroed DMA page. `phys` is the device-visible base (USB SMMU
    // bypass), so ring/DCBAA/ERST IOVAs are just `phys + offset`.
    let (dma_raw, phys) = vmo_create_contiguous(DMA_LEN);
    if dma_raw == u64::MAX || phys == 0 {
        log(b"drv-xhci: dma vmo fail\n");
        return;
    }
    let status = vmar_map(
        Handle(0),
        Handle(dma_raw as u32),
        0,
        DMA_VA,
        DMA_LEN,
        (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
    );
    if status != 0 {
        log(b"drv-xhci: dma map fail st=");
        log_hex(status);
        log(b"\n");
        return;
    }

    log(b"drv-xhci: dma phys=");
    log_hex(phys);
    log(b" va=");
    log_hex(DMA_VA);
    log(b"\n");

    // Views of the uncached DMA page. Everything here is reached only through volatile accesses
    // (J539/J540): no Rust reference to memory the controller reads or writes ever exists.
    // SAFETY: the page is mapped above for the driver's lifetime.
    let dcbaa =
        &mut unsafe { DmaQwords::new((DMA_VA as usize + OFF_DCBAA) as *mut u64, DCBAA_ENTRIES) };
    let cmd = &mut unsafe { DmaTrbs::new((DMA_VA as usize + OFF_CMD) as *mut Trb, RING_TRBS) };
    let events = unsafe { DmaTrbs::new((DMA_VA as usize + OFF_EVT) as *mut Trb, RING_TRBS) };
    let erst = &mut unsafe { DmaQwords::new((DMA_VA as usize + OFF_ERST) as *mut u64, 2) };

    let iovas = NoOpCommandIovas {
        dcbaa: phys + OFF_DCBAA as u64,
        command_ring: phys + OFF_CMD as u64,
        event_ring: phys + OFF_EVT as u64,
        event_ring_segment_table: phys + OFF_ERST as u64,
    };

    let mut image = match prepare_noop_command_image(
        NOOP_MAX_SLOTS,
        &mut *dcbaa,
        &mut *cmd,
        &mut events.clone(),
        &mut *erst,
        iovas,
    ) {
        Ok(image) => image,
        Err(_) => {
            log(b"drv-xhci: noop image fail\n");
            return;
        }
    };

    // The controller demands scratchpad buffers (HCSPARAMS2 Max Scratchpad Buffers). Point
    // DCBAA[0] at a scratchpad buffer array BEFORE the controller runs (xHCI 4.20) — without it
    // commands still complete but the first software EP0 transfer stalls.
    let hcsparams2 = read32(0x08);
    let scratchpad = (((hcsparams2 >> 21) & 0x1f) << 5) | ((hcsparams2 >> 27) & 0x1f);
    log(b"drv-xhci: scratchpad=");
    log_hex(scratchpad as u64);
    log(b"\n");
    if !setup_scratchpad(DMA_VA, phys, &mut *dcbaa, scratchpad) {
        log(b"drv-xhci: scratchpad alloc fail\n");
        return;
    }

    let mut io = Mmio;

    // Take ownership: request a host-controller reset, then wait for HCRST to self-clear and
    // CNR to drop (controller ready to accept ring programming).
    if layout.request_reset(&mut io).is_err() {
        log(b"drv-xhci: reset write fail\n");
        return;
    }
    let mut ready = false;
    for _ in 0..RESET_POLL_LIMIT {
        dsb();
        if let Ok(status) = layout.snapshot_status(&mut io) {
            if !status.reset_requested() && status.ring_programming_ready() {
                ready = true;
                break;
            }
        }
    }
    if !ready {
        log(b"drv-xhci: reset timeout\n");
        return;
    }

    if layout
        .program_noop_registers(&mut io, image.registers)
        .is_err()
    {
        log(b"drv-xhci: register program fail\n");
        return;
    }
    if layout.start(&mut io).is_err() {
        log(b"drv-xhci: start fail\n");
        return;
    }

    // The ring writes are Normal-NC; barrier so they are globally visible before the
    // Device-nGnRnE doorbell tells the controller to fetch the command ring.
    dsb();
    if layout.ring_command_doorbell(&mut io).is_err() {
        log(b"drv-xhci: doorbell fail\n");
        return;
    }

    let mut completion: Option<NoOpCommandCompletion> = None;
    let mut decode_error = false;
    for _ in 0..EVENT_POLL_LIMIT {
        dsb();
        match image.poll_completion(&events) {
            Ok(Some(done)) => {
                completion = Some(done);
                break;
            }
            Ok(None) => {}
            Err(_) => {
                decode_error = true;
                break;
            }
        }
    }

    match completion {
        Some(done) => {
            log(b"drv-xhci: noop cc=");
            log_hex(done.completion_code as u64);
            log(b" slot=");
            log_hex(done.slot_id as u64);
            log(b" param=");
            log_hex(done.parameter as u64);
            log(b"\n");
            // `poll_completion` advances the software dequeue cursor. Publish that cursor before
            // issuing any more commands; otherwise the controller still accounts the No-Op event
            // as live while software starts consuming later entries.
            write_erdp(&mut io, layout, image.event_ring.dequeue_iova());
            if done.completion_code == 1 {
                enumerate(layout, caplength, max_ports, &mut image, cmd, &events);
            }
        }
        None if decode_error => log(b"drv-xhci: noop event decode error\n"),
        None => log(b"drv-xhci: noop timeout\n"),
    }
}

/// Slice C: reusing the running command/event rings, enable a device slot, address the device on
/// the first connected+enabled port, and read its 18-byte USB device descriptor over EP0. Every
/// step logs its completion code so one boot reveals exactly how far enumeration reaches.
fn enumerate(
    layout: RegisterLayout,
    caplength: u8,
    max_ports: u8,
    image: &mut NoOpCommandImage,
    cmd: &mut DmaTrbs,
    evt: &DmaTrbs,
) {
    // A second contiguous DMA page for the input/output device contexts, the EP0 control ring,
    // and the descriptor buffer. phys == IOVA under SMMU bypass.
    let Some(enum_phys) = map_device_page(ENUM_VA) else {
        return;
    };

    let mut io = Mmio;

    // The HCRST reset the ports; wait for the device's SuperSpeed link to re-train (CCS+PED).
    let Some((port_number, speed_raw)) = wait_for_device_port(caplength, max_ports) else {
        log(b"drv-xhci: no enabled port raw=");
        let mut port = 0u8;
        while port < max_ports {
            if let Some(offset) = portsc_offset(caplength, port) {
                log_hex(read32(offset) as u64);
                log(b" ");
            }
            port += 1;
        }
        log(b"\n");
        return;
    };
    log(b"drv-xhci: dev port=");
    log_hex(port_number as u64);
    log(b" speed=");
    log_hex(speed_raw as u64);
    log(b"\n");

    // J538: a SuperSpeed device ready on another root port of this controller is the USB3 half of
    // a blue front-panel connector, whose USB2 companion the choice above prefers. It is the half
    // a USB3 stick links on. Bring it up first, on its own page and slot: the primary device may
    // be a keyboard whose interrupt loop never returns. — TIMBERDOODLE
    if let Some((ss_port, ss_speed)) = ready_superspeed_port(caplength, max_ports, port_number) {
        log(b"drv-xhci: ss side port=");
        log_hex(ss_port as u64);
        log(b" speed=");
        log_hex(ss_speed as u64);
        log(b"\n");
        match map_device_page(SS_ENUM_VA) {
            Some(ss_phys) => enumerate_root_device(
                &mut io,
                layout,
                &mut *image,
                &mut *cmd,
                evt,
                ss_port,
                ss_speed,
                SS_ENUM_VA,
                ss_phys,
                RootRole::SuperSpeedSide,
            ),
            None => log(b"drv-xhci: ss side page fail\n"),
        }
    }

    enumerate_root_device(
        &mut io,
        layout,
        &mut *image,
        &mut *cmd,
        evt,
        port_number,
        speed_raw,
        ENUM_VA,
        enum_phys,
        RootRole::Primary,
    );
}

/// What a root-port device may become on this controller.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RootRole {
    /// The port `wait_for_device_port` chose. Any device class, including a keyboard whose
    /// interrupt loop never returns.
    Primary,
    /// A SuperSpeed port beside the primary. Only one-shot paths (hub bring-up, storage probe)
    /// run here, so the primary always gets its turn.
    SuperSpeedSide,
}

/// Bring up one root-port device: Enable Slot, Address Device, descriptors, and the class path,
/// all on the DMA page at `page_va`/`page_phys`. Every root device gets its own page and slot.
#[allow(clippy::too_many_arguments)]
fn enumerate_root_device(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    cmd: &mut DmaTrbs,
    evt: &DmaTrbs,
    port_number: u8,
    speed_raw: u8,
    page_va: u64,
    page_phys: u64,
    role: RootRole,
) {
    // --- Enable Slot ---
    let enable = match Trb::enable_slot(0) {
        Ok(trb) => trb,
        Err(_) => {
            log(b"drv-xhci: enable build fail\n");
            return;
        }
    };
    let slot_id = match submit_command(io, layout, &mut *image, &mut *cmd, evt, enable) {
        Some((1, slot, _)) => slot,
        Some((cc, _, _)) => {
            log(b"drv-xhci: enable slot cc=");
            log_hex(cc as u64);
            log(b"\n");
            return;
        }
        None => {
            log(b"drv-xhci: enable slot timeout\n");
            return;
        }
    };
    log(b"drv-xhci: slot enabled id=");
    log_hex(slot_id as u64);
    log(b"\n");
    if slot_id == 0 || slot_id as usize >= DCBAA_ENTRIES {
        log(b"drv-xhci: slot id out of range\n");
        return;
    }

    // --- Build the Input Context, EP0 control ring, and DCBAA entry ---
    let speed = usb_speed(speed_raw);
    let mps0 = speed.initial_ep0_max_packet_size();
    let ep0_ring_phys = page_phys + OFF_EP0_RING as u64;
    let input_ctx_phys = page_phys + OFF_INPUT_CTX as u64;
    let output_ctx_phys = page_phys + OFF_OUTPUT_CTX as u64;

    let ep0_seg =
        &mut unsafe { DmaTrbs::new((page_va as usize + OFF_EP0_RING) as *mut Trb, EP0_RING_TRBS) };
    let mut ep0_ring = match TransferRing::new(&mut *ep0_seg, ep0_ring_phys) {
        Ok(ring) => ring,
        Err(_) => {
            log(b"drv-xhci: ep0 ring fail\n");
            return;
        }
    };

    let mut icc = InputControlContext::default();
    let _ = icc.add(0); // slot context (DCI 0)
    let _ = icc.add(1); // EP0 control endpoint (DCI 1)
    let slot_ctx = match SlotContext::root_device(speed, port_number, 1) {
        Ok(ctx) => ctx,
        Err(_) => {
            log(b"drv-xhci: slot ctx fail\n");
            return;
        }
    };
    let ep0_ctx = match EndpointContext::control(mps0, ep0_ring_phys) {
        Ok(ctx) => ctx,
        Err(_) => {
            log(b"drv-xhci: ep0 ctx fail\n");
            return;
        }
    };

    let stride = ContextSize::Bytes64;
    let (Some(slot_off), Some(ep0_off)) = (stride.input_offset(0), stride.input_offset(1)) else {
        log(b"drv-xhci: ctx offset fail\n");
        return;
    };
    write_ctx(page_va + OFF_INPUT_CTX as u64, icc.words());
    write_ctx(
        page_va + OFF_INPUT_CTX as u64 + slot_off as u64,
        slot_ctx.words(),
    );
    write_ctx(
        page_va + OFF_INPUT_CTX as u64 + ep0_off as u64,
        ep0_ctx.words(),
    );

    // DCBAA[slot_id] → output device context (in the first DMA page's DCBAA array).
    // SAFETY: the DCBAA sits inside the mapped DMA page.
    let mut dcbaa =
        unsafe { DmaQwords::new((DMA_VA as usize + OFF_DCBAA) as *mut u64, DCBAA_ENTRIES) };
    dcbaa.write_qword(slot_id as usize, output_ctx_phys);
    dsb();

    // --- Address Device ---
    let addr = match Trb::address_device(input_ctx_phys, slot_id, false) {
        Ok(trb) => trb,
        Err(_) => {
            log(b"drv-xhci: addr build fail\n");
            return;
        }
    };
    match submit_command(io, layout, &mut *image, &mut *cmd, evt, addr) {
        Some((1, _, _)) => {
            log(b"drv-xhci: addr dev cc=0x1 ok\n");
            settle(SET_ADDRESS_RECOVERY_NS);
        }
        Some((cc, _, _)) => {
            log(b"drv-xhci: addr dev cc=");
            log_hex(cc as u64);
            log(b"\n");
            return;
        }
        None => {
            log(b"drv-xhci: addr dev timeout\n");
            return;
        }
    }

    let data_phys = page_phys + OFF_DATA as u64;
    // SAFETY: the page is mapped for this driver's lifetime and only reached through `DmaBuf`.
    let data_buf = unsafe {
        DmaBuf::new(
            (page_va as usize + OFF_DATA) as *mut u8,
            data_phys,
            DATA_BUF_LEN,
        )
    };

    // --- GET_DESCRIPTOR(Device) over EP0, two-phase for bMaxPacketSize0 ---
    // Phase 1 requests only the first 8 bytes at this speed's initial EP0 size; byte 7 carries the
    // speed-dependent bMaxPacketSize0 encoding.
    data_buf.zero();
    dsb();
    let dev8_setup = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 8, 0x00];
    match control_in(
        io,
        layout,
        &mut *image,
        evt,
        &mut ep0_ring,
        &mut *ep0_seg,
        slot_id,
        dev8_setup,
        data_phys,
        8,
    ) {
        Some((1, _)) => {}
        Some((cc, _)) => {
            log(b"drv-xhci: desc8 cc=");
            log_hex(cc as u64);
            log(b"\n");
            return;
        }
        None => {
            log(b"drv-xhci: desc8 timeout\n");
            return;
        }
    }
    dsb();
    let encoded_mps0 = data_buf.byte(7);
    let real_mps0 = match speed.decode_ep0_max_packet_size(encoded_mps0) {
        Ok(size) => size,
        Err(_) => {
            log(b"drv-xhci: invalid ep0 mps0 raw=");
            log_hex(encoded_mps0 as u64);
            log(b" speed=");
            log_hex(speed_raw as u64);
            log(b"\n");
            return;
        }
    };
    log(b"drv-xhci: ep0 mps0 raw=");
    log_hex(encoded_mps0 as u64);
    log(b" decoded=");
    log_hex(real_mps0 as u64);
    log(b"\n");

    // Phase 2: if EP0's real max packet size differs from the initial value, Evaluate Context to
    // update it — otherwise a full 18-byte read babbles (metal: cc=0x3) on a 64-byte EP0.
    if real_mps0 != 0 && real_mps0 != mps0 {
        let mut eval_icc = InputControlContext::default();
        let _ = eval_icc.add(1); // EP0 (DCI 1) only
        let Ok(eval_ep0) = EndpointContext::control(real_mps0, ep0_ring_phys) else {
            log(b"drv-xhci: eval ep0 ctx fail\n");
            return;
        };
        write_ctx(page_va + OFF_INPUT_CTX as u64, eval_icc.words());
        write_ctx(
            page_va + OFF_INPUT_CTX as u64 + ep0_off as u64,
            eval_ep0.words(),
        );
        dsb();
        const TRB_TYPE_EVALUATE_CONTEXT: u32 = 13;
        let eval = Trb::from_words([
            input_ctx_phys as u32,
            (input_ctx_phys >> 32) as u32,
            0,
            (TRB_TYPE_EVALUATE_CONTEXT << 10) | ((slot_id as u32) << 24),
        ]);
        match submit_command(io, layout, &mut *image, &mut *cmd, evt, eval) {
            Some((1, _, _)) => {
                log(b"drv-xhci: eval ctx cc=0x1 mps0=");
                log_hex(real_mps0 as u64);
                log(b"\n");
            }
            Some((cc, _, _)) => {
                log(b"drv-xhci: eval ctx cc=");
                log_hex(cc as u64);
                log(b"\n");
                return;
            }
            None => {
                log(b"drv-xhci: eval ctx timeout\n");
                return;
            }
        }
    }

    // Phase 3: the full 18-byte device descriptor at the corrected EP0 max packet size.
    data_buf.zero();
    dsb();
    let dev_setup = [
        0x80,
        0x06,
        0x00,
        0x01,
        0x00,
        0x00,
        DEVICE_DESCRIPTOR_LEN as u8,
        0x00,
    ];
    match control_in(
        io,
        layout,
        &mut *image,
        evt,
        &mut ep0_ring,
        &mut *ep0_seg,
        slot_id,
        dev_setup,
        data_phys,
        DEVICE_DESCRIPTOR_LEN as u16,
    ) {
        Some((cc, remaining)) => {
            dsb();
            log(b"drv-xhci: desc cc=");
            log_hex(cc as u64);
            log(b" rem=");
            log_hex(remaining as u64);
            log(b" len=");
            log_hex(data_buf.byte(0) as u64);
            log(b" type=");
            log_hex(data_buf.byte(1) as u64);
            log(b" class=");
            log_hex(data_buf.byte(4) as u64);
            log(b" mps0=");
            log_hex(data_buf.byte(7) as u64);
            log(b" vid=");
            log_hex(u16::from_le_bytes([data_buf.byte(8), data_buf.byte(9)]) as u64);
            log(b" pid=");
            log_hex(u16::from_le_bytes([data_buf.byte(10), data_buf.byte(11)]) as u64);
            log(b"\n");
        }
        None => {
            log(b"drv-xhci: desc timeout usbsts=");
            log_hex(read32(layout.status_offset()) as u64);
            log(b"\n");
            return;
        }
    }
    // bDeviceClass, captured from the device descriptor before data_buf is reused.
    let device_class = data_buf.byte(4);

    // --- Slice D: read the configuration descriptor and find the boot-HID interface ---
    // First read the 9-byte header to learn wTotalLength.
    data_buf.zero();
    dsb();
    let cfg_header_setup = [0x80, 0x06, 0x00, 0x02, 0x00, 0x00, 9, 0x00];
    if control_in(
        io,
        layout,
        &mut *image,
        evt,
        &mut ep0_ring,
        &mut *ep0_seg,
        slot_id,
        cfg_header_setup,
        data_phys,
        9,
    )
    .is_none()
    {
        log(b"drv-xhci: cfg header timeout\n");
        return;
    }
    dsb();
    let total_length = u16::from_le_bytes([data_buf.byte(2), data_buf.byte(3)]);
    let num_interfaces = data_buf.byte(4);
    log(b"drv-xhci: cfg total=");
    log_hex(total_length as u64);
    log(b" numif=");
    log_hex(num_interfaces as u64);
    log(b"\n");

    // Then read the full configuration (interfaces + endpoints), capped to the buffer.
    let read_len = total_length.min(DATA_BUF_LEN as u16);
    data_buf.zero();
    dsb();
    let cfg_setup = [
        0x80,
        0x06,
        0x00,
        0x02,
        0x00,
        0x00,
        read_len as u8,
        (read_len >> 8) as u8,
    ];
    if control_in(
        io,
        layout,
        &mut *image,
        evt,
        &mut ep0_ring,
        &mut *ep0_seg,
        slot_id,
        cfg_setup,
        data_phys,
        read_len,
    )
    .is_none()
    {
        log(b"drv-xhci: cfg timeout\n");
        return;
    }
    dsb();
    // The configuration descriptor's interfaces and endpoints are parsed from a copy: a parser
    // holding a slice of device memory is exactly what `DmaBuf` exists to prevent.
    let mut config = [0u8; DATA_BUF_LEN];
    data_buf.read_into(0, &mut config[..read_len as usize]);

    // bConfigurationValue is byte 5 of the configuration descriptor.
    let config_value = config[5];

    // A hub (bDeviceClass 9) gets the hub bring-up path (U1); anything else is checked for a boot
    // HID interface.
    if device_class == kumo_hid::USB_CLASS_HUB {
        let hub_int = kumo_hid::find_interrupt_in_endpoint(
            &config[..read_len as usize],
            kumo_hid::USB_CLASS_HUB,
        );
        configure_and_read_hub(
            io,
            layout,
            &mut *image,
            &mut *cmd,
            evt,
            &mut ep0_ring,
            &mut *ep0_seg,
            slot_id,
            page_va,
            page_phys,
            speed_raw,
            port_number,
            config_value,
            data_buf,
            data_phys,
            hub_int,
        );
        return;
    }

    match kumo_usb_storage::find_bulk_only_scsi_interface(&config[..read_len as usize]) {
        Ok(Some(storage)) => {
            log_storage_interface(storage);
            configure_and_probe_storage(
                io,
                layout,
                &mut *image,
                &mut *cmd,
                evt,
                &mut ep0_ring,
                &mut *ep0_seg,
                slot_id,
                page_va,
                page_phys,
                slot_context_words(0, speed_raw, 1, port_number, 0, 0),
                storage,
                data_buf,
            );
            return;
        }
        Err(_) => log(b"drv-xhci: malformed storage interface\n"),
        Ok(None) => {}
    }

    if role == RootRole::SuperSpeedSide {
        return log(b"drv-xhci: ss side device is neither hub nor storage; left alone\n");
    }
    let Some(plan) = HidPlan::from_config(&config[..read_len as usize]) else {
        return log(b"drv-xhci: no hid boot iface\n");
    };
    let Some(primary) = plan.primary() else {
        return log(b"drv-xhci: no hid boot iface\n");
    };
    log(b"drv-xhci: hid if=");
    log_hex(primary.interface_number as u64);
    log(b" proto=");
    log_hex(primary.protocol as u64);
    log(b" ep=");
    log_hex(primary.in_endpoint_address as u64);
    log(b" mps=");
    log_hex(primary.max_packet_size as u64);
    log(b" interval=");
    log_hex(primary.interval as u64);
    log(b"\n");
    if let Some(mouse) = plan.mouse.filter(|_| plan.keyboard.is_some()) {
        log(b"drv-xhci: hid mouse if=");
        log_hex(mouse.interface_number as u64);
        log(b" proto=");
        log_hex(mouse.protocol as u64);
        log(b" ep=");
        log_hex(mouse.in_endpoint_address as u64);
        log(b" mps=");
        log_hex(mouse.max_packet_size as u64);
        log(b" interval=");
        log_hex(mouse.interval as u64);
        log(b"\n");
    }
    let mut devices = [
        Some(KeyboardHandoff {
            slot_id,
            page_va,
            page_phys,
            speed_raw,
            slot_words: slot_context_words(0, speed_raw, 1, port_number, 0, 0),
            config_value,
            plan,
            ep0_ring,
        }),
        None,
    ];
    run_hid_devices(&mut devices, io, layout, image, cmd, evt);
}

/// J525: select a root-attached SCSI/BOT interface, identify its geometry, and discover a GPT ESP
/// through bounded READ(10)s. This remains a one-shot proof: no block service or filesystem exists.
#[allow(clippy::too_many_arguments)]
fn configure_and_probe_storage(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    cmd: &mut DmaTrbs,
    evt: &DmaTrbs,
    ep0_ring: &mut TransferRing,
    ep0_seg: &mut DmaTrbs,
    slot_id: u8,
    base_va: u64,
    enum_phys: u64,
    slot_words: [u32; 8],
    storage: MassStorageInterface,
    storage_buf: DmaBuf,
) {
    // J541: one probe per driver. A second mass-storage device would remap the fixed read window
    // and refill the one static snapshot mid-probe, so it is named and left alone instead.
    if STORAGE_PROBED.swap(true, Ordering::Relaxed) {
        return log(b"drv-xhci: storage probe already ran; device left alone\n");
    }
    log_storage_phase(b"set-config", b"begin");
    let set_config = [0x00, 0x09, storage.configuration_value, 0, 0, 0, 0, 0];
    match control_no_data(
        io, layout, image, evt, ep0_ring, ep0_seg, slot_id, set_config,
    ) {
        Some((1, _)) => log_storage_phase(b"set-config", b"ok"),
        Some((cc, _)) => return log_storage_cc(b"set-config", cc, 0),
        None => return log_storage_phase(b"set-config", b"timeout"),
    }

    // Configuration selection activates alternate setting zero. Select a nonzero BOT alternate
    // before configuring its endpoint contexts.
    if storage.alternate_setting != 0 {
        log_storage_phase(b"set-interface", b"begin");
        let set_interface = [
            0x01,
            0x0b,
            storage.alternate_setting,
            0,
            storage.interface_number,
            0,
            0,
            0,
        ];
        match control_no_data(
            io,
            layout,
            image,
            evt,
            ep0_ring,
            ep0_seg,
            slot_id,
            set_interface,
        ) {
            Some((1, _)) => log_storage_phase(b"set-interface", b"ok"),
            Some((cc, _)) => return log_storage_cc(b"set-interface", cc, 0),
            None => return log_storage_phase(b"set-interface", b"timeout"),
        }
    }

    let Some(out_dci) = endpoint_context_index(storage.bulk_out.endpoint_number(), false) else {
        return log(b"drv-xhci: storage invalid OUT DCI\n");
    };
    let Some(in_dci) = endpoint_context_index(storage.bulk_in.endpoint_number(), true) else {
        return log(b"drv-xhci: storage invalid IN DCI\n");
    };
    if out_dci == in_dci {
        return log(b"drv-xhci: storage duplicate DCI\n");
    }

    let bulk_out_phys = enum_phys + OFF_INT_RING as u64;
    let bulk_in_phys = enum_phys + OFF_REPORT as u64;
    let out_seg = &mut unsafe {
        DmaTrbs::new(
            (base_va as usize + OFF_INT_RING) as *mut Trb,
            BULK_RING_TRBS,
        )
    };
    let in_seg =
        &mut unsafe { DmaTrbs::new((base_va as usize + OFF_REPORT) as *mut Trb, BULK_RING_TRBS) };
    let Ok(mut out_ring) = TransferRing::new(&mut *out_seg, bulk_out_phys) else {
        return log(b"drv-xhci: storage OUT ring fail\n");
    };
    let Ok(mut in_ring) = TransferRing::new(&mut *in_seg, bulk_in_phys) else {
        return log(b"drv-xhci: storage IN ring fail\n");
    };

    let stride = ContextSize::Bytes64;
    let (Some(slot_off), Some(out_off), Some(in_off)) = (
        stride.input_offset(0),
        stride.input_offset(out_dci),
        stride.input_offset(in_dci),
    ) else {
        return log(b"drv-xhci: storage context offset fail\n");
    };
    // The enumeration page reserves [0x000, 0x400) for its input context. A high-numbered USB
    // endpoint would otherwise overwrite the controller-owned output context at 0x400. — KESTREL
    if out_off + 32 > OFF_OUTPUT_CTX || in_off + 32 > OFF_OUTPUT_CTX {
        return log(b"drv-xhci: storage endpoint DCI exceeds input window\n");
    }

    let mut icc = InputControlContext::default();
    if icc.add(0).is_err() || icc.add(out_dci).is_err() || icc.add(in_dci).is_err() {
        return log(b"drv-xhci: storage input flags fail\n");
    }
    let mut slot = slot_words;
    let context_entries = out_dci.max(in_dci);
    slot[0] = (slot[0] & !(0x1f << 27)) | ((context_entries as u32) << 27);
    let Ok(out_ctx) = EndpointContext::bulk_out(
        storage.bulk_out.max_packet_size,
        storage.bulk_out.max_burst,
        bulk_out_phys,
    ) else {
        return log(b"drv-xhci: storage OUT context fail\n");
    };
    let Ok(in_ctx) = EndpointContext::bulk_in(
        storage.bulk_in.max_packet_size,
        storage.bulk_in.max_burst,
        bulk_in_phys,
    ) else {
        return log(b"drv-xhci: storage IN context fail\n");
    };

    write_ctx(base_va + OFF_INPUT_CTX as u64, icc.words());
    write_ctx(base_va + OFF_INPUT_CTX as u64 + slot_off as u64, slot);
    write_ctx(
        base_va + OFF_INPUT_CTX as u64 + out_off as u64,
        out_ctx.words(),
    );
    write_ctx(
        base_va + OFF_INPUT_CTX as u64 + in_off as u64,
        in_ctx.words(),
    );
    dsb();
    log_storage_phase(b"configure-endpoints", b"begin");
    let input_ctx_phys = enum_phys + OFF_INPUT_CTX as u64;
    let Ok(configure) = Trb::configure_endpoint(input_ctx_phys, slot_id) else {
        return log(b"drv-xhci: storage configure TRB fail\n");
    };
    match submit_command(io, layout, image, cmd, evt, configure) {
        Some((1, _, _)) => log_storage_phase(b"configure-endpoints", b"ok"),
        Some((cc, _, _)) => return log_storage_cc(b"configure-endpoints", cc, 0),
        None => return log_storage_phase(b"configure-endpoints", b"timeout"),
    }

    if storage_buf.len() < DATA_BUF_LEN {
        return log(b"drv-xhci: storage buffer too short\n");
    }
    let (Some(cbw_buf), Some(data_buf), Some(csw_buf)) = (
        storage_buf.window(BOT_CBW_OFFSET, COMMAND_BLOCK_WRAPPER_BYTES),
        storage_buf.window(BOT_DATA_OFFSET, BOT_DATA_LEN),
        storage_buf.window(BOT_DATA_OFFSET + BOT_DATA_LEN, COMMAND_STATUS_WRAPPER_BYTES),
    ) else {
        return log(b"drv-xhci: storage buffer layout\n");
    };
    let cbw_phys = enum_phys + OFF_DATA as u64 + BOT_CBW_OFFSET as u64;
    let data_phys = enum_phys + OFF_DATA as u64 + BOT_DATA_OFFSET as u64;
    let csw_phys = enum_phys + OFF_DATA as u64 + BOT_CSW_OFFSET as u64;

    let inquiry = ScsiCommand::inquiry(STANDARD_INQUIRY_BYTES);
    let Some(inquiry_len) = bot_command(
        io,
        layout,
        image,
        evt,
        &mut out_ring,
        out_seg,
        out_dci,
        &mut in_ring,
        in_seg,
        in_dci,
        slot_id,
        cbw_buf,
        cbw_phys,
        data_buf,
        data_phys,
        csw_buf,
        csw_phys,
        BOT_TAG_INQUIRY,
        &inquiry,
        b"inquiry",
    ) else {
        return;
    };
    let mut reply = [0u8; BOT_DATA_LEN];
    data_buf.read_into(0, &mut reply[..inquiry_len]);
    let Ok(inquiry_data) = StandardInquiryData::decode(&reply[..inquiry_len]) else {
        return log(b"drv-xhci: storage inquiry decode fail\n");
    };
    log(b"drv-xhci: storage inquiry type=");
    log_hex(inquiry_data.peripheral_device_type as u64);
    log(b" removable=");
    log_hex(inquiry_data.removable_media as u64);
    log(b" vendor=");
    log_ascii(&inquiry_data.vendor_identification);
    log(b" product=");
    log_ascii(&inquiry_data.product_identification);
    log(b"\n");

    let capacity_command = ScsiCommand::read_capacity_10();
    let Some(capacity_len) = bot_command(
        io,
        layout,
        image,
        evt,
        &mut out_ring,
        out_seg,
        out_dci,
        &mut in_ring,
        in_seg,
        in_dci,
        slot_id,
        cbw_buf,
        cbw_phys,
        data_buf,
        data_phys,
        csw_buf,
        csw_phys,
        BOT_TAG_READ_CAPACITY,
        &capacity_command,
        b"read-capacity",
    ) else {
        return;
    };
    data_buf.read_into(0, &mut reply[..capacity_len]);
    let Ok(capacity) = MediaCapacity::decode_read_capacity_10(&reply[..capacity_len]) else {
        return log(b"drv-xhci: storage capacity decode fail\n");
    };
    log(b"drv-xhci: storage capacity blocks=");
    log_hex(capacity.block_count());
    log(b" block-size=");
    log_hex(capacity.logical_block_size as u64);
    log(b" bytes=");
    log_hex(capacity.byte_length());
    log(b"\n");
    let product = &inquiry_data.product_identification;
    let product_len = product
        .iter()
        .rposition(|&b| b != b' ' && b != 0)
        .map_or(0, |end| end + 1);
    let mut digits = [0u8; 20];
    notice(&[
        b"usb: storage \"",
        &product[..product_len],
        b"\" ",
        decimal(capacity.byte_length() / 1_000_000_000, &mut digits),
        b" GB\n",
    ]);

    let adapter = match Read10BlockAdapter::new(capacity, STORAGE_DATA_LEN as usize) {
        Ok(adapter) => adapter,
        Err(_) => return log(b"drv-xhci: storage block geometry exceeds DMA window\n"),
    };
    let (read_raw, read_allocation_phys) = vmo_create_contiguous(STORAGE_ALLOCATION_LEN);
    if read_raw == u64::MAX || read_allocation_phys == 0 {
        return log(b"drv-xhci: storage read vmo fail\n");
    }
    if vmar_map(
        Handle(0),
        Handle(read_raw as u32),
        0,
        STORAGE_DATA_VA,
        STORAGE_ALLOCATION_LEN,
        (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
    ) != 0
    {
        return log(b"drv-xhci: storage read map fail\n");
    }
    let boundary_remaining = XHCI_DATA_BOUNDARY - (read_allocation_phys & (XHCI_DATA_BOUNDARY - 1));
    let read_offset = if boundary_remaining >= STORAGE_DATA_LEN {
        0
    } else {
        boundary_remaining
    };
    let Some(read_end) = read_offset.checked_add(STORAGE_DATA_LEN) else {
        return log(b"drv-xhci: storage read window overflow\n");
    };
    if read_end > STORAGE_ALLOCATION_LEN {
        return log(b"drv-xhci: storage read window does not fit backing\n");
    }
    let read_phys = read_allocation_phys + read_offset;
    // SAFETY: mapped above for the driver's lifetime and only reached through `DmaBuf`.
    let read_window = unsafe {
        DmaBuf::new(
            (STORAGE_DATA_VA + read_offset) as usize as *mut u8,
            read_phys,
            STORAGE_DATA_LEN as usize,
        )
    };
    log(b"drv-xhci: storage read-window backing-phys=");
    log_hex(read_allocation_phys);
    log(b" data-phys=");
    log_hex(read_phys);
    log(b" bytes=");
    log_hex(STORAGE_DATA_LEN);
    log(b"\n");

    let mut disk = BotDisk {
        io,
        layout,
        image,
        evt,
        slot_id,
        out_ring,
        out_seg,
        out_dci,
        in_ring,
        in_seg,
        in_dci,
        cbw_buf,
        cbw_phys,
        csw_buf,
        csw_phys,
        adapter,
        read_window,
        read_phys,
        blocks_read: 0,
    };
    let Some(lba0) = disk.read_blocks(0, 1, BOT_TAG_READ_LBA0, b"read10-lba0") else {
        return;
    };
    log(b"drv-xhci: storage lba0 prefix=");
    for byte in &lba0[..lba0.len().min(16)] {
        log(b" ");
        log_hex(*byte as u64);
    }
    if lba0.len() >= 512 {
        log(b" tail510=");
        log_hex(lba0[510] as u64);
        log(b" tail511=");
        log_hex(lba0[511] as u64);
    }
    log(b"\n");

    let logical_block_size = capacity.logical_block_size as usize;
    let media_blocks = capacity.block_count();
    let (volume, first_lba, block_count): (&[u8], u64, u64) =
        match validate_protective_mbr(lba0, logical_block_size, media_blocks) {
            Ok(()) => {
                log_storage_phase(b"gpt-mbr", b"ok");
                let Some((kind, partition)) =
                    find_gpt_volume(&mut disk, logical_block_size, media_blocks)
                else {
                    return;
                };
                (kind, partition.first_lba, partition.block_count())
            }
            // J542: a stick formatted elsewhere carries a classic MBR, not a GPT: CRUX's did at
            // J541. Its FAT32 record is judged the same way as a GPT's ESP. — TIMBERDOODLE
            Err(GptError::BadProtectiveMbr) => {
                log_storage_phase(b"gpt-mbr", b"classic");
                let Some(partition) = find_mbr_fat32(lba0, logical_block_size, media_blocks) else {
                    return;
                };
                (b"mbr", partition.first_lba, partition.block_count)
            }
            Err(error) => return log_gpt_error(b"mbr", error),
        };
    if probe_fat32_volume(
        &mut disk,
        volume,
        first_lba,
        block_count,
        logical_block_size,
    ) {
        keep_volume(&mut disk, base_va, volume, first_lba, block_count);
    }
}

/// A configured Bulk-Only Transport interface and its READ(10) window: everything one block read
/// needs once INQUIRY and READ CAPACITY have run (J542).
struct BotDisk<'a> {
    io: &'a mut Mmio,
    layout: RegisterLayout,
    image: &'a mut NoOpCommandImage,
    evt: &'a DmaTrbs,
    slot_id: u8,
    out_ring: TransferRing,
    out_seg: &'a mut DmaTrbs,
    out_dci: u8,
    in_ring: TransferRing,
    in_seg: &'a mut DmaTrbs,
    in_dci: u8,
    cbw_buf: DmaBuf,
    cbw_phys: u64,
    csw_buf: DmaBuf,
    csw_phys: u64,
    adapter: Read10BlockAdapter,
    read_window: DmaBuf,
    read_phys: u64,
    /// Device blocks read so far.
    blocks_read: u64,
}

impl BotDisk<'_> {
    /// Reuse the read window's checked geometry and volatile stores for one sector OUT.
    /// No new DMA reach or buffer ownership is introduced. — KESTREL
    fn write_block(&mut self, lba: u64, data: &[u8]) -> bool {
        if data.len() != SECTOR_SIZE || self.adapter.plan_read(lba, 1).is_err() {
            return false;
        }
        let Ok(command) = ScsiCommand::write_10(lba as u32, 1, SECTOR_SIZE as u32) else {
            return false;
        };
        self.read_window.write_from(0, data);
        self.write_or_flush(&command, BOT_TAG_WRITE_VOLUME, b"write10-volume") == Some(SECTOR_SIZE)
    }

    fn flush(&mut self) -> bool {
        self.write_or_flush(
            &ScsiCommand::synchronize_cache_10(),
            BOT_TAG_SYNC,
            b"sync-cache",
        ) == Some(0)
    }

    fn write_or_flush(&mut self, command: &ScsiCommand, tag: u32, name: &[u8]) -> Option<usize> {
        bot_command(
            self.io,
            self.layout,
            self.image,
            self.evt,
            &mut self.out_ring,
            self.out_seg,
            self.out_dci,
            &mut self.in_ring,
            self.in_seg,
            self.in_dci,
            self.slot_id,
            self.cbw_buf,
            self.cbw_phys,
            self.read_window,
            self.read_phys,
            self.csw_buf,
            self.csw_phys,
            tag,
            command,
            name,
        )
    }

    /// READ(10) `count` blocks from `lba`. Returns exactly the bytes that arrived, copied out of
    /// device memory by [`storage_snapshot`], so the slice is valid only until the next read.
    fn read_blocks(
        &mut self,
        lba: u64,
        count: u16,
        tag: u32,
        name: &[u8],
    ) -> Option<&'static [u8]> {
        let Ok(plan) = self.adapter.plan_read(lba, count) else {
            log_storage_phase(name, b"plan-fail");
            return None;
        };
        let Some(window) = self.read_window.window(0, plan.byte_length) else {
            log(b"drv-xhci: storage read plan exceeds window\n");
            return None;
        };
        let len = bot_command(
            self.io,
            self.layout,
            self.image,
            self.evt,
            &mut self.out_ring,
            self.out_seg,
            self.out_dci,
            &mut self.in_ring,
            self.in_seg,
            self.in_dci,
            self.slot_id,
            self.cbw_buf,
            self.cbw_phys,
            window,
            self.read_phys,
            self.csw_buf,
            self.csw_phys,
            tag,
            &plan.command,
            name,
        )?;
        if len != plan.byte_length {
            log_storage_phase(name, b"short");
            return None;
        }
        self.blocks_read += count as u64;
        Some(storage_snapshot(window, len))
    }
}

/// The stick as `kumo-fatfs` sees it: one READ(10) per 512-byte block (J543). The probe has
/// already refused any other block size through `Bpb::fits_partition`.
impl BlockReader for BotDisk<'_> {
    fn read_block(&mut self, lba: u64, buf: &mut [u8; SECTOR_SIZE]) -> bool {
        match self.read_blocks(lba, 1, BOT_TAG_READ_VOLUME, b"read10-volume") {
            Some(block) if block.len() >= SECTOR_SIZE => {
                buf.copy_from_slice(&block[..SECTOR_SIZE]);
                true
            }
            _ => false,
        }
    }
}

/// J525/J543: the primary GPT behind a protective MBR (header at LBA1, then its entry array), and
/// the volume a removable medium keeps its data on: its Basic Data partition, else its ESP.
/// Returns that volume and which kind it is (`data` or `esp`).
fn find_gpt_volume(
    disk: &mut BotDisk,
    logical_block_size: usize,
    media_blocks: u64,
) -> Option<(&'static [u8], Partition)> {
    let header_block = disk.read_blocks(1, 1, BOT_TAG_READ_GPT_HEADER, b"read10-gpt-header")?;
    let header = match GptHeader::parse_primary(header_block, logical_block_size, media_blocks) {
        Ok(header) => header,
        Err(error) => {
            log_gpt_error(b"header", error);
            return None;
        }
    };
    log_storage_phase(b"gpt-header", b"ok");
    log(b"drv-xhci: storage gpt disk-guid=");
    log_guid(header.disk_guid);
    log(b" entries-lba=");
    log_hex(header.partition_entry_lba);
    log(b" entries-bytes=");
    log_hex(header.partition_entry_array_byte_len() as u64);
    log(b"\n");

    let Ok(entry_block_count) = u16::try_from(header.partition_entry_array_block_count()) else {
        log(b"drv-xhci: storage GPT table exceeds READ(10) count\n");
        return None;
    };
    let entries = disk.read_blocks(
        header.partition_entry_lba,
        entry_block_count,
        BOT_TAG_READ_GPT_ENTRIES,
        b"read10-gpt-entries",
    )?;
    let table = match header.parse_partition_table(entries) {
        Ok(table) => table,
        Err(error) => {
            log_gpt_error(b"entries", error);
            return None;
        }
    };
    log_storage_phase(b"gpt-entries", b"ok");
    for partition in table.partitions() {
        log(b"drv-xhci: storage gpt part=");
        log_hex(partition.index as u64);
        log(b" type=");
        log_guid(partition.type_guid);
        log(b" first-lba=");
        log_hex(partition.first_lba);
        log(b" blocks=");
        log_hex(partition.block_count());
        log(b"\n");
    }
    let Some(volume) = table.data_volume() else {
        log_storage_phase(b"gpt-volume", b"not-found");
        return None;
    };
    let kind: &'static [u8] = if volume.type_guid == EFI_SYSTEM_PARTITION_TYPE_GUID {
        b"esp"
    } else {
        b"data"
    };
    log(b"drv-xhci: storage ");
    log(kind);
    log(b" index=");
    log_hex(volume.index as u64);
    log(b" guid=");
    log_guid(volume.unique_guid);
    log(b" first-lba=");
    log_hex(volume.first_lba);
    log(b" last-lba=");
    log_hex(volume.last_lba);
    log(b" blocks=");
    log_hex(volume.block_count());
    log(b"\n");
    Some((kind, volume))
}

/// J542: the classic partition table in LBA0, and the first record filed under a FAT32 type.
fn find_mbr_fat32(
    lba0: &[u8],
    logical_block_size: usize,
    media_blocks: u64,
) -> Option<MbrPartition> {
    let table = match MbrPartitionTable::parse(lba0, logical_block_size, media_blocks) {
        Ok(table) => table,
        Err(error) => {
            log_mbr_error(error);
            return None;
        }
    };
    log_storage_phase(b"mbr-table", b"ok");
    for partition in table.partitions() {
        log(b"drv-xhci: storage mbr part=");
        log_hex(partition.index as u64);
        log(b" type=");
        log_hex(partition.partition_type as u64);
        log(b" boot=");
        log_hex(partition.bootable as u64);
        log(b" first-lba=");
        log_hex(partition.first_lba);
        log(b" blocks=");
        log_hex(partition.block_count);
        log(b"\n");
    }
    let partition = table.fat32_partition();
    if partition.is_none() {
        log_storage_phase(b"mbr-fat32", b"not-found");
    }
    partition
}

/// J537/J542: a FAT32 volume's first block is its boot sector. Read it and judge it against the
/// partition that holds it; `volume` says which kind (`esp`, `data` or `mbr`). True when the
/// volume may be mounted.
fn probe_fat32_volume(
    disk: &mut BotDisk,
    volume: &[u8],
    first_lba: u64,
    block_count: u64,
    logical_block_size: usize,
) -> bool {
    let Some(boot) = disk.read_blocks(first_lba, 1, BOT_TAG_READ_FAT_BOOT, b"read10-fat-boot")
    else {
        return false;
    };
    let bpb = match Bpb::parse(boot) {
        Ok(bpb) => bpb,
        Err(error) => {
            log_fat_error(volume, error);
            return false;
        }
    };
    if let Err(error) = bpb.fits_partition(block_count, logical_block_size) {
        log_fit_error(volume, error);
        return false;
    }
    log_storage_phase_pair(volume, b"fat32", b"ok");
    log(b"drv-xhci: storage ");
    log(volume);
    log(b" fat32 serial=");
    log_hex(bpb.volume_serial as u64);
    log(b" sectors=");
    log_hex(bpb.total_sectors as u64);
    log(b" per-cluster=");
    log_hex(bpb.sectors_per_cluster as u64);
    log(b" reserved=");
    log_hex(bpb.reserved_sectors as u64);
    log(b" fats=");
    log_hex(bpb.num_fats as u64);
    log(b"x");
    log_hex(bpb.sectors_per_fat as u64);
    log(b" root-cluster=");
    log_hex(bpb.root_cluster as u64);
    log(b"\n");
    log_storage_phase(b"probe", b"complete");
    true
}

/// J545: mount the judged volume through the partition-relative reader, read its label, and
/// keep the stick's pipes so the resident loop can serve the volume. The volume is offered to
/// Sora only once that loop runs ([`offer_storage`]), so no client is ever handed a driver that
/// will not answer.
fn keep_volume(disk: &mut BotDisk, page_va: u64, volume: &[u8], first_lba: u64, block_count: u64) {
    // The probe traced every BOT phase. The volume reads are many more, so from here on only a
    // failure or an unexpected completion code is logged. — TIMBERDOODLE
    STORAGE_TRACE.store(false, Ordering::Relaxed);
    log_storage_phase(b"trace", b"quiet");
    let mut sectors = PartitionReader::new(&mut *disk, first_lba, block_count);
    let fat = match FatVolume::mount(&mut sectors) {
        Ok(fat) => fat,
        Err(error) => return log_fat_error(volume, error),
    };
    let label = fat.volume_label(&mut sectors);
    let ready = MediaReady {
        block_count,
        serial: fat.bpb().volume_serial,
        label,
        read_only: false,
    };
    log(b"drv-xhci: storage ");
    log(volume);
    log(b" mount ok label=");
    match label {
        Some(label) => log_ascii(label.trim_ascii_end()),
        None => log(b"(none)"),
    }
    log(b" serial=");
    log_hex(ready.serial as u64);
    log(b"\n");
    *storage_service() = Some(StorageService::keep(disk, page_va, first_lba, ready));
}

/// The stick's Bulk-Only pipes and its volume, kept after the probe so the resident loop can
/// serve the volume to block clients (J545). It is all `Copy` state: the bulk rings' segments
/// are rebuilt as `DmaTrbs` views over the device page for each use, and the rings' positions
/// are written back afterwards.
#[derive(Clone, Copy)]
struct StorageService {
    slot_id: u8,
    page_va: u64,
    out_ring: TransferRing,
    out_dci: u8,
    in_ring: TransferRing,
    in_dci: u8,
    cbw_buf: DmaBuf,
    cbw_phys: u64,
    csw_buf: DmaBuf,
    csw_phys: u64,
    adapter: Read10BlockAdapter,
    read_window: DmaBuf,
    read_phys: u64,
    /// Device block of the volume's first sector: a client's LBA 0.
    first_lba: u64,
    ready: MediaReady,
    clients: [BlockClient; MAX_BLOCK_CLIENTS],
}

/// One block client's channel, its koid for port wakes, and the blocks served to it.
#[derive(Clone, Copy, Default)]
struct BlockClient {
    handle: u32,
    koid: u64,
    access: BlockAccess,
    served: u32,
    writes: u32,
    flushes: u32,
}

impl StorageService {
    fn keep(disk: &BotDisk, page_va: u64, first_lba: u64, ready: MediaReady) -> Self {
        Self {
            slot_id: disk.slot_id,
            page_va,
            out_ring: disk.out_ring,
            out_dci: disk.out_dci,
            in_ring: disk.in_ring,
            in_dci: disk.in_dci,
            cbw_buf: disk.cbw_buf,
            cbw_phys: disk.cbw_phys,
            csw_buf: disk.csw_buf,
            csw_phys: disk.csw_phys,
            adapter: disk.adapter,
            read_window: disk.read_window,
            read_phys: disk.read_phys,
            first_lba,
            ready,
            clients: [BlockClient::default(); MAX_BLOCK_CLIENTS],
        }
    }

    /// Run `f` over a `BotDisk` rebuilt from these pipes, then keep the rings' new positions.
    fn with_disk<T>(
        &mut self,
        io: &mut Mmio,
        layout: RegisterLayout,
        image: &mut NoOpCommandImage,
        evt: &DmaTrbs,
        f: impl FnOnce(&mut BotDisk) -> T,
    ) -> T {
        // SAFETY: the stick's device page stays mapped for the driver's lifetime, and these are
        // the same ring segments `configure_and_probe_storage` built over it.
        let out_seg = &mut unsafe {
            DmaTrbs::new(
                (self.page_va as usize + OFF_INT_RING) as *mut Trb,
                BULK_RING_TRBS,
            )
        };
        let in_seg = &mut unsafe {
            DmaTrbs::new(
                (self.page_va as usize + OFF_REPORT) as *mut Trb,
                BULK_RING_TRBS,
            )
        };
        let mut disk = BotDisk {
            io,
            layout,
            image,
            evt,
            slot_id: self.slot_id,
            out_ring: self.out_ring,
            out_seg,
            out_dci: self.out_dci,
            in_ring: self.in_ring,
            in_seg,
            in_dci: self.in_dci,
            cbw_buf: self.cbw_buf,
            cbw_phys: self.cbw_phys,
            csw_buf: self.csw_buf,
            csw_phys: self.csw_phys,
            adapter: self.adapter,
            read_window: self.read_window,
            read_phys: self.read_phys,
            blocks_read: 0,
        };
        let result = f(&mut disk);
        self.out_ring = disk.out_ring;
        self.in_ring = disk.in_ring;
        result
    }
}

/// J545: offer the kept volume to Sora and watch the storage control channel on the resident
/// loop's `port`. Called only once that loop is about to run. Returns the control channel's
/// koid when the volume was offered.
fn offer_storage(port: Handle) -> Option<u64> {
    let service = storage_service().as_mut()?;
    let raw = STORAGE_CONTROL.load(Ordering::Relaxed);
    if raw == 0 {
        log(b"drv-xhci: storage kept, but no control channel to offer it on\n");
        return None;
    }
    let control = Handle(raw);
    let koid = handle_koid(control);
    if koid == u64::MAX || port_bind(port, control) != 0 {
        log(b"drv-xhci: storage control bind fail\n");
        return None;
    }
    // From here on a block read can pop the keyboard's completion: keep it for the keyboard.
    STASH_ENABLED.store(true, Ordering::Relaxed);
    let ready = service.ready.encode();
    if channel_write(control, ready.as_ptr(), ready.len()) != 0 {
        log(b"drv-xhci: storage offer fail\n");
        return None;
    }
    log(b"drv-xhci: storage offered blocks=");
    log_hex(service.ready.block_count);
    log(b" read-write\n");
    Some(koid)
}

/// J545: one resident-loop wake that is not the controller's interrupt: a new client from the
/// control channel, or a block request from a client. Returns `false` when `source` is none of
/// them, and the caller goes back to waiting for the controller as before.
fn serve_storage_wake(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    evt: &DmaTrbs,
    port: Handle,
    source: u64,
    control_koid: Option<u64>,
) -> bool {
    let Some(service) = storage_service().as_mut() else {
        return false;
    };
    if Some(source) == control_koid {
        accept_block_client(service, port);
        return true;
    }
    let Some(index) = service
        .clients
        .iter()
        .position(|client| client.handle != 0 && client.koid == source)
    else {
        return false;
    };
    serve_block_client(service, index, io, layout, image, evt, port);
    true
}

/// Bind the transferred block channel with the authority in Sora's connect request.
fn accept_block_client(service: &mut StorageService, port: Handle) {
    let control = Handle(STORAGE_CONTROL.load(Ordering::Relaxed));
    // An extra byte makes an oversized control frame fail decoding instead of being truncated
    // to a valid write grant. — KESTREL
    let mut tag = [0u8; CONTROL_CONNECT_LEN + 1];
    let (len, raw) = channel_read_with_handle(control, tag.as_mut_ptr(), tag.len());
    if len == 0 {
        // Sora closed its end: no client will come again, so stop watching it.
        let _ = port_unbind(port, control);
        return log(b"drv-xhci: storage control closed\n");
    }
    if raw == 0 || raw == u64::MAX {
        return log(b"drv-xhci: storage control message without a channel\n");
    }
    let client = Handle(raw as u32);
    let Some(access) = decode_connect(&tag[..len.min(tag.len())]) else {
        let _ = handle_close(client);
        return log(b"drv-xhci: storage control unknown message\n");
    };
    let Some(slot) = service.clients.iter().position(|client| client.handle == 0) else {
        let _ = handle_close(client);
        return log(b"drv-xhci: block clients full; client refused\n");
    };
    let koid = handle_koid(client);
    if koid == u64::MAX || port_bind(port, client) != 0 {
        let _ = handle_close(client);
        return log(b"drv-xhci: block client bind fail\n");
    }
    service.clients[slot] = BlockClient {
        handle: client.0,
        koid,
        access,
        served: 0,
        writes: 0,
        flushes: 0,
    };
    log(b"drv-xhci: block client connected slot=");
    log_hex(slot as u64);
    log(if access == BlockAccess::ReadOnly {
        b" read-only\n"
    } else {
        b" read-write\n"
    });
}

/// Answer one request from client `index` in the `drv-blk` protocol. LBAs are the volume's: a
/// client can name no block outside it. Reads and writes go one block at a time;
/// a flush completes only after SYNCHRONIZE CACHE's successful CSW. — KESTREL
#[allow(clippy::too_many_arguments)]
fn serve_block_client(
    service: &mut StorageService,
    index: usize,
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    evt: &DmaTrbs,
    port: Handle,
) {
    let client = Handle(service.clients[index].handle);
    // One extra byte exposes oversized frames instead of accepting their truncated prefix.
    // — KESTREL
    let mut frame = [0u8; BLOCK_REQUEST_LEN + SECTOR_SIZE + 1];
    let n = channel_read(client, frame.as_mut_ptr(), frame.len());
    if n == 0 || n == u64::MAX {
        // The client's `fatfs` exited: drop the channel and free the slot.
        let _ = port_unbind(port, client);
        let _ = handle_close(client);
        log(b"drv-xhci: block client closed slot=");
        log_hex(index as u64);
        log(b" served=");
        log_hex(service.clients[index].served as u64);
        log(b" writes=");
        log_hex(service.clients[index].writes as u64);
        log(b" flushes=");
        log_hex(service.clients[index].flushes as u64);
        log(b"\n");
        service.clients[index] = BlockClient::default();
        return;
    }
    let mut reply = [0u8; 1 + SECTOR_SIZE];
    let mut reply_len = 1;
    reply[0] = match storage_request(
        &frame[..n as usize],
        service.first_lba,
        service.ready.block_count,
        service.ready.read_only || service.clients[index].access == BlockAccess::ReadOnly,
    ) {
        Ok(StorageRequest::Read(lba)) => {
            let read = service.with_disk(io, layout, image, evt, |disk| {
                match disk.read_blocks(lba, 1, BOT_TAG_READ_VOLUME, b"read10-volume") {
                    Some(block) if block.len() >= SECTOR_SIZE => {
                        reply[1..].copy_from_slice(&block[..SECTOR_SIZE]);
                        true
                    }
                    _ => false,
                }
            });
            if read {
                reply_len = reply.len();
                service.clients[index].served += 1;
                STATUS_OK
            } else {
                STATUS_IO_ERROR
            }
        }
        Ok(StorageRequest::Write(lba, data)) => {
            if service.with_disk(io, layout, image, evt, |disk| disk.write_block(lba, data)) {
                service.clients[index].served += 1;
                service.clients[index].writes += 1;
                STATUS_OK
            } else {
                STATUS_IO_ERROR
            }
        }
        Ok(StorageRequest::Flush) => {
            if service.with_disk(io, layout, image, evt, |disk| disk.flush()) {
                service.clients[index].flushes += 1;
                STATUS_OK
            } else {
                STATUS_IO_ERROR
            }
        }
        Err(status) => status,
    };
    let _ = channel_write(client, reply.as_ptr(), reply_len);
}

fn log_mbr_error(error: MbrError) {
    let name = match error {
        MbrError::BufferTooShort => b"buffer-too-short" as &[u8],
        MbrError::InvalidLogicalBlockSize => b"invalid-logical-block-size",
        MbrError::InvalidMediaSize => b"invalid-media-size",
        MbrError::BadSignature => b"bad-signature",
        MbrError::BadBootIndicator => b"bad-boot-indicator",
        MbrError::ProtectiveRecord => b"protective-record",
        MbrError::BadPartitionEntry => b"bad-partition-entry",
        MbrError::OverlappingPartitions => b"overlapping-partitions",
    };
    log(b"drv-xhci: storage mbr error=");
    log(name);
    log(b"\n");
}

fn log_fat_error(volume: &[u8], error: FatError) {
    let name = match error {
        FatError::TooShort => b"too-short" as &[u8],
        FatError::BadSignature => b"bad-signature",
        FatError::NotFat32 => b"not-fat32",
        FatError::BadGeometry => b"bad-geometry",
        FatError::ReadFailed => b"read-failed",
    };
    log(b"drv-xhci: storage ");
    log(volume);
    log(b" fat32 error=");
    log(name);
    log(b"\n");
}

fn log_fit_error(volume: &[u8], error: PartitionFitError) {
    let name = match error {
        PartitionFitError::BlockSize => b"block-size" as &[u8],
        PartitionFitError::SectorSize => b"sector-size",
        PartitionFitError::VolumeTooLarge => b"volume-too-large",
        PartitionFitError::RegionsPastEnd => b"regions-past-end",
    };
    log(b"drv-xhci: storage ");
    log(volume);
    log(b" fat32 fit error=");
    log(name);
    log(b"\n");
}

/// Execute exactly one BOT transaction. The CBW, optional data TD, and CSW are submitted and
/// completed serially; there is never more than one outstanding TRB on either bulk ring.
#[allow(clippy::too_many_arguments)]
fn bot_command(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    evt: &DmaTrbs,
    out_ring: &mut TransferRing,
    out_seg: &mut DmaTrbs,
    out_dci: u8,
    in_ring: &mut TransferRing,
    in_seg: &mut DmaTrbs,
    in_dci: u8,
    slot_id: u8,
    cbw_buf: DmaBuf,
    cbw_phys: u64,
    data_buf: DmaBuf,
    data_phys: u64,
    csw_buf: DmaBuf,
    csw_phys: u64,
    tag: u32,
    command: &ScsiCommand,
    command_name: &[u8],
) -> Option<usize> {
    if command.transfer_length() as usize > data_buf.len() {
        return None;
    }
    let cbw = CommandBlockWrapper::for_scsi(tag, 0, command).ok()?;
    cbw_buf.write_from(0, cbw.as_bytes());
    dsb();
    let cbw_len = bulk_transfer(
        io,
        layout,
        image,
        evt,
        out_ring,
        out_seg,
        slot_id,
        out_dci,
        cbw_phys,
        COMMAND_BLOCK_WRAPPER_BYTES as u32,
        false,
        command_name,
        b"cbw",
    )?;
    if cbw_len != COMMAND_BLOCK_WRAPPER_BYTES as u32 {
        log(b"drv-xhci: storage short CBW\n");
        return None;
    }

    let expected = command.transfer_length();
    let transferred = match command.direction() {
        DataDirection::None => 0,
        DataDirection::FromDevice => {
            if expected as usize > data_buf.len() {
                log(b"drv-xhci: storage data buffer too short\n");
                return None;
            }
            data_buf.window(0, expected as usize)?.zero();
            dsb();
            bulk_transfer(
                io,
                layout,
                image,
                evt,
                in_ring,
                in_seg,
                slot_id,
                in_dci,
                data_phys,
                expected,
                true,
                command_name,
                b"data-in",
            )?
        }
        DataDirection::ToDevice => {
            dsb();
            bulk_transfer(
                io,
                layout,
                image,
                evt,
                out_ring,
                out_seg,
                slot_id,
                out_dci,
                data_phys,
                expected,
                false,
                command_name,
                b"data-out",
            )?
        }
    };

    csw_buf.zero();
    dsb();
    let csw_len = bulk_transfer(
        io,
        layout,
        image,
        evt,
        in_ring,
        in_seg,
        slot_id,
        in_dci,
        csw_phys,
        COMMAND_STATUS_WRAPPER_BYTES as u32,
        true,
        command_name,
        b"csw",
    )?;
    if csw_len != COMMAND_STATUS_WRAPPER_BYTES as u32 {
        log(b"drv-xhci: storage short CSW\n");
        return None;
    }
    let csw_bytes: [u8; COMMAND_STATUS_WRAPPER_BYTES] = csw_buf.read(0);
    let csw = match CommandStatusWrapper::decode(&csw_bytes, tag, expected) {
        Ok(csw) => csw,
        Err(_) => {
            log(b"drv-xhci: storage invalid CSW\n");
            return None;
        }
    };
    if csw.transferred_bytes != transferred {
        log(b"drv-xhci: storage residue mismatch xhci=");
        log_hex(transferred as u64);
        log(b" csw=");
        log_hex(csw.transferred_bytes as u64);
        log(b"\n");
        return None;
    }
    match csw.status {
        CommandStatus::Passed => Some(transferred as usize),
        CommandStatus::Failed => {
            log_storage_phase(command_name, b"command-failed");
            None
        }
        CommandStatus::PhaseError => {
            log_storage_phase(command_name, b"phase-error");
            None
        }
    }
}

/// Submit one single-TRB bulk TD and return its actual byte count. A Data-IN short packet is a
/// normal BOT termination, while CBW OUT must complete exactly.
#[allow(clippy::too_many_arguments)]
fn bulk_transfer(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    evt: &DmaTrbs,
    ring: &mut TransferRing,
    segment: &mut DmaTrbs,
    slot_id: u8,
    dci: u8,
    buffer_phys: u64,
    length: u32,
    direction_in: bool,
    command_name: &[u8],
    phase: &[u8],
) -> Option<u32> {
    let config = if direction_in {
        NormalTransfer::bulk_in(buffer_phys, length)
    } else {
        NormalTransfer::bulk_out(buffer_phys, length)
    };
    let trb = Trb::normal_transfer(config).ok()?;
    if ring.enqueue(segment, trb).is_err() {
        log_storage_phase(command_name, b"ring-full");
        return None;
    }
    dsb();
    let trace = STORAGE_TRACE.load(Ordering::Relaxed);
    if trace {
        log_storage_phase_pair(command_name, phase, b"begin");
    }
    ring_slot_doorbell(io, layout, slot_id, dci);
    let Some((cc, remaining, trb_iova)) = poll_transfer_event(io, layout, image, evt, slot_id)
    else {
        log_storage_phase_pair(command_name, phase, b"timeout");
        return None;
    };
    let _ = ring.reclaim_through(trb_iova);
    dsb();
    if trace || cc != 1 {
        log_storage_cc_pair(command_name, phase, cc, remaining);
    }
    if remaining > length {
        return None;
    }
    if direction_in {
        if cc != 1 && cc != 13 {
            return None;
        }
    } else if cc != 1 || remaining != 0 {
        return None;
    }
    Some(length - remaining)
}

fn log_storage_interface(storage: MassStorageInterface) {
    log(b"drv-xhci: storage if=");
    log_hex(storage.interface_number as u64);
    log(b" out=");
    log_hex(storage.bulk_out.address as u64);
    log(b" in=");
    log_hex(storage.bulk_in.address as u64);
    log(b" burst=");
    log_hex(storage.bulk_out.max_burst as u64);
    log(b"/");
    log_hex(storage.bulk_in.max_burst as u64);
    log(b"\n");
}

fn log_storage_phase(phase: &[u8], state: &[u8]) {
    log(b"drv-xhci: storage phase=");
    log(phase);
    log(b" ");
    log(state);
    log(b"\n");
}

fn log_storage_phase_pair(command: &[u8], phase: &[u8], state: &[u8]) {
    log(b"drv-xhci: storage phase=");
    log(command);
    log(b"-");
    log(phase);
    log(b" ");
    log(state);
    log(b"\n");
}

fn log_storage_cc(phase: &[u8], cc: u8, remaining: u32) {
    log(b"drv-xhci: storage phase=");
    log(phase);
    log(b" cc=");
    log_hex(cc as u64);
    log(b" rem=");
    log_hex(remaining as u64);
    log(b"\n");
}

fn log_storage_cc_pair(command: &[u8], phase: &[u8], cc: u8, remaining: u32) {
    log(b"drv-xhci: storage phase=");
    log(command);
    log(b"-");
    log(phase);
    log(b" cc=");
    log_hex(cc as u64);
    log(b" rem=");
    log_hex(remaining as u64);
    log(b"\n");
}

fn log_gpt_error(stage: &[u8], error: GptError) {
    let name = match error {
        GptError::BufferTooShort => b"buffer-too-short" as &[u8],
        GptError::InvalidLogicalBlockSize => b"invalid-logical-block-size",
        GptError::InvalidMediaSize => b"invalid-media-size",
        GptError::BadMbrSignature => b"bad-mbr-signature",
        GptError::BadProtectiveMbr => b"bad-protective-mbr",
        GptError::BadHeaderSignature => b"bad-header-signature",
        GptError::UnsupportedRevision => b"unsupported-revision",
        GptError::BadHeaderSize => b"bad-header-size",
        GptError::BadHeaderCrc => b"bad-header-crc",
        GptError::NonzeroReservedBytes => b"nonzero-reserved-bytes",
        GptError::BadHeaderLocation => b"bad-header-location",
        GptError::BadUsableRange => b"bad-usable-range",
        GptError::BadDiskGuid => b"bad-disk-guid",
        GptError::BadEntryGeometry => b"bad-entry-geometry",
        GptError::EntryArrayOutOfBounds => b"entry-array-out-of-bounds",
        GptError::EntryArrayTooShort => b"entry-array-too-short",
        GptError::BadEntryArrayCrc => b"bad-entry-array-crc",
        GptError::BadPartitionEntry => b"bad-partition-entry",
        GptError::OverlappingPartitions => b"overlapping-partitions",
    };
    log(b"drv-xhci: storage gpt ");
    log(stage);
    log(b" error=");
    log(name);
    log(b"\n");
}

/// Print a GPT's on-disk mixed-endian GUID bytes in canonical textual order.
fn log_guid(guid: Guid) {
    const ORDER: [usize; 16] = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
    for (position, byte_index) in ORDER.into_iter().enumerate() {
        if matches!(position, 4 | 6 | 8 | 10) {
            log(b"-");
        }
        log_hex_byte(guid.0[byte_index]);
    }
}

fn log_hex_byte(value: u8) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let bytes = [
        DIGITS[(value >> 4) as usize],
        DIGITS[(value & 0x0f) as usize],
    ];
    log(&bytes);
}

fn log_ascii(bytes: &[u8]) {
    let mut safe = [b'.'; 16];
    let length = bytes.len().min(safe.len());
    for (destination, source) in safe[..length].iter_mut().zip(bytes.iter()) {
        if source.is_ascii_graphic() || *source == b' ' {
            *destination = *source;
        }
    }
    log(&safe[..length]);
}

/// The boot HID interfaces a configuration publishes: a keyboard, a mouse, or both. A composite
/// receiver needs both endpoints added by one Configure Endpoint command, so the pair travels
/// together.
#[derive(Clone, Copy)]
struct HidPlan {
    keyboard: Option<kumo_hid::HidBootInterface>,
    mouse: Option<kumo_hid::HidBootInterface>,
}

impl HidPlan {
    fn from_config(config: &[u8]) -> Option<Self> {
        let (keyboard, mouse) = kumo_hid::find_hid_boot_pair(config);
        if keyboard.is_none() && mouse.is_none() {
            None
        } else {
            Some(Self { keyboard, mouse })
        }
    }

    /// The interface logged first and served alone when the pair cannot be configured: the
    /// keyboard when present, else the mouse.
    fn primary(self) -> Option<kumo_hid::HidBootInterface> {
        self.keyboard.or(self.mouse)
    }
}

/// One boot HID interface's report lane: its own ring, report buffer, decoders, and one
/// outstanding transfer. The keyboard keeps the historical OFF_INT_RING/OFF_REPORT regions; a
/// composite mouse takes the spare window above the input context. A lane names its device slot
/// and owns its report decoders, so one resident loop can serve several devices without their
/// key or button edges interfering.
struct HidLane {
    hid: kumo_hid::HidBootInterface,
    ring: TransferRing,
    ring_va: u64,
    report: DmaBuf,
    report_phys: u64,
    dci: u8,
    slot_id: u8,
    pending: bool,
    token: u64,
    armed_logged: bool,
    decoder: kumo_hid::Decoder,
    mouse_decoder: kumo_hid::MouseDecoder,
}

impl HidLane {
    /// Create the lane over its page regions. Both offsets are 64-byte aligned for the xHCI
    /// ring rules, and the lane reaches the page only through the DMA views.
    fn new(
        hid: kumo_hid::HidBootInterface,
        dci: u8,
        slot_id: u8,
        ring_off: usize,
        report_off: usize,
        base_va: u64,
        enum_phys: u64,
    ) -> Option<Self> {
        // SAFETY: both regions live inside this device's mapped page.
        let mut ring_seg =
            unsafe { DmaTrbs::new((base_va as usize + ring_off) as *mut Trb, INT_RING_TRBS) };
        let ring = TransferRing::new(&mut ring_seg, enum_phys + ring_off as u64).ok()?;
        // SAFETY: the report buffer is inside the same page, reached only through `DmaBuf`.
        let report = unsafe {
            DmaBuf::new(
                (base_va as usize + report_off) as *mut u8,
                enum_phys + report_off as u64,
                8,
            )
        };
        Some(Self {
            hid,
            ring,
            ring_va: base_va + ring_off as u64,
            report,
            report_phys: enum_phys + report_off as u64,
            dci,
            slot_id,
            pending: false,
            token: 0,
            armed_logged: false,
            decoder: kumo_hid::Decoder::new(),
            mouse_decoder: kumo_hid::MouseDecoder::new(),
        })
    }

    /// Arm this lane's one outstanding interrupt-IN transfer. Re-arming only after the previous
    /// event is reclaimed is the J425/J632 rule: a wake without a match used to leak a ring slot
    /// until enqueue failed and the driver exited.
    fn arm(&mut self, io: &mut Mmio, layout: RegisterLayout) {
        self.report.zero();
        dsb();
        let Ok(normal) = Trb::normal_transfer(NormalTransfer::interrupt_in(
            self.report_phys,
            self.hid.max_packet_size as u32,
        )) else {
            return log(b"drv-xhci: normal trb fail\n");
        };
        let mut seg = unsafe { DmaTrbs::new(self.ring_va as *mut Trb, INT_RING_TRBS) };
        match self.ring.enqueue(&mut seg, normal) {
            Err(_) => {
                // Never exit on a full ring: exiting quiesces the controller and the device
                // never comes back. Retry on the next interrupt.
                log(b"drv-xhci: int ring full; draining\n");
            }
            Ok(token) => {
                self.pending = true;
                self.token = token.iova;
                dsb();
                ring_slot_doorbell(io, layout, self.slot_id, self.dci);
                if !self.armed_logged {
                    log(b"drv-xhci: hid transfer armed dci=");
                    log_hex(self.dci as u64);
                    log(b" trb=");
                    log_hex(token.iova);
                    log(b"\n");
                    self.armed_logged = true;
                }
            }
        }
    }
}

/// Configure BOTH boot-HID endpoints of a composite device in one Configure Endpoint command.
/// A Configure Endpoint command replaces the device's endpoint set, so adding the mouse with a
/// second command would drop the keyboard. The keyboard's ring is OFF_INT_RING; the mouse's is
/// the spare window at OFF_HID2_RING. Refuses when the input context's highest context slot
/// would run into that window.
#[allow(clippy::too_many_arguments)]
fn configure_hid_endpoint_pair(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    cmd: &mut DmaTrbs,
    evt: &DmaTrbs,
    slot_id: u8,
    base_va: u64,
    enum_phys: u64,
    slot_words: [u32; 8],
    keyboard: kumo_hid::HidBootInterface,
    mouse: kumo_hid::HidBootInterface,
    speed_raw: u8,
) -> Option<(u8, u8)> {
    let keyboard_dci = endpoint_context_index(keyboard.in_endpoint_address & 0x0f, true)?;
    let mouse_dci = endpoint_context_index(mouse.in_endpoint_address & 0x0f, true)?;
    if keyboard_dci == mouse_dci {
        return None;
    }
    let max_dci = keyboard_dci.max(mouse_dci);
    let stride = ContextSize::Bytes64;
    if stride.input_offset(max_dci)? + stride.bytes() > OFF_HID2_RING {
        log(b"drv-xhci: composite hid dci=");
        log_hex(max_dci as u64);
        log(b" would overlap the second ring\n");
        return None;
    }
    let mut icc = InputControlContext::default();
    icc.add(0).ok()?; // slot (to raise Context Entries)
    icc.add(keyboard_dci).ok()?;
    icc.add(mouse_dci).ok()?;
    let mut slot = slot_words;
    slot[0] = (slot[0] & !(0x1f << 27)) | ((max_dci as u32) << 27);
    let keyboard_ctx = EndpointContext::interrupt_in(
        keyboard.max_packet_size,
        xhci_interval(speed_raw, keyboard.interval),
        enum_phys + OFF_INT_RING as u64,
        keyboard.max_packet_size.max(1),
    )
    .ok()?;
    let mouse_ctx = EndpointContext::interrupt_in(
        mouse.max_packet_size,
        xhci_interval(speed_raw, mouse.interval),
        enum_phys + OFF_HID2_RING as u64,
        mouse.max_packet_size.max(1),
    )
    .ok()?;
    write_ctx(base_va + OFF_INPUT_CTX as u64, icc.words());
    write_ctx(
        base_va + OFF_INPUT_CTX as u64 + stride.input_offset(0)? as u64,
        slot,
    );
    write_ctx(
        base_va + OFF_INPUT_CTX as u64 + stride.input_offset(keyboard_dci)? as u64,
        keyboard_ctx.words(),
    );
    write_ctx(
        base_va + OFF_INPUT_CTX as u64 + stride.input_offset(mouse_dci)? as u64,
        mouse_ctx.words(),
    );
    dsb();
    let configure = Trb::configure_endpoint(enum_phys + OFF_INPUT_CTX as u64, slot_id).ok()?;
    match submit_command(io, layout, image, cmd, evt, configure) {
        Some((1, _, _)) => Some((keyboard_dci, mouse_dci)),
        Some((cc, _, _)) => {
            log_cc(b"drv-xhci: config hid pair cc=", cc);
            None
        }
        None => {
            log(b"drv-xhci: config hid pair timeout\n");
            None
        }
    }
}

/// Bring a device's boot HID interface or interfaces online: SET_CONFIGURATION, Configure
/// Endpoint for each lane's interrupt-IN endpoint, and SET_PROTOCOL(boot), appending the device's
/// lanes to `lanes`. [`run_hid_lanes`] then serves every device's lanes from one resident loop.
#[allow(clippy::too_many_arguments)]
fn bring_up_hid_device(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    cmd: &mut DmaTrbs,
    evt: &DmaTrbs,
    ep0_ring: &mut TransferRing,
    ep0_seg: &mut DmaTrbs,
    slot_id: u8,
    base_va: u64,
    enum_phys: u64,
    speed_raw: u8,
    slot_words: [u32; 8],
    config_value: u8,
    plan: HidPlan,
    lanes: &mut [Option<HidLane>; MAX_HID_LANES],
    count: &mut usize,
) {
    // --- SET_CONFIGURATION(config_value) --- bmRequestType=0, bRequest=9, wValue=config, no data.
    let set_config = [0x00, 0x09, config_value, 0x00, 0x00, 0x00, 0x00, 0x00];
    match control_no_data(
        io,
        layout,
        image,
        evt,
        ep0_ring,
        &mut *ep0_seg,
        slot_id,
        set_config,
    ) {
        Some((1, _)) => log(b"drv-xhci: set config cc=0x1\n"),
        Some((cc, _)) => return log_cc(b"drv-xhci: set config cc=", cc),
        None => return log(b"drv-xhci: set config timeout\n"),
    }

    // --- Configure Endpoint for the device's boot HID lane(s) ---
    // A composite receiver publishes a keyboard and a mouse in one configuration; a Configure
    // Endpoint command replaces the endpoint set, so both endpoints must be added together.
    // When the pair cannot be configured, fall back to the primary interface alone. Lanes are
    // appended to the shared table; a device that fails keeps its lanes out of it.
    let first = *count;
    let mut paired = false;
    if let (Some(keyboard), Some(mouse)) = (plan.keyboard, plan.mouse) {
        if *count + 2 > lanes.len() {
            return log(b"drv-xhci: hid lane table full\n");
        }
        match configure_hid_endpoint_pair(
            io, layout, image, cmd, evt, slot_id, base_va, enum_phys, slot_words, keyboard, mouse,
            speed_raw,
        ) {
            Some((keyboard_dci, mouse_dci)) => {
                log(b"drv-xhci: config ep pair cc=0x1\n");
                let keyboard_lane = HidLane::new(
                    keyboard,
                    keyboard_dci,
                    slot_id,
                    OFF_INT_RING,
                    OFF_REPORT,
                    base_va,
                    enum_phys,
                );
                let mouse_lane = HidLane::new(
                    mouse,
                    mouse_dci,
                    slot_id,
                    OFF_HID2_RING,
                    OFF_HID2_REPORT,
                    base_va,
                    enum_phys,
                );
                match (keyboard_lane, mouse_lane) {
                    (Some(keyboard_lane), Some(mouse_lane)) => {
                        lanes[*count] = Some(keyboard_lane);
                        *count += 1;
                        lanes[*count] = Some(mouse_lane);
                        *count += 1;
                        paired = true;
                    }
                    _ => return log(b"drv-xhci: hid lane ring fail\n"),
                }
            }
            None => log(b"drv-xhci: composite hid refused; primary only\n"),
        }
    }
    if !paired {
        if *count >= lanes.len() {
            return log(b"drv-xhci: hid lane table full\n");
        }
        let Some(primary) = plan.primary() else {
            return log(b"drv-xhci: hid plan empty\n");
        };
        let Some(dci) = configure_interrupt_endpoint(
            io,
            layout,
            image,
            cmd,
            evt,
            slot_id,
            base_va,
            enum_phys,
            slot_words,
            primary.in_endpoint_address,
            primary.max_packet_size,
            primary.interval,
            speed_raw,
        ) else {
            return;
        };
        log(b"drv-xhci: config ep cc=0x1\n");
        let Some(lane) = HidLane::new(
            primary,
            dci,
            slot_id,
            OFF_INT_RING,
            OFF_REPORT,
            base_va,
            enum_phys,
        ) else {
            return log(b"drv-xhci: hid lane ring fail\n");
        };
        lanes[*count] = Some(lane);
        *count += 1;
    }

    // --- SET_PROTOCOL(boot) per new lane --- HID class request to the interface:
    // bmRequestType=0x21, bRequest=0x0B, wValue=0 (boot), wIndex=interface, no data. A device
    // that fails here drops its lanes rather than serving misparsed reports.
    for lane in lanes[first..*count].iter().flatten() {
        let set_proto = [
            0x21,
            0x0b,
            0x00,
            0x00,
            lane.hid.interface_number,
            0x00,
            0x00,
            0x00,
        ];
        match control_no_data(
            io,
            layout,
            image,
            evt,
            ep0_ring,
            &mut *ep0_seg,
            slot_id,
            set_proto,
        ) {
            Some((1, _)) => log(b"drv-xhci: set proto cc=0x1\n"),
            Some((cc, _)) => {
                *count = first;
                return log_cc(b"drv-xhci: set proto cc=", cc);
            }
            None => {
                *count = first;
                return log(b"drv-xhci: set proto timeout\n");
            }
        }
    }
}

/// Serve every lane in `lanes` from one resident loop. The controller grants one interrupt for
/// all its slots, so a keyboard and a composite receiver behind the same hub share a wake; each
/// completion is matched to its lane by TRB address. Bounded only when the controller has no IRQ.
fn run_hid_lanes(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    evt: &DmaTrbs,
    lanes: &mut [Option<HidLane>; MAX_HID_LANES],
) {
    let mut slots = [0u8; MAX_HID_LANES];
    let mut slot_count = 0usize;
    for lane in lanes.iter().flatten() {
        if !slots[..slot_count].contains(&lane.slot_id) {
            slots[slot_count] = lane.slot_id;
            slot_count += 1;
        }
    }

    // U4: IRQ-driven persistent path. A busy-poll at our priority (63) would starve Sora (64), so
    // if this controller was granted its IRQ, enable the controller interrupter and block on the
    // xHCI interrupt between reports — the driver stays resident and services the HID devices
    // forever.
    let irq = DEVICE_IRQ.load(Ordering::Relaxed);
    if irq != 0 {
        let raw = interrupt_create(Handle(RESOURCE_HANDLE.load(Ordering::Relaxed)), irq);
        if raw != 0 && raw != u64::MAX {
            let interrupt = Handle(raw as u32);
            // Wait via a PORT bound to the interrupt, not bare `interrupt_wait`. `signal_interrupt`
            // wakes both ways, but the port path (`signal_ports(koid, IRQ)` →
            // `wake_child_waiting_on_port`) is the one drv-i2c-hid proves keeps a resident child
            // serviced once the system goes idle at the shell prompt — which is exactly when a bare
            // interrupt wait stopped delivering here.
            let port_raw = port_create();
            let irq_port = if port_raw != 0 && port_raw != u64::MAX {
                let port = Handle(port_raw as u32);
                if port_bind(port, interrupt) == 0 {
                    Some(port)
                } else {
                    log(b"drv-xhci: irq port bind fail\n");
                    None
                }
            } else {
                log(b"drv-xhci: irq port create fail\n");
                None
            };
            // Arm the controller only now that the wait is bound. Enabling the interrupter
            // earlier is a race we lost on metal: enumeration leaves USBSTS.EINT latched, so the
            // controller asserts immediately; the kernel masks the SPI on that fire and only
            // unmasks on `InterruptComplete` — and with no port bound yet, nothing was waiting to
            // complete it. The line stayed masked forever and no keypress ever raised an
            // interrupt again (observed: driver armed, zero `IRQ dev` for the whole boot).
            // Clear the latched condition, enable, then complete once to release any mask a
            // pre-bind fire already applied.
            clear_interrupt_pending(io, layout);
            enable_interrupter(io, layout);
            interrupt_complete(interrupt);
            // J545: the loop below is resident, so a stick this driver kept can now be served on
            // the same port. Wakes are told apart by their source's koid.
            let interrupt_koid = handle_koid(interrupt);
            let storage_control_koid = irq_port.and_then(offer_storage);
            for lane in lanes.iter().flatten() {
                log(if lane.hid.is_mouse() {
                    b"drv-xhci: mouse irq-driven port=" as &[u8]
                } else {
                    b"drv-xhci: keyboard irq-driven port=" as &[u8]
                });
                log_hex(irq_port.is_some() as u64);
                if !lane.hid.is_mouse() {
                    log(b"; type at the shell");
                }
                log(b"\n");
                notice(&[if lane.hid.is_mouse() {
                    b"usb: mouse ready\n" as &[u8]
                } else {
                    b"usb: keyboard ready\n"
                }]);
            }
            // Exactly ONE transfer per lane outstanding at a time, re-armed only after its own
            // event is reclaimed: the previous shape enqueued a TRB on every iteration and
            // leaked a slot on every wake without a match, until enqueue failed and the driver
            // exited (J425/J632). Each wake drains up to HID_EVENTS_PER_WAKE completions so a
            // second lane's event cannot wait for an interrupt the controller already raised.
            loop {
                for lane in lanes.iter_mut().flatten() {
                    if !lane.pending {
                        lane.arm(io, layout);
                    }
                }
                // Block until the controller raises its interrupt (a report completed).
                match irq_port {
                    Some(port) => {
                        // J545: the port also wakes for the stick's control channel and its block
                        // clients. Serve those in place and keep waiting for the controller.
                        loop {
                            let source = port_wait(port);
                            if source == interrupt_koid
                                || !serve_storage_wake(
                                    io,
                                    layout,
                                    &mut *image,
                                    evt,
                                    port,
                                    source,
                                    storage_control_koid,
                                )
                            {
                                break;
                            }
                        }
                        interrupt_wait(interrupt);
                    }
                    None => {
                        interrupt_wait(interrupt);
                    }
                }
                clear_interrupt_pending(io, layout);
                // ERDP must be re-written on EVERY serviced interrupt, not only when a matching
                // transfer event is found: the write clears EHB (Event Handler Busy), and the
                // controller will not raise another interrupt while EHB is set.
                write_erdp(io, layout, image.event_ring.dequeue_iova());
                for _ in 0..HID_EVENTS_PER_WAKE {
                    let Some((_cc, _, trb_iova)) = poll_transfer_event_slots(
                        io,
                        layout,
                        &mut *image,
                        evt,
                        &slots[..slot_count],
                        IRQ_EVENT_DRAIN_LIMIT,
                    ) else {
                        break;
                    };
                    for lane in lanes.iter_mut().flatten() {
                        if !lane.pending || lane.token != trb_iova {
                            continue;
                        }
                        let _ = lane.ring.reclaim_through(trb_iova);
                        lane.pending = false;
                        dsb();
                        let report: [u8; kumo_hid::REPORT_BYTES] = lane.report.read(0);
                        if lane.hid.is_mouse() {
                            // Boot mouse: forward the 3-byte `[buttons, dx, dy]` wire form Sora's
                            // pointer pipeline already consumes. Idle reports repeat at the
                            // polling interval, so drop them rather than waking Sora for nothing.
                            if let Some((motion, _edges)) = lane
                                .mouse_decoder
                                .decode(&report[..lane.hid.max_packet_size.min(8) as usize], None)
                            {
                                let mouse = MOUSE_CHANNEL.load(Ordering::Relaxed);
                                if mouse != 0 && !motion.is_idle() {
                                    let wire = motion.to_wire();
                                    let _ = channel_write(Handle(mouse), wire.as_ptr(), wire.len());
                                }
                            }
                        } else if let Ok(events) = lane.decoder.decode(report) {
                            let keyboard = KEYBOARD_CHANNEL.load(Ordering::Relaxed);
                            for event in events.as_slice() {
                                if event.state == kumo_hid::KeyState::Pressed {
                                    let bytes = event.symbol.terminal_bytes();
                                    let slice = bytes.as_slice();
                                    if keyboard != 0 && !slice.is_empty() {
                                        let _ = channel_write(
                                            Handle(keyboard),
                                            slice.as_ptr(),
                                            slice.len(),
                                        );
                                    }
                                }
                            }
                        }
                        break;
                    }
                }
                interrupt_complete(interrupt);
            }
        }
        // This child runs above Sora. Falling back to the old busy-poll here monopolises the CPU
        // for tens of millions of MMIO/barrier iterations, making a local IRQ-creation failure
        // look like a system-wide lock (serial IRQ-side sysrq remains the only live path). Keep
        // the failure contained and leave the other controllers and serial shell operational.
        return log(b"drv-xhci: interrupt create fail; not serving\n");
    }

    // A resident HID device without an interrupt cannot be served safely at this priority.
    log(b"drv-xhci: hid found but no irq granted; not serving\n");
}

/// Bring up every device's lanes and hand them to one resident loop that never returns.
fn run_hid_devices(
    devices: &mut [Option<KeyboardHandoff>; MAX_HID_DEVICES],
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    cmd: &mut DmaTrbs,
    evt: &DmaTrbs,
) {
    let mut lanes: [Option<HidLane>; MAX_HID_LANES] = [None, None, None, None];
    let mut count = 0usize;
    for device in devices.iter_mut().flatten() {
        device.bring_up(io, layout, image, cmd, evt, &mut lanes, &mut count);
    }
    if count == 0 {
        return log(b"drv-xhci: no hid lanes; not serving\n");
    }
    run_hid_lanes(io, layout, image, evt, &mut lanes);
}

/// Issue a no-data control transfer over EP0 (Setup + IN Status), for SET_CONFIGURATION and
/// SET_PROTOCOL. Returns `(completion_code, remaining)`.
#[allow(clippy::too_many_arguments)]
fn control_no_data(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    evt: &DmaTrbs,
    ep0_ring: &mut TransferRing,
    ep0_seg: &mut DmaTrbs,
    slot_id: u8,
    setup_packet: [u8; 8],
) -> Option<(u8, u32)> {
    let setup = Trb::setup_stage(setup_packet, SetupTransferType::NoData, 0).ok()?;
    // A no-data control transfer's Status stage is IN, with IOC.
    let status = Trb::status_stage(0, true, true).ok()?;
    ep0_ring.enqueue(&mut *ep0_seg, setup).ok()?;
    let status_token = ep0_ring.enqueue(&mut *ep0_seg, status).ok()?;
    dsb();
    ring_slot_doorbell(
        io,
        layout,
        slot_id,
        endpoint_context_index(0, false).unwrap_or(1),
    );
    let (cc, remaining, trb_iova) =
        wait_ep0_status(io, layout, image, evt, slot_id, status_token.iova)?;
    let _ = ep0_ring.reclaim_through(trb_iova);
    Some((cc, remaining))
}

/// Transfer events a control transfer may skip before giving up on its own Status stage.
const EP0_STALE_EVENT_LIMIT: u32 = 8;

/// Wait for the Transfer Event that completes *this* control transfer: the one naming its Status
/// TRB. Matching on the slot alone let an event left over from an earlier transfer complete the
/// next one before its data arrived. J538's first boot read a hub descriptor of 16 zero bytes on
/// both hubs that way. Any other event for the slot is logged and skipped. — TIMBERDOODLE
fn wait_ep0_status(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    evt: &DmaTrbs,
    slot_id: u8,
    status_iova: u64,
) -> Option<(u8, u32, u64)> {
    for _ in 0..EP0_STALE_EVENT_LIMIT {
        let (cc, remaining, trb_iova) = poll_transfer_event(io, layout, image, evt, slot_id)?;
        if trb_iova == status_iova {
            return Some((cc, remaining, trb_iova));
        }
        log(b"drv-xhci: ep0 skipped event slot=");
        log_hex(slot_id as u64);
        log(b" trb=");
        log_hex(trb_iova);
        log(b" cc=");
        log_hex(cc as u64);
        log(b" rem=");
        log_hex(remaining as u64);
        log(b" want=");
        log_hex(status_iova);
        log(b"\n");
    }
    log(b"drv-xhci: ep0 status event never arrived\n");
    None
}

/// The xHCI Interval field for an interrupt endpoint, from PORTSC speed + bInterval. Full/Low speed
/// bInterval is in 1 ms frames → Interval = log2(bInterval * 8 * 125us units); High/Super it is
/// already a log2 exponent (1..16) → Interval = bInterval - 1. Clamped to the 0..15 context field.
fn xhci_interval(speed_raw: u8, b_interval: u8) -> u8 {
    match speed_raw {
        1 | 2 => {
            let mut microframes = (b_interval.max(1) as u32) * 8;
            let mut interval = 0u8;
            while microframes > 1 && interval < 15 {
                microframes >>= 1;
                interval += 1;
            }
            interval
        }
        _ => b_interval.saturating_sub(1).min(15),
    }
}

fn log_cc(prefix: &[u8], cc: u8) {
    log(prefix);
    log_hex(cc as u64);
    log(b"\n");
}

/// What a device's descriptors say it is, copied out of device memory for the class matchers.
struct DeviceConfig {
    device_class: u8,
    config_value: u8,
    len: usize,
    bytes: [u8; DATA_BUF_LEN],
}

impl DeviceConfig {
    fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// Read a hub-downstream device's descriptors over its EP0. Handles the speed-aware
/// `bMaxPacketSize0` dance (8-byte probe → optional Evaluate Context → full read), then copies the
/// configuration descriptor out for the caller to match a class against (J541: it used to look
/// only for a boot keyboard).
#[allow(clippy::too_many_arguments)]
fn read_device_config(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    cmd: &mut DmaTrbs,
    evt: &DmaTrbs,
    ep0_ring: &mut TransferRing,
    ep0_seg: &mut DmaTrbs,
    slot_id: u8,
    base_va: u64,
    base_phys: u64,
    speed: UsbSpeed,
    data_buf: DmaBuf,
) -> Option<DeviceConfig> {
    let data_phys = base_phys + OFF_DATA as u64;
    let ep0_ring_phys = base_phys + OFF_EP0_RING as u64;
    let input_ctx_phys = base_phys + OFF_INPUT_CTX as u64;

    let mps0 = speed.initial_ep0_max_packet_size();

    // Phase 1: 8-byte device descriptor at this speed's initial EP0 packet size.
    data_buf.zero();
    dsb();
    let dev8 = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 8, 0x00];
    match control_in(
        io,
        layout,
        image,
        evt,
        ep0_ring,
        &mut *ep0_seg,
        slot_id,
        dev8,
        data_phys,
        8,
    ) {
        Some((1, _)) => {}
        Some((cc, _)) => {
            log_cc(b"drv-xhci: dev desc8 cc=", cc);
            return None;
        }
        None => {
            log(b"drv-xhci: dev desc8 timeout\n");
            return None;
        }
    }
    dsb();
    let encoded_mps0 = data_buf.byte(7);
    let real_mps0 = match speed.decode_ep0_max_packet_size(encoded_mps0) {
        Ok(size) => size,
        Err(_) => {
            log(b"drv-xhci: dev invalid ep0 mps0 raw=");
            log_hex(encoded_mps0 as u64);
            log(b"\n");
            return None;
        }
    };
    log(b"drv-xhci: dev ep0 mps0 raw=");
    log_hex(encoded_mps0 as u64);
    log(b" decoded=");
    log_hex(real_mps0 as u64);
    log(b"\n");

    // Phase 2: correct EP0's max packet size if the device disagrees with our default.
    if real_mps0 != 0 && real_mps0 != mps0 {
        let mut eval_icc = InputControlContext::default();
        eval_icc.add(1).ok()?;
        let eval_ep0 = EndpointContext::control(real_mps0, ep0_ring_phys).ok()?;
        let ep0_off = ContextSize::Bytes64.input_offset(1)?;
        write_ctx(base_va + OFF_INPUT_CTX as u64, eval_icc.words());
        write_ctx(
            base_va + OFF_INPUT_CTX as u64 + ep0_off as u64,
            eval_ep0.words(),
        );
        dsb();
        const TRB_TYPE_EVALUATE_CONTEXT: u32 = 13;
        let eval = Trb::from_words([
            input_ctx_phys as u32,
            (input_ctx_phys >> 32) as u32,
            0,
            (TRB_TYPE_EVALUATE_CONTEXT << 10) | ((slot_id as u32) << 24),
        ]);
        match submit_command(io, layout, image, cmd, evt, eval) {
            Some((1, _, _)) => {
                log(b"drv-xhci: dev eval ctx mps0=");
                log_hex(real_mps0 as u64);
                log(b"\n");
            }
            Some((cc, _, _)) => {
                log_cc(b"drv-xhci: dev eval ctx cc=", cc);
                return None;
            }
            None => {
                log(b"drv-xhci: dev eval ctx timeout\n");
                return None;
            }
        }
    }

    // Phase 3: the full device descriptor.
    data_buf.zero();
    dsb();
    let dev_full = [
        0x80,
        0x06,
        0x00,
        0x01,
        0x00,
        0x00,
        DEVICE_DESCRIPTOR_LEN as u8,
        0x00,
    ];
    match control_in(
        io,
        layout,
        image,
        evt,
        ep0_ring,
        &mut *ep0_seg,
        slot_id,
        dev_full,
        data_phys,
        DEVICE_DESCRIPTOR_LEN as u16,
    ) {
        Some((1, _)) => {
            dsb();
            log(b"drv-xhci: dev desc vid=");
            log_hex(u16::from_le_bytes([data_buf.byte(8), data_buf.byte(9)]) as u64);
            log(b" pid=");
            log_hex(u16::from_le_bytes([data_buf.byte(10), data_buf.byte(11)]) as u64);
            log(b"\n");
        }
        Some((cc, _)) => {
            log_cc(b"drv-xhci: dev desc cc=", cc);
            return None;
        }
        None => {
            log(b"drv-xhci: dev desc timeout\n");
            return None;
        }
    }

    // bDeviceClass, captured before data_buf is reused.
    let device_class = data_buf.byte(4);

    // Configuration descriptor: header for wTotalLength, then the whole thing.
    data_buf.zero();
    dsb();
    let cfg_head = [0x80, 0x06, 0x00, 0x02, 0x00, 0x00, 9, 0x00];
    control_in(
        io,
        layout,
        image,
        evt,
        ep0_ring,
        &mut *ep0_seg,
        slot_id,
        cfg_head,
        data_phys,
        9,
    )?;
    dsb();
    let total = u16::from_le_bytes([data_buf.byte(2), data_buf.byte(3)]).min(DATA_BUF_LEN as u16);
    data_buf.zero();
    dsb();
    let cfg = [
        0x80,
        0x06,
        0x00,
        0x02,
        0x00,
        0x00,
        total as u8,
        (total >> 8) as u8,
    ];
    control_in(
        io,
        layout,
        image,
        evt,
        ep0_ring,
        &mut *ep0_seg,
        slot_id,
        cfg,
        data_phys,
        total,
    )?;
    dsb();
    let mut config = [0u8; DATA_BUF_LEN];
    data_buf.read_into(0, &mut config[..total as usize]);
    Some(DeviceConfig {
        device_class,
        config_value: config[5],
        len: total as usize,
        bytes: config,
    })
}

/// Issue a Configure Endpoint command adding one interrupt-IN endpoint (and bumping the slot's
/// Context Entries) at its DCI. The endpoint's transfer ring is `OFF_INT_RING`. Returns the DCI on
/// success, logging the completion code on failure. Shared by the keyboard and hub paths.
#[allow(clippy::too_many_arguments)]
fn configure_interrupt_endpoint(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    cmd: &mut DmaTrbs,
    evt: &DmaTrbs,
    slot_id: u8,
    base_va: u64,
    enum_phys: u64,
    slot_words: [u32; 8],
    endpoint_address: u8,
    max_packet_size: u16,
    b_interval: u8,
    speed_raw: u8,
) -> Option<u8> {
    let dci = endpoint_context_index(endpoint_address & 0x0f, endpoint_address & 0x80 != 0)?;
    let input_ctx_phys = enum_phys + OFF_INPUT_CTX as u64;
    let int_ring_phys = enum_phys + OFF_INT_RING as u64;

    let mut icc = InputControlContext::default();
    icc.add(0).ok()?; // slot (to raise Context Entries)
    icc.add(dci).ok()?; // the interrupt endpoint
                        // The caller supplies the slot context so a hub-downstream device keeps its route string and
                        // TT fields; only Context Entries is raised here to cover the new endpoint's DCI.
    let mut slot = slot_words;
    slot[0] = (slot[0] & !(0x1f << 27)) | ((dci as u32) << 27);
    let interval = xhci_interval(speed_raw, b_interval);
    let int_ctx = EndpointContext::interrupt_in(
        max_packet_size,
        interval,
        int_ring_phys,
        max_packet_size.max(1),
    )
    .ok()?;
    let stride = ContextSize::Bytes64;
    let slot_off = stride.input_offset(0)?;
    let ep_off = stride.input_offset(dci)?;
    write_ctx(base_va + OFF_INPUT_CTX as u64, icc.words());
    write_ctx(base_va + OFF_INPUT_CTX as u64 + slot_off as u64, slot);
    write_ctx(
        base_va + OFF_INPUT_CTX as u64 + ep_off as u64,
        int_ctx.words(),
    );
    dsb();
    let configure = Trb::configure_endpoint(input_ctx_phys, slot_id).ok()?;
    match submit_command(io, layout, image, cmd, evt, configure) {
        Some((1, _, _)) => Some(dci),
        Some((cc, _, _)) => {
            log_cc(b"drv-xhci: config ep cc=", cc);
            None
        }
        None => {
            log(b"drv-xhci: config ep timeout\n");
            None
        }
    }
}

/// Slice U1: bring a USB hub online — GET_DESCRIPTOR(Hub) for the port count, SET_CONFIGURATION,
/// Configure Endpoint for the status-change interrupt endpoint, power every downstream port, read
/// one status-change report, then GET_STATUS each port to find which one has a device. Proves
/// SET_CONFIGURATION + Configure Endpoint + interrupt transfers on the reliably-present hub.
#[allow(clippy::too_many_arguments)]
fn configure_and_read_hub(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    cmd: &mut DmaTrbs,
    evt: &DmaTrbs,
    ep0_ring: &mut TransferRing,
    ep0_seg: &mut DmaTrbs,
    slot_id: u8,
    page_va: u64,
    enum_phys: u64,
    speed_raw: u8,
    port_number: u8,
    config_value: u8,
    data_buf: DmaBuf,
    data_phys: u64,
    hub_int: Option<kumo_hid::InterruptInEndpoint>,
) {
    // --- SET_CONFIGURATION, before any hub-class request ---
    // The Linux hub driver reads the hub descriptor only once the hub is configured; follow it.
    let set_config = [0x00, 0x09, config_value, 0x00, 0x00, 0x00, 0x00, 0x00];
    match control_no_data(
        io,
        layout,
        image,
        evt,
        ep0_ring,
        &mut *ep0_seg,
        slot_id,
        set_config,
    ) {
        Some((1, _)) => log(b"drv-xhci: hub set config cc=0x1\n"),
        Some((cc, _)) => return log_cc(b"drv-xhci: hub set config cc=", cc),
        None => return log(b"drv-xhci: hub set config timeout\n"),
    }

    // --- GET_DESCRIPTOR(Hub) --- class request; descriptor type 0x2A for SuperSpeed hubs, else
    // 0x29. bNbrPorts is byte 2.
    let hub_desc_type = if speed_raw >= 4 { 0x2a } else { 0x29 };
    data_buf.zero();
    dsb();
    let hub_setup = [0xa0, 0x06, 0x00, hub_desc_type, 0x00, 0x00, 16, 0x00];
    match control_in(
        io,
        layout,
        image,
        evt,
        ep0_ring,
        &mut *ep0_seg,
        slot_id,
        hub_setup,
        data_phys,
        16,
    ) {
        Some((1, _)) => {}
        Some((cc, _)) => return log_cc(b"drv-xhci: hub desc cc=", cc),
        None => return log(b"drv-xhci: hub desc timeout\n"),
    }
    dsb();
    let reply: [u8; 16] = data_buf.read(0);
    let Ok(hub) = HubDescriptor::parse(&reply) else {
        log(b"drv-xhci: hub desc invalid bytes=");
        for byte in &reply[..8] {
            log(b" ");
            log_hex(*byte as u64);
        }
        return log(b"\n");
    };
    let nbr_ports = hub.port_count();
    log(b"drv-xhci: hub ports=");
    log_hex(nbr_ports as u64);
    log(b" power-good ms=");
    log_hex(hub.power_good_delay_ns() / 1_000_000);
    log(b"\n");
    // Everything below is one bounded pass: a driver child cannot be preempted, so the hub work
    // must promise an upper bound on how long it holds the CPU away from a live keyboard driver.
    let pass = Deadline::after(HUB_PASS_BUDGET_NS);

    // A SuperSpeed hub routes by the Route String, indexed by its own tier: SET_HUB_DEPTH (USB 3.2
    // §10.16.2.9) tells it which nibble is its own. A hub on a root port is depth 0.
    if speed_raw >= 4 {
        let set_depth = [0x20, 0x0c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        match control_no_data(
            io,
            layout,
            image,
            evt,
            ep0_ring,
            &mut *ep0_seg,
            slot_id,
            set_depth,
        ) {
            Some((1, _)) => log(b"drv-xhci: hub depth=0 cc=0x1\n"),
            Some((cc, _)) => return log_cc(b"drv-xhci: hub depth cc=", cc),
            None => return log(b"drv-xhci: hub depth timeout\n"),
        }
    }

    // --- Configure the hub's status-change interrupt endpoint ---
    let hub_dci = match hub_int {
        Some(ep) => configure_interrupt_endpoint(
            io,
            layout,
            image,
            cmd,
            evt,
            slot_id,
            page_va,
            enum_phys,
            slot_context_words(0, speed_raw, 1, port_number, 0, 0),
            ep.in_endpoint_address,
            ep.max_packet_size,
            ep.interval,
            speed_raw,
        ),
        None => {
            log(b"drv-xhci: hub no int ep\n");
            None
        }
    };
    if hub_dci.is_some() {
        log(b"drv-xhci: hub config ep cc=0x1\n");
    }

    // --- Power every downstream port (SET_FEATURE PORT_POWER, feature 8) ---
    let mut port = 1u8;
    while port <= nbr_ports {
        if pass.expired() {
            return log(b"drv-xhci: hub pass budget spent (power)\n");
        }
        let set_power = [0x23, 0x03, 0x08, 0x00, port, 0x00, 0x00, 0x00];
        let _ = control_no_data(
            io,
            layout,
            image,
            evt,
            ep0_ring,
            &mut *ep0_seg,
            slot_id,
            set_power,
        );
        port += 1;
    }
    // A port's VBUS is not good until bPwrOn2PwrGood after PORT_POWER, and only then can its
    // device begin to attach. The old fixed spin was ~4 ms on metal, so a keyboard behind a
    // freshly powered hub was never seen on a cold boot. Sleep, rather than spin: this wait is
    // tens to hundreds of milliseconds and must not hold the CPU. — TIMBERDOODLE
    settle(hub.power_good_delay_ns());

    // --- Read one status-change report from the hub's interrupt endpoint ---
    if let Some(dci) = hub_dci {
        let report_phys = enum_phys + OFF_REPORT as u64;
        let int_ring_base = enum_phys + OFF_INT_RING as u64;
        let int_seg = &mut unsafe {
            DmaTrbs::new((page_va as usize + OFF_INT_RING) as *mut Trb, INT_RING_TRBS)
        };
        if let Ok(mut int_ring) = TransferRing::new(&mut *int_seg, int_ring_base) {
            // SAFETY: inside this hub's mapped page, and only reached through `DmaBuf`.
            let report_buf =
                unsafe { DmaBuf::new((page_va as usize + OFF_REPORT) as *mut u8, report_phys, 4) };
            report_buf.zero();
            dsb();
            if let Ok(normal) = Trb::normal_transfer(NormalTransfer::interrupt_in(report_phys, 4)) {
                if int_ring.enqueue(&mut *int_seg, normal).is_ok() {
                    dsb();
                    ring_slot_doorbell(io, layout, slot_id, dci);
                    // Tight budget: a driver child is not preemptible (the IRQ handoff only
                    // switches from Sora's context), so a long spin here yields the CPU to nobody
                    // and strands a sibling controller's masked keyboard interrupt.
                    let mut got = None;
                    for _ in 0..HUB_STATUS_WAIT_ROUNDS {
                        if pass.expired() {
                            break;
                        }
                        if let Some(result) = poll_transfer_event_limited(
                            io,
                            layout,
                            image,
                            evt,
                            slot_id,
                            IRQ_EVENT_DRAIN_LIMIT,
                        ) {
                            got = Some(result);
                            break;
                        }
                    }
                    match got {
                        Some((cc, _, trb_iova)) => {
                            let _ = int_ring.reclaim_through(trb_iova);
                            dsb();
                            log(b"drv-xhci: hub status cc=");
                            log_hex(cc as u64);
                            log(b" ");
                            for byte in report_buf.read::<4>(0) {
                                log_hex(byte as u64);
                                log(b" ");
                            }
                            log(b"\n");
                        }
                        None => log(b"drv-xhci: hub status idle\n"),
                    }
                }
            }
        }
    }

    // --- Wait for a device to attach and hold still (USB 2.0 §7.1.7.3), then pick its port ---
    let attach_start = clock_get();
    let polled_ports = nbr_ports.min(kumo_xhci::MAX_HUB_PORTS as u8);
    let mut debounce = HubConnectDebounce::new(polled_ports, attach_start);
    let mut last_status = [None::<u16>; kumo_xhci::MAX_HUB_PORTS];
    let mut rounds = 0u32;
    let stable = loop {
        let mut port = 1u8;
        while port <= polled_ports {
            data_buf.zero();
            dsb();
            let get_status = [0xa3, 0x00, 0x00, 0x00, port, 0x00, 0x04, 0x00];
            if let Some((1, _)) = control_in(
                io,
                layout,
                image,
                evt,
                ep0_ring,
                &mut *ep0_seg,
                slot_id,
                get_status,
                data_phys,
                4,
            ) {
                dsb();
                let reply: [u8; 2] = data_buf.read(0);
                let status = u16::from_le_bytes(reply);
                last_status[port as usize - 1] = Some(status);
                debounce.observe(port, status & 1 != 0, clock_get());
            }
            port += 1;
        }
        match debounce.verdict(clock_get()) {
            HubConnectVerdict::Stable(ports) => break ports,
            HubConnectVerdict::Empty => break HubPorts::empty(),
            HubConnectVerdict::Waiting if rounds < HUB_ATTACH_MAX_ROUNDS => {
                rounds += 1;
                settle(kumo_xhci::HUB_DEBOUNCE_STEP_NS);
            }
            // The wall clock never settled the hub: take what has held still so far.
            HubConnectVerdict::Waiting => break debounce.stable_ports(clock_get()),
        }
    };
    for (index, status) in last_status[..polled_ports as usize].iter().enumerate() {
        log(b"drv-xhci: hub port");
        log_hex(index as u64 + 1);
        match status {
            Some(status) => {
                log(b" status=");
                log_hex(*status as u64);
                log(b" conn=");
                log_hex((status & 1) as u64);
            }
            None => log(b" status unread"),
        }
        log(b"\n");
    }
    log(b"drv-xhci: hub attach ms=");
    log_hex(clock_get().saturating_sub(attach_start) / 1_000_000);
    log(b"\n");
    log(b"drv-xhci: hub scan done\n");

    if speed_raw >= 4 {
        // A device behind a SuperSpeed hub needs a SuperSpeed slot context (Route String, no
        // Transaction Translator) and SS port-status decoding that `enumerate_hub_downstream` does
        // not have yet. Name what is there; enumerating it is the next slice. — TIMBERDOODLE
        for down_port in stable.iter() {
            log(b"drv-xhci: ss hub dev port=");
            log_hex(down_port as u64);
            log(b" (downstream enumeration pending)\n");
        }
        return;
    }

    // U2/J541: reach through the hub to every stable port, in ascending order and one at a time:
    // only one device may answer at address 0 while its port resets. Each gets its own slot, page
    // and pass budget (the attach wait above slept rather than held the CPU). The boot HID
    // devices' shared report loop never returns, so they run only after every one-shot device
    // (storage) has had its turn. — TIMBERDOODLE
    let mut devices: [Option<KeyboardHandoff>; MAX_HID_DEVICES] = [None, None];
    let mut device_count = 0usize;
    for down_port in stable.iter() {
        let found = enumerate_hub_downstream(
            io,
            layout,
            image,
            cmd,
            evt,
            ep0_ring,
            ep0_seg,
            slot_id,
            port_number,
            speed_raw,
            data_phys,
            data_buf,
            down_port,
            Deadline::after(HUB_PASS_BUDGET_NS),
        );
        if let Some(handoff) = found {
            if device_count < MAX_HID_DEVICES {
                devices[device_count] = Some(handoff);
                device_count += 1;
            } else {
                log(b"drv-xhci: hub dev third hid device left alone\n");
            }
        }
    }
    if device_count > 0 {
        run_hid_devices(&mut devices, io, layout, image, cmd, evt);
    }
}

/// A hub-downstream boot HID device, addressed and described. The shared report loop never
/// returns, so the handoffs wait here until every downstream device has had its turn (J541).
#[derive(Clone, Copy)]
struct KeyboardHandoff {
    slot_id: u8,
    page_va: u64,
    page_phys: u64,
    speed_raw: u8,
    slot_words: [u32; 8],
    config_value: u8,
    plan: HidPlan,
    ep0_ring: TransferRing,
}

impl KeyboardHandoff {
    fn bring_up(
        &mut self,
        io: &mut Mmio,
        layout: RegisterLayout,
        image: &mut NoOpCommandImage,
        cmd: &mut DmaTrbs,
        evt: &DmaTrbs,
        lanes: &mut [Option<HidLane>; MAX_HID_LANES],
        count: &mut usize,
    ) {
        // SAFETY: the device's page stays mapped for the driver's lifetime.
        let ep0_seg = &mut unsafe {
            DmaTrbs::new(
                (self.page_va as usize + OFF_EP0_RING) as *mut Trb,
                EP0_RING_TRBS,
            )
        };
        bring_up_hid_device(
            io,
            layout,
            image,
            cmd,
            evt,
            &mut self.ep0_ring,
            &mut *ep0_seg,
            self.slot_id,
            self.page_va,
            self.page_phys,
            self.speed_raw,
            self.slot_words,
            self.config_value,
            self.plan,
            lanes,
            count,
        );
    }
}

/// The next unused downstream device page in the `DEV2_VA` window, mapped: `(va, phys)` (J541).
fn map_downstream_page() -> Option<(u64, u64)> {
    let index = DOWNSTREAM_PAGES_USED.fetch_add(1, Ordering::Relaxed);
    if index >= DOWNSTREAM_PAGES {
        return skipped(b"drv-xhci: hub dev pages exhausted\n");
    }
    let va = DEV2_VA + index as u64 * PAGE_SIZE;
    map_device_page(va).map(|phys| (va, phys))
}

/// Log why a downstream device was skipped.
fn skipped<T>(msg: &[u8]) -> Option<T> {
    log(msg);
    None
}

fn skipped_cc<T>(prefix: &[u8], cc: u8) -> Option<T> {
    log_cc(prefix, cc);
    None
}

/// Slice U2/J541: enumerate the device on `down_port` of an already-configured hub. Mass storage is
/// probed here and now; a boot keyboard comes back as a [`KeyboardHandoff`] for the caller to run
/// once every port has had its turn.
#[allow(clippy::too_many_arguments)]
fn enumerate_hub_downstream(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    cmd: &mut DmaTrbs,
    evt: &DmaTrbs,
    hub_ep0_ring: &mut TransferRing,
    hub_ep0_seg: &mut DmaTrbs,
    hub_slot_id: u8,
    hub_root_port: u8,
    hub_speed_raw: u8,
    hub_data_phys: u64,
    hub_data_buf: DmaBuf,
    down_port: u8,
    pass: Deadline,
) -> Option<KeyboardHandoff> {
    let Some(DownstreamDevice {
        slot_id: dev_slot,
        speed_raw: down_speed,
        page_va: dev_va,
        page_phys: dev_phys,
        slot_words,
        mut ep0_ring,
    }) = reset_and_address_downstream(
        io,
        layout,
        image,
        cmd,
        evt,
        &mut *hub_ep0_ring,
        &mut *hub_ep0_seg,
        hub_slot_id,
        hub_root_port,
        hub_speed_raw,
        hub_data_phys,
        hub_data_buf,
        down_port,
        pass,
    )
    else {
        // The device may have come out of reset and be answering at address 0. Left enabled, it
        // would answer beside the next port's device while that one is addressed. — TIMBERDOODLE
        disable_hub_port(
            io,
            layout,
            image,
            evt,
            hub_ep0_ring,
            hub_ep0_seg,
            hub_slot_id,
            down_port,
        );
        return None;
    };
    // SAFETY: the device's page stays mapped for the driver's lifetime.
    let ep0_seg =
        &mut unsafe { DmaTrbs::new((dev_va as usize + OFF_EP0_RING) as *mut Trb, EP0_RING_TRBS) };
    let speed = usb_speed(down_speed);

    // --- Descriptors over the downstream device's EP0 ---
    let data_phys = dev_phys + OFF_DATA as u64;
    // SAFETY: the downstream page is mapped for the driver's lifetime and only reached through
    // `DmaBuf`.
    let data_buf = unsafe {
        DmaBuf::new(
            (dev_va as usize + OFF_DATA) as *mut u8,
            data_phys,
            DATA_BUF_LEN,
        )
    };
    let config = read_device_config(
        io,
        layout,
        image,
        cmd,
        evt,
        &mut ep0_ring,
        &mut *ep0_seg,
        dev_slot,
        dev_va,
        dev_phys,
        speed,
        data_buf,
    )?;

    // --- What is it? Storage is probed now; a keyboard waits for the other ports ---
    if config.device_class == kumo_hid::USB_CLASS_HUB {
        return skipped(b"drv-xhci: hub dev is a hub; nested hubs left alone\n");
    }
    match kumo_usb_storage::find_bulk_only_scsi_interface(config.bytes()) {
        Ok(Some(storage)) => {
            log_storage_interface(storage);
            configure_and_probe_storage(
                io,
                layout,
                image,
                cmd,
                evt,
                &mut ep0_ring,
                &mut *ep0_seg,
                dev_slot,
                dev_va,
                dev_phys,
                slot_words,
                storage,
                data_buf,
            );
            return None;
        }
        // As on a root port: a configuration the BOT matcher rejects may still be a keyboard's.
        Err(_) => log(b"drv-xhci: malformed storage interface\n"),
        Ok(None) => {}
    }
    let Some(plan) = HidPlan::from_config(config.bytes()) else {
        return skipped(b"drv-xhci: dev no hid boot iface\n");
    };
    let Some(primary) = plan.primary() else {
        return skipped(b"drv-xhci: dev no hid boot iface\n");
    };
    log(b"drv-xhci: dev hid if=");
    log_hex(primary.interface_number as u64);
    log(b" proto=");
    log_hex(primary.protocol as u64);
    log(b" ep=");
    log_hex(primary.in_endpoint_address as u64);
    log(b"\n");
    if let Some(mouse) = plan.mouse.filter(|_| plan.keyboard.is_some()) {
        log(b"drv-xhci: dev hid mouse if=");
        log_hex(mouse.interface_number as u64);
        log(b" proto=");
        log_hex(mouse.protocol as u64);
        log(b" ep=");
        log_hex(mouse.in_endpoint_address as u64);
        log(b"\n");
    }
    Some(KeyboardHandoff {
        slot_id: dev_slot,
        page_va: dev_va,
        page_phys: dev_phys,
        speed_raw: down_speed,
        slot_words,
        config_value: config.config_value,
        plan,
        ep0_ring,
    })
}

/// A hub-downstream device that answered Address Device on its own slot and page.
#[derive(Clone, Copy)]
struct DownstreamDevice {
    slot_id: u8,
    speed_raw: u8,
    page_va: u64,
    page_phys: u64,
    slot_words: [u32; 8],
    ep0_ring: TransferRing,
}

/// Slice U2: reset `down_port` of an already-configured hub through hub class requests, then
/// address its device with a **route string** (and, for a Full/Low-speed device behind a
/// High-speed hub, the Transaction Translator fields) so the controller can reach it through the
/// hub — multi-tier addressing. `None` means the device may still sit at address 0.
#[allow(clippy::too_many_arguments)]
fn reset_and_address_downstream(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    cmd: &mut DmaTrbs,
    evt: &DmaTrbs,
    hub_ep0_ring: &mut TransferRing,
    hub_ep0_seg: &mut DmaTrbs,
    hub_slot_id: u8,
    hub_root_port: u8,
    hub_speed_raw: u8,
    hub_data_phys: u64,
    hub_data_buf: DmaBuf,
    down_port: u8,
    pass: Deadline,
) -> Option<DownstreamDevice> {
    // --- Reset the downstream port (SET_FEATURE PORT_RESET) ---
    let set_reset = [
        0x23,
        0x03,
        HUB_FEATURE_PORT_RESET,
        0x00,
        down_port,
        0x00,
        0x00,
        0x00,
    ];
    match control_no_data(
        io,
        layout,
        image,
        evt,
        hub_ep0_ring,
        &mut *hub_ep0_seg,
        hub_slot_id,
        set_reset,
    ) {
        Some((1, _)) => {}
        Some((cc, _)) => return skipped_cc(b"drv-xhci: hub port reset cc=", cc),
        None => return skipped(b"drv-xhci: hub port reset timeout\n"),
    }

    // Wait for the port to leave reset and report enabled, then read its negotiated speed.
    let mut down_speed = 0u8;
    let mut enabled = false;
    for _ in 0..HUB_RESET_POLL_ROUNDS {
        if pass.expired() {
            return skipped(b"drv-xhci: hub pass budget spent (reset)\n");
        }
        // Keep each settle short: this driver preempts Sora, so a long spin here stalls the
        // whole system (and a sibling controller's live keyboard) while a port comes up.
        spin_delay(200_000);
        hub_data_buf.zero();
        dsb();
        let get_status = [0xa3, 0x00, 0x00, 0x00, down_port, 0x00, 0x04, 0x00];
        if control_in(
            io,
            layout,
            image,
            evt,
            hub_ep0_ring,
            &mut *hub_ep0_seg,
            hub_slot_id,
            get_status,
            hub_data_phys,
            4,
        )
        .is_none()
        {
            continue;
        }
        dsb();
        let reply: [u8; 2] = hub_data_buf.read(0);
        let status = u16::from_le_bytes(reply);
        if status & HUB_PORT_STATUS_ENABLE != 0 {
            down_speed = if status & HUB_PORT_STATUS_LOW_SPEED != 0 {
                2
            } else if status & HUB_PORT_STATUS_HIGH_SPEED != 0 {
                3
            } else {
                1
            };
            enabled = true;
            break;
        }
    }
    if !enabled {
        return skipped(b"drv-xhci: hub port never enabled\n");
    }
    log(b"drv-xhci: hub dev port=");
    log_hex(down_port as u64);
    log(b" speed=");
    log_hex(down_speed as u64);
    log(b"\n");

    // Acknowledge the reset-complete change bit so the hub stops reporting it.
    let clear_change = [
        0x23,
        0x01,
        HUB_FEATURE_C_PORT_RESET,
        0x00,
        down_port,
        0x00,
        0x00,
        0x00,
    ];
    let _ = control_no_data(
        io,
        layout,
        image,
        evt,
        hub_ep0_ring,
        &mut *hub_ep0_seg,
        hub_slot_id,
        clear_change,
    );

    // --- A private DMA page for this device's contexts, EP0 ring and buffers ---
    let (dev_va, dev_phys) = map_downstream_page()?;

    // --- Enable a slot for the downstream device ---
    let Ok(enable) = Trb::enable_slot(0) else {
        return skipped(b"drv-xhci: hub dev enable build fail\n");
    };
    let dev_slot = match submit_command(io, layout, image, cmd, evt, enable) {
        Some((1, slot, _)) => slot,
        Some((cc, _, _)) => return skipped_cc(b"drv-xhci: hub dev enable cc=", cc),
        None => return skipped(b"drv-xhci: hub dev enable timeout\n"),
    };
    if dev_slot == 0 || dev_slot as usize >= DCBAA_ENTRIES {
        return skipped(b"drv-xhci: hub dev slot out of range\n");
    }
    log(b"drv-xhci: hub dev slot=");
    log_hex(dev_slot as u64);
    log(b"\n");

    // --- Input context: route string + TT for a FS/LS device behind a HS hub ---
    let ep0_ring_phys = dev_phys + OFF_EP0_RING as u64;
    let input_ctx_phys = dev_phys + OFF_INPUT_CTX as u64;
    let output_ctx_phys = dev_phys + OFF_OUTPUT_CTX as u64;
    let ep0_seg =
        &mut unsafe { DmaTrbs::new((dev_va as usize + OFF_EP0_RING) as *mut Trb, EP0_RING_TRBS) };
    let Ok(ep0_ring) = TransferRing::new(&mut *ep0_seg, ep0_ring_phys) else {
        return skipped(b"drv-xhci: hub dev ep0 ring fail\n");
    };

    // One tier down: the route string's first nibble is this hub's downstream port number.
    let needs_tt = matches!(down_speed, 1 | 2) && hub_speed_raw == 3;
    let slot_words = slot_context_words(
        down_port as u32 & 0xf,
        down_speed,
        1,
        hub_root_port,
        if needs_tt { hub_slot_id } else { 0 },
        if needs_tt { down_port } else { 0 },
    );
    let speed = usb_speed(down_speed);
    let mps0 = speed.initial_ep0_max_packet_size();
    let Ok(ep0_ctx) = EndpointContext::control(mps0, ep0_ring_phys) else {
        return skipped(b"drv-xhci: hub dev ep0 ctx fail\n");
    };
    let mut icc = InputControlContext::default();
    let _ = icc.add(0);
    let _ = icc.add(1);
    let stride = ContextSize::Bytes64;
    let (Some(slot_off), Some(ep0_off)) = (stride.input_offset(0), stride.input_offset(1)) else {
        return skipped(b"drv-xhci: hub dev ctx offset fail\n");
    };
    write_ctx(dev_va + OFF_INPUT_CTX as u64, icc.words());
    write_ctx(dev_va + OFF_INPUT_CTX as u64 + slot_off as u64, slot_words);
    write_ctx(
        dev_va + OFF_INPUT_CTX as u64 + ep0_off as u64,
        ep0_ctx.words(),
    );
    // SAFETY: the DCBAA sits inside the mapped DMA page.
    let mut dcbaa =
        unsafe { DmaQwords::new((DMA_VA as usize + OFF_DCBAA) as *mut u64, DCBAA_ENTRIES) };
    dcbaa.write_qword(dev_slot as usize, output_ctx_phys);
    dsb();

    // --- Address Device (through the hub) ---
    let Ok(addr) = Trb::address_device(input_ctx_phys, dev_slot, false) else {
        return skipped(b"drv-xhci: hub dev addr build fail\n");
    };
    match submit_command(io, layout, image, cmd, evt, addr) {
        Some((1, _, _)) => {
            log(b"drv-xhci: hub dev addr cc=0x1 ok\n");
            settle(SET_ADDRESS_RECOVERY_NS);
        }
        Some((cc, _, _)) => return skipped_cc(b"drv-xhci: hub dev addr cc=", cc),
        None => return skipped(b"drv-xhci: hub dev addr timeout\n"),
    }

    Some(DownstreamDevice {
        slot_id: dev_slot,
        speed_raw: down_speed,
        page_va: dev_va,
        page_phys: dev_phys,
        slot_words,
        ep0_ring,
    })
}

/// CLEAR_FEATURE(PORT_ENABLE) on a hub's downstream port (USB 2.0 §11.24.2.2), for a device that
/// was reset but never addressed (J541).
#[allow(clippy::too_many_arguments)]
fn disable_hub_port(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    evt: &DmaTrbs,
    hub_ep0_ring: &mut TransferRing,
    hub_ep0_seg: &mut DmaTrbs,
    hub_slot_id: u8,
    port: u8,
) {
    let clear_enable = [
        0x23,
        0x01,
        HUB_FEATURE_PORT_ENABLE,
        0x00,
        port,
        0x00,
        0x00,
        0x00,
    ];
    let done = control_no_data(
        io,
        layout,
        image,
        evt,
        hub_ep0_ring,
        hub_ep0_seg,
        hub_slot_id,
        clear_enable,
    );
    log(if matches!(done, Some((1, _))) {
        b"drv-xhci: hub port disabled port="
    } else {
        b"drv-xhci: hub port disable failed port="
    });
    log_hex(port as u64);
    log(b"\n");
}

/// Iterations between cooperative yields inside a long driver loop.
const YIELD_INTERVAL: u32 = 512;

/// Wall-clock budget for the entire hub pass (discovery through downstream enumeration).
/// Iteration counts are a poor proxy for "long enough": they vary with clock speed and with how
/// often we yield. A deadline bounds the pass in the unit that actually matters — how long a
/// non-preemptible driver may hold the machine — and lets the pass abort cleanly instead of
/// stranding a sibling controller's keyboard.
const HUB_PASS_BUDGET_NS: u64 = 400_000_000; // 400 ms

/// A monotonic wall-clock budget for one bounded pass of work.
#[derive(Clone, Copy)]
struct Deadline {
    end_ns: u64,
}

impl Deadline {
    fn after(budget_ns: u64) -> Self {
        Self {
            end_ns: clock_get().saturating_add(budget_ns),
        }
    }

    fn expired(self) -> bool {
        clock_get() >= self.end_ns
    }
}

/// Give the scheduler a chance to run someone else. A resident driver child is NOT preemptible —
/// `irq_handoff_allowed` only permits a context switch when Sora is the interrupted context — so a
/// long spin in a driver yields the CPU to nobody and can strand a sibling driver's (masked)
/// interrupt for the whole duration. A cheap syscall reaches an SVC boundary, where
/// `reschedule_if_pending_after_svc` dispatches any child made runnable in the meantime.
#[inline]
fn cooperative_yield() {
    let _ = clock_get();
}

/// How long a device gets after `Address Device` before its first control request. USB 2.0
/// §9.2.6.3 allows it 2 ms of SetAddress recovery; Linux waits 10 ms. Until J559 the log line
/// written between the two was a synchronous UART write, and that was the only delay. Once logs
/// went to the kernel ring, the opi5's USB2 hub got GET_DESCRIPTOR 17 µs after its address,
/// answered with a transaction error (cc 4), and the keyboard and stick behind it never
/// enumerated. — TIMBERDOODLE
const SET_ADDRESS_RECOVERY_NS: u64 = 10_000_000;

/// Wait `delay_ns` without holding the CPU: block on a one-shot timer, and fall back to a
/// deadline-bounded, yielding spin only when the timer cannot be set up.
fn settle(delay_ns: u64) {
    if sleep_ns(delay_ns) {
        return;
    }
    let until = Deadline::after(delay_ns);
    for _ in 0..SETTLE_SPIN_ROUNDS {
        if until.expired() {
            break;
        }
        spin_delay(YIELD_INTERVAL);
    }
}

/// Bounded busy-wait for power-on and reset settle times, yielding periodically so the wait does
/// not monopolise the CPU (see [`cooperative_yield`]).
fn spin_delay(iterations: u32) {
    for i in 0..iterations {
        dsb();
        if i % YIELD_INTERVAL == 0 {
            cooperative_yield();
        }
    }
}

/// Issue a standard control-IN transfer over EP0: Setup + (unchained) Data-IN + Status, three
/// separate TDs, then ring the slot doorbell and poll for the Transfer Event. Returns
/// `(completion_code, remaining)`. The device DMAs the payload into `data_phys`.
#[allow(clippy::too_many_arguments)]
fn control_in(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    evt: &DmaTrbs,
    ep0_ring: &mut TransferRing,
    ep0_seg: &mut DmaTrbs,
    slot_id: u8,
    setup_packet: [u8; 8],
    data_phys: u64,
    length: u16,
) -> Option<(u8, u32)> {
    let setup = Trb::setup_stage(setup_packet, SetupTransferType::In, 0).ok()?;
    // Chain=0 single Data Stage TRB (see the metal note above about data_stage's errant Chain).
    const TRB_TYPE_DATA_STAGE: u32 = 3;
    let data = Trb::from_words([
        data_phys as u32,
        (data_phys >> 32) as u32,
        length as u32,
        (TRB_TYPE_DATA_STAGE << 10) | (1 << 16),
    ]);
    let status = Trb::status_stage(0, false, true).ok()?;
    ep0_ring.enqueue(&mut *ep0_seg, setup).ok()?;
    ep0_ring.enqueue(&mut *ep0_seg, data).ok()?;
    let status_token = ep0_ring.enqueue(&mut *ep0_seg, status).ok()?;
    dsb();
    ring_slot_doorbell(
        io,
        layout,
        slot_id,
        endpoint_context_index(0, false).unwrap_or(1),
    );
    let (cc, remaining, trb_iova) =
        wait_ep0_status(io, layout, image, evt, slot_id, status_token.iova)?;
    // Reclaim the setup/data/status TRBs so the EP0 ring doesn't fill across many transfers.
    let _ = ep0_ring.reclaim_through(trb_iova);
    Some((cc, remaining))
}

/// Read one 32-bit word from an xHCI context in DMA memory (word index, not byte offset).
fn read_ctx_word(va: u64, word_index: usize) -> u32 {
    unsafe { core::ptr::read_volatile((va as usize + word_index * 4) as *const u32) }
}

/// Allocate and map one zeroed, uncached, contiguous device page at `va`; returns its physical
/// address (the IOVA under SMMU bypass).
fn map_device_page(va: u64) -> Option<u64> {
    let (raw, phys) = vmo_create_contiguous(ENUM_LEN);
    if raw == u64::MAX || phys == 0 {
        log(b"drv-xhci: enum vmo fail\n");
        return None;
    }
    if vmar_map(
        Handle(0),
        Handle(raw as u32),
        0,
        va,
        ENUM_LEN,
        (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
    ) != 0
    {
        log(b"drv-xhci: enum map fail\n");
        return None;
    }
    log(b"drv-xhci: enum phys=");
    log_hex(phys);
    log(b"\n");
    Some(phys)
}

/// One PORTSC pass for a ready SuperSpeed port other than `primary`.
fn ready_superspeed_port(caplength: u8, max_ports: u8, primary: u8) -> Option<(u8, u8)> {
    let mut scan = RootPortScan::new();
    let mut port = 0u8;
    while port < max_ports {
        if port + 1 != primary {
            if let Some(offset) = portsc_offset(caplength, port) {
                scan.observe(port + 1, PortStatus::new(read32(offset)));
            }
        }
        port += 1;
    }
    scan.first_superspeed_ready()
}

/// Poll PORTSC after HCRST re-links the root ports until [`RootPortScan::choice`] picks one.
/// Returns `(port_number, speed)` (port number is 1-based).
fn wait_for_device_port(caplength: u8, max_ports: u8) -> Option<(u8, u8)> {
    // PORTSC bits. PED (1) reads enabled; writing 1 disables, so mask it out of a modify-write.
    // The change bits (17..=23, RW1C) must also be written 0 so a modify-write doesn't clear them.
    const PORTSC_PED: u32 = 1 << 1;
    const PORTSC_PR: u32 = 1 << 4;
    const PORTSC_CHANGE_BITS: u32 = 0x7f << 17;
    let mut reset_issued = [false; MAX_LOGGED_PORTS as usize];
    let mut fallback_ready = None;
    let deadline = Deadline::after(PORT_POLL_BUDGET_NS);
    for spin in 0..PORT_POLL_LIMIT {
        dsb();
        if spin != 0 && spin % YIELD_INTERVAL == 0 && deadline.expired() {
            break;
        }
        let mut scan = RootPortScan::new();
        let mut port = 0u8;
        while port < max_ports {
            if let Some(offset) = portsc_offset(caplength, port) {
                let raw = read32(offset);
                let status = PortStatus::new(raw);
                scan.observe(port + 1, status);
                if status.connected() && !status.enabled() {
                    // Connected but not enabled: a USB2 (Full/Low/High) device needs a software
                    // Port Reset to reach Enabled; SuperSpeed ports auto-enable after link
                    // training. Issue the reset once per port, then keep polling for PED.
                    let index = port as usize;
                    if index < reset_issued.len() && !reset_issued[index] {
                        let neutral = raw & !(PORTSC_PED | PORTSC_CHANGE_BITS);
                        write32(offset, neutral | PORTSC_PR);
                        reset_issued[index] = true;
                    }
                }
            }
            port += 1;
        }
        if scan.first_ready().is_some() {
            fallback_ready = scan.first_ready();
        }
        // `RootPortScan::choice` prefers a ready USB2 companion over its SuperSpeed sibling. If
        // a reset never settles, the poll budget bounds the wait and the last known-good ready
        // port is used instead.
        if let Some(ready) = scan.choice() {
            return Some(ready);
        }
    }
    fallback_ready
}

/// Enqueue one command, ring the command doorbell, and poll the event ring for its Command
/// Completion, skipping unrelated events. Returns `(completion_code, slot_id, parameter)`.
fn submit_command(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    cmd: &mut DmaTrbs,
    evt: &DmaTrbs,
    command: Trb,
) -> Option<(u8, u8, u32)> {
    let token = image.command_ring.enqueue(cmd, command).ok()?;
    dsb();
    layout.ring_command_doorbell(io).ok()?;
    for spin in 0..EVENT_POLL_LIMIT {
        dsb();
        // Command completions can take the same metal-scale budget as transfer completions. Keep
        // the scheduler/re-entry probe alive here too, especially while storage endpoint setup is
        // the last printed sysrq-visible phase marker. — KESTREL
        if spin % YIELD_INTERVAL == 0 && spin != 0 {
            cooperative_yield();
        }
        let popped = match image.event_ring.pop(evt) {
            Ok(popped) => popped,
            Err(_) => return None,
        };
        if let Some(trb) = popped {
            let event = trb.decode_event();
            // Every successful pop advances the software dequeue cursor, including port-change
            // and transfer events unrelated to this command. Mirror every such advance to ERDP;
            // publishing only the matching completion leaves hardware and software disagreeing
            // about the ring head whenever boot traffic is interleaved.
            write_erdp(io, layout, image.event_ring.dequeue_iova());
            if let Some(Event::CommandCompletion {
                command_iova,
                parameter,
                completion_code,
                slot_id,
            }) = event
            {
                if command_iova == token.iova {
                    let _ = image.command_ring.reclaim_through(command_iova);
                    return Some((completion_code, slot_id, parameter));
                }
            }
            // Unrelated event (e.g. Port Status Change) — consumed and skipped.
        }
    }
    None
}

/// Poll the event ring for a Transfer Event on `slot_id`, skipping unrelated events. Returns
/// `(completion_code, remaining)`.
/// Poll the event ring for a Transfer Event on `slot_id`. Returns `(completion_code, remaining,
/// trb_iova)` — the `trb_iova` names the completed transfer TRB so the caller can reclaim its
/// transfer ring (otherwise the ring reports full after ~15 TRBs and enqueue silently fails).
fn poll_transfer_event(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    evt: &DmaTrbs,
    slot_id: u8,
) -> Option<(u8, u32, u64)> {
    poll_transfer_event_limited(io, layout, image, evt, slot_id, EVENT_POLL_LIMIT)
}

/// As [`poll_transfer_event`] but with an explicit spin budget. The IRQ-driven loop must use a
/// SMALL budget: this driver runs at a higher priority than Sora, so a long spin here (the
/// enumeration-time 2,000,000) starves the whole system whenever a wake arrives without a matching
/// event. The interrupt already told us an event is queued, so a short drain is sufficient.
fn poll_transfer_event_limited(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    evt: &DmaTrbs,
    slot_id: u8,
    limit: u32,
) -> Option<(u8, u32, u64)> {
    poll_transfer_event_slots(io, layout, image, evt, &[slot_id], limit)
}

/// [`poll_transfer_event_limited`] over a set of slots: the resident HID loop serves lanes on
/// more than one device slot, so a completion for any of those slots is its to reclaim. An event
/// for another slot is stashed for that slot's own poll, exactly as the single-slot path did.
fn poll_transfer_event_slots(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    evt: &DmaTrbs,
    slots: &[u8],
    limit: u32,
) -> Option<(u8, u32, u64)> {
    let stashing = STASH_ENABLED.load(Ordering::Relaxed);
    if stashing {
        for slot in slots {
            if let Some(event) = event_stash().take(*slot) {
                return Some((event.completion_code, event.remaining, event.trb_iova));
            }
        }
    }
    for spin in 0..limit {
        dsb();
        // Long event polls must not monopolise a non-preemptible driver child.
        if spin % YIELD_INTERVAL == 0 && spin != 0 {
            cooperative_yield();
        }
        let popped = match image.event_ring.pop(evt) {
            Ok(popped) => popped,
            Err(_) => return None,
        };
        if let Some(trb) = popped {
            let event = trb.decode_event();
            // A consumed event is no longer owned by the controller regardless of whether it is
            // the transfer this caller wants. In particular, acknowledge boot-time Port Status
            // Change events immediately; deferring ERDP until a later keyboard completion can
            // leave EHB asserted and make the first HID interrupt nondeterministic after reboot.
            write_erdp(io, layout, image.event_ring.dequeue_iova());
            if let Some(Event::Transfer {
                completion_code,
                remaining,
                slot_id: event_slot,
                trb_iova,
                ..
            }) = event
            {
                if slots.contains(&event_slot) {
                    return Some((completion_code, remaining, trb_iova));
                }
                // J545: another device's completion, popped while a HID slot waited. In the
                // resident loop that is the keyboard's report during a block read: keep it for
                // the keyboard's own poll rather than drop it and stall typing. — TIMBERDOODLE
                if stashing {
                    let stashed = StashedTransfer {
                        slot_id: event_slot,
                        completion_code,
                        remaining,
                        trb_iova,
                    };
                    if event_stash().push(stashed).is_some() {
                        log(b"drv-xhci: event stash full; oldest dropped\n");
                    }
                }
            }
        }
    }
    None
}

/// Write a device-slot doorbell: `db[slot_id] = dci` (stream 0).
fn ring_slot_doorbell(io: &mut Mmio, layout: RegisterLayout, slot_id: u8, dci: u8) {
    io.write32(layout.doorbell_offset() + slot_id as usize * 4, dci as u32);
}

/// Provide the scratchpad buffers the controller requires (HCSPARAMS2 Max Scratchpad Buffers):
/// allocate `count` page-sized buffers, publish their physical bases in a scratchpad buffer array
/// in the first DMA page, and point DCBAA[0] at that array. Must run before the controller starts
/// (xHCI 4.20). The buffers are controller-only (never CPU-mapped). Returns false on alloc failure.
fn setup_scratchpad(dma_va: u64, dma_phys: u64, dcbaa: &mut DmaQwords, count: u32) -> bool {
    if count == 0 {
        return true;
    }
    let bytes = count as u64 * PAGE_SIZE;
    let (buffers_raw, buffers_phys) = vmo_create_contiguous(bytes);
    if buffers_raw == u64::MAX || buffers_phys == 0 {
        return false;
    }
    // SAFETY: the scratchpad array sits inside the mapped DMA page.
    let mut array = unsafe {
        DmaQwords::new(
            (dma_va as usize + OFF_SCRATCH_ARRAY) as *mut u64,
            count as usize,
        )
    };
    for index in 0..count as usize {
        array.write_qword(index, buffers_phys + index as u64 * PAGE_SIZE);
    }
    dcbaa.write_qword(0, dma_phys + OFF_SCRATCH_ARRAY as u64);
    dsb();
    true
}

/// Advance the primary interrupter's Event Ring Dequeue Pointer so the controller sees consumed
/// events and never treats the ring as full. `iova` is the current software dequeue; bit 3 (EHB)
/// is written to clear the Event Handler Busy latch. ERDP sits 0x10 past ERSTSZ within IR0.
fn write_erdp(io: &mut Mmio, layout: RegisterLayout, iova: u64) {
    let offset = layout.interrupter0_erst_size_offset() + 0x10;
    let value = (iova & !0xf) | (1 << 3);
    io.write32(offset, value as u32);
    io.write32(offset + 4, (value >> 32) as u32);
}

/// IR0 IMAN register offset (runtime base + IR0(0x20) + IMAN(0x00)).
fn iman_offset(layout: RegisterLayout) -> usize {
    layout.runtime_offset() + 0x20
}

/// Enable the primary interrupter (IMAN.IE) and the controller global interrupt enable
/// (USBCMD.INTE), so a completed transfer raises the xHCI IRQ.
fn enable_interrupter(io: &mut Mmio, layout: RegisterLayout) {
    let iman = io.read32(iman_offset(layout));
    io.write32(iman_offset(layout), iman | (1 << 1));
    let usbcmd = io.read32(layout.command_offset());
    io.write32(layout.command_offset(), usbcmd | (1 << 2));
}

/// Clear the pending-interrupt latches after servicing an IRQ so the controller can raise the next:
/// USBSTS.EINT (bit 3) and IMAN.IP (bit 0), both write-1-to-clear; keep IMAN.IE set.
fn clear_interrupt_pending(io: &mut Mmio, layout: RegisterLayout) {
    io.write32(layout.status_offset(), 1 << 3);
    io.write32(iman_offset(layout), (1 << 1) | (1 << 0));
}

/// Write the 8 software-owned dwords of an xHCI context at a 64-byte-strided offset.
fn write_ctx(va: u64, words: [u32; 8]) {
    let ptr = va as usize as *mut u32;
    for (index, word) in words.iter().enumerate() {
        unsafe { core::ptr::write_volatile(ptr.add(index), *word) };
    }
}

/// Build the 8 software-owned dwords of a Slot Context. Written by hand (rather than via
/// `SlotContext::root_device`) so a hub-downstream device can carry its Route String and the
/// Transaction Translator fields a Full/Low-speed device behind a High-speed hub requires.
/// DW0: Route[19:0] | Speed[23:20] | ContextEntries[31:27]; DW1: RootHubPort[23:16];
/// DW2: TT Hub Slot ID[7:0] | TT Port[15:8]. A root-port device passes route/tt as 0.
fn slot_context_words(
    route: u32,
    speed_raw: u8,
    context_entries: u8,
    root_hub_port: u8,
    tt_hub_slot_id: u8,
    tt_port_number: u8,
) -> [u32; 8] {
    let mut words = [0u32; 8];
    words[0] = (route & 0x000f_ffff)
        | ((speed_raw as u32 & 0xf) << 20)
        | ((context_entries as u32 & 0x1f) << 27);
    words[1] = (root_hub_port as u32) << 16;
    words[2] = (tt_hub_slot_id as u32) | ((tt_port_number as u32) << 8);
    words
}

fn usb_speed(raw: u8) -> UsbSpeed {
    match raw {
        1 => UsbSpeed::Full,
        2 => UsbSpeed::Low,
        3 => UsbSpeed::High,
        5 => UsbSpeed::SuperPlus,
        _ => UsbSpeed::Super,
    }
}

fn log(msg: &[u8]) {
    log_info(msg);
}

fn log_port(index: u8, port: PortStatus) {
    log(b"drv-xhci: port");
    log_hex(index as u64);
    log(b" raw=");
    log_hex(port.raw() as u64);
    log(b" ccs=");
    log_hex(port.connected() as u64);
    log(b" ped=");
    log_hex(port.enabled() as u64);
    log(b" pls=");
    log_hex(port.link_state() as u64);
    log(b" speed=");
    log_hex(port.speed() as u64);
    log(b" pp=");
    log_hex(port.powered() as u64);
    log(b"\n");
}

fn log_layout(layout: RegisterLayout) {
    log(b"drv-xhci: layout op=");
    log_hex(layout.operational_offset() as u64);
    log(b" run=");
    log_hex(layout.runtime_offset() as u64);
    log(b" db=");
    log_hex(layout.doorbell_offset() as u64);
    log(b" crcr=");
    log_hex(layout.command_ring_offset() as u64);
    log(b" dcbaa=");
    log_hex(layout.dcbaa_offset() as u64);
    log(b" ir0_erstsz=");
    log_hex(layout.interrupter0_erst_size_offset() as u64);
    log(b"\n");
}

fn log_controller_status(status: ControllerStatus) {
    log(b"drv-xhci: status usbcmd=");
    log_hex(status.raw_command() as u64);
    log(b" usbsts=");
    log_hex(status.raw_status() as u64);
    log(b" pagesize=");
    log_hex(status.raw_page_size() as u64);
    log(b" run=");
    log_hex(status.running() as u64);
    log(b" hcrst=");
    log_hex(status.reset_requested() as u64);
    log(b" hch=");
    log_hex(status.halted() as u64);
    log(b" cnr=");
    log_hex(status.controller_not_ready() as u64);
    log(b" hse=");
    log_hex(status.host_system_error() as u64);
    log(b" page4k=");
    log_hex(status.supports_4k_pages() as u64);
    log(b" ring_ready=");
    log_hex(status.ring_programming_ready() as u64);
    log(b"\n");
}

fn log_hex(mut value: u64) {
    log_info(b"0x");
    let mut digits = [0u8; 16];
    let mut start = digits.len();
    loop {
        start -= 1;
        let digit = (value & 0xf) as u8;
        digits[start] = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        };
        value >>= 4;
        if value == 0 {
            break;
        }
    }
    log_info(&digits[start..]);
}

fn read32(offset: usize) -> u32 {
    unsafe { core::ptr::read_volatile((MMIO_VA as usize + offset) as *const u32) }
}

fn write32(offset: usize, value: u32) {
    unsafe { core::ptr::write_volatile((MMIO_VA as usize + offset) as *mut u32, value) }
}

fn align_up(value: u64, align: u64) -> Option<u64> {
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
}
