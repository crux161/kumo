#![no_std]
#![no_main]

//j426
//j430
//j431
//j481

use core::sync::atomic::{AtomicU32, Ordering};

use kumo_abi::{Handle, VmarFlags};
use kumo_rt::{
    channel_read_with_handle, channel_write, clock_get, debug_write, interrupt_complete,
    interrupt_create, interrupt_wait, port_bind, port_create, port_wait, process_exit,
    resource_mint_mmio, vmar_map, vmo_create_contiguous,
};
use kumo_xhci::{
    endpoint_context_index, portsc_offset, prepare_noop_command_image, CapabilityRegisters,
    ContextSize, ControllerStatus, EndpointContext, Event, EventRingSegmentTableEntry,
    InputControlContext, NoOpCommandCompletion, NoOpCommandImage, NoOpCommandIovas, NormalTransfer,
    PortStatus, RegisterIo, RegisterLayout, SetupTransferType, SlotContext, TransferRing, Trb,
    UsbSpeed, XhciProbeConfig, XHCI_NO_STREAM_ID, XHCI_PROBE_CONFIG_LEN,
};

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

// Slice C: a second contiguous DMA page holds the enumeration structures for the device on the
// first connected+enabled port. 64-byte context stride (caps ctx=0x40).
const ENUM_VA: u64 = 0x0000_0000_1300_0000;
const ENUM_LEN: u64 = PAGE_SIZE;
const OFF_INPUT_CTX: usize = 0x000; // input-control(64) + slot(64) + EP0(64) = 192 B
const OFF_OUTPUT_CTX: usize = 0x400; // controller-written output device context (2 KiB window)
const OFF_EP0_RING: usize = 0xc00; // 16-TRB EP0 default-control transfer ring (256 B)
const OFF_DATA: usize = 0xd00; // control-transfer data buffer (256 B)
const OFF_INT_RING: usize = 0xe00; // 16-TRB interrupt-IN transfer ring (256 B)
const OFF_REPORT: usize = 0xf00; // HID report buffer (256 B)
const EP0_RING_TRBS: usize = 16;
const INT_RING_TRBS: usize = 16;
/// Reports to log in the Slice-D2 read window, and how long to wait for each before giving up.
const KEYBOARD_REPORT_LIMIT: u32 = 24;
const REPORT_WAIT_ROUNDS: u32 = 240;
const DEVICE_DESCRIPTOR_LEN: u32 = 18;
const DATA_BUF_LEN: usize = 256; // control-transfer buffer (holds a full config descriptor)
/// Bounded spins waiting for the reset device's port to re-link and enable after HCRST.
const PORT_POLL_LIMIT: u32 = 4_000_000;

// U2: a hub-downstream device gets its own page, laid out exactly like the root device's so the
// same enumeration/HID helpers serve both (they take `base_va` + phys).
const DEV2_VA: u64 = 0x0000_0000_1400_0000;
/// Hub class feature selectors (USB 2.0 §11.24.2) and port-status bits used by the reach-through.
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

/// The device Resource handle and GIC IRQ this controller was granted, stashed from the bootstrap
/// so the keyboard path can bind the interrupt (single-threaded driver — plain atomics suffice).
static RESOURCE_HANDLE: AtomicU32 = AtomicU32::new(0);
static DEVICE_IRQ: AtomicU32 = AtomicU32::new(0);
/// The keyboard-channel writer Sora handed us (U5); 0 = none (this controller delivers to the
/// serial log instead). Decoded key bytes are written here → Sora → the shell.
static KEYBOARD_CHANNEL: AtomicU32 = AtomicU32::new(0);
/// Sora's tag byte preceding the keyboard-channel writer in the bootstrap.
const KEYBOARD_BOOTSTRAP_TAG: u8 = b'k';

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

    // Optional second bootstrap message: the keyboard-channel writer for delivering decoded
    // keystrokes to the shell (U5). Absent for controllers Sora didn't grant one — the read then
    // returns empty once Sora closes the bootstrap sender.
    let mut kbd_tag = [0u8; 1];
    let (tag_len, kbd_raw) =
        channel_read_with_handle(bootstrap, kbd_tag.as_mut_ptr(), kbd_tag.len());
    if tag_len == 1 && kbd_tag[0] == KEYBOARD_BOOTSTRAP_TAG && kbd_raw != 0 && kbd_raw != u64::MAX {
        KEYBOARD_CHANNEL.store(kbd_raw as u32, Ordering::Relaxed);
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

    // Slices/refs into the uncached DMA page. Stores land in DRAM for the controller to fetch.
    let dcbaa = unsafe {
        core::slice::from_raw_parts_mut((DMA_VA as usize + OFF_DCBAA) as *mut u64, DCBAA_ENTRIES)
    };
    let cmd =
        unsafe { core::slice::from_raw_parts_mut((DMA_VA as usize + OFF_CMD) as *mut Trb, RING_TRBS) };
    let evt =
        unsafe { core::slice::from_raw_parts_mut((DMA_VA as usize + OFF_EVT) as *mut Trb, RING_TRBS) };
    let erst =
        unsafe { &mut *((DMA_VA as usize + OFF_ERST) as *mut EventRingSegmentTableEntry) };

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
        &mut *evt,
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

    let evt_view: &[Trb] = evt;
    let mut completion: Option<NoOpCommandCompletion> = None;
    let mut decode_error = false;
    for _ in 0..EVENT_POLL_LIMIT {
        dsb();
        match image.poll_completion(evt_view) {
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
            if done.completion_code == 1 {
                enumerate(layout, caplength, max_ports, &mut image, cmd, evt);
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
    cmd: &mut [Trb],
    evt: &mut [Trb],
) {
    // A second contiguous DMA page for the input/output device contexts, the EP0 control ring,
    // and the descriptor buffer. phys == IOVA under SMMU bypass.
    let (enum_raw, enum_phys) = vmo_create_contiguous(ENUM_LEN);
    if enum_raw == u64::MAX || enum_phys == 0 {
        log(b"drv-xhci: enum vmo fail\n");
        return;
    }
    if vmar_map(
        Handle(0),
        Handle(enum_raw as u32),
        0,
        ENUM_VA,
        ENUM_LEN,
        (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
    ) != 0
    {
        log(b"drv-xhci: enum map fail\n");
        return;
    }
    log(b"drv-xhci: enum phys=");
    log_hex(enum_phys);
    log(b"\n");

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

    // --- Enable Slot ---
    let enable = match Trb::enable_slot(0) {
        Ok(trb) => trb,
        Err(_) => {
            log(b"drv-xhci: enable build fail\n");
            return;
        }
    };
    let slot_id = match submit_command(&mut io, layout, &mut *image, &mut *cmd, &*evt, enable) {
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
    let mps0 = ep0_max_packet(speed_raw);
    let ep0_ring_phys = enum_phys + OFF_EP0_RING as u64;
    let input_ctx_phys = enum_phys + OFF_INPUT_CTX as u64;
    let output_ctx_phys = enum_phys + OFF_OUTPUT_CTX as u64;

    let ep0_seg = unsafe {
        core::slice::from_raw_parts_mut((ENUM_VA as usize + OFF_EP0_RING) as *mut Trb, EP0_RING_TRBS)
    };
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
    write_ctx(ENUM_VA + OFF_INPUT_CTX as u64, icc.words());
    write_ctx(ENUM_VA + OFF_INPUT_CTX as u64 + slot_off as u64, slot_ctx.words());
    write_ctx(ENUM_VA + OFF_INPUT_CTX as u64 + ep0_off as u64, ep0_ctx.words());

    // DCBAA[slot_id] → output device context (in the first DMA page's DCBAA array).
    let dcbaa = unsafe {
        core::slice::from_raw_parts_mut((DMA_VA as usize + OFF_DCBAA) as *mut u64, DCBAA_ENTRIES)
    };
    dcbaa[slot_id as usize] = output_ctx_phys;
    dsb();

    // --- Address Device ---
    let addr = match Trb::address_device(input_ctx_phys, slot_id, false) {
        Ok(trb) => trb,
        Err(_) => {
            log(b"drv-xhci: addr build fail\n");
            return;
        }
    };
    match submit_command(&mut io, layout, &mut *image, &mut *cmd, &*evt, addr) {
        Some((1, _, _)) => log(b"drv-xhci: addr dev cc=0x1 ok\n"),
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

    let data_phys = enum_phys + OFF_DATA as u64;
    let data_buf = unsafe {
        core::slice::from_raw_parts_mut((ENUM_VA as usize + OFF_DATA) as *mut u8, DATA_BUF_LEN)
    };

    // --- GET_DESCRIPTOR(Device) over EP0, two-phase for the Full-speed bMaxPacketSize0 ---
    // Phase 1: read the first 8 bytes at the default EP0 mps (8). A single 8-byte packet cannot
    // babble regardless of the device's real EP0 size; byte 7 is bMaxPacketSize0.
    data_buf.fill(0);
    dsb();
    let dev8_setup = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 8, 0x00];
    match control_in(
        &mut io, layout, &mut *image, &*evt, &mut ep0_ring, &mut *ep0_seg, slot_id, dev8_setup,
        data_phys, 8,
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
    let real_mps0 = data_buf[7] as u16;

    // Phase 2: if EP0's real max packet size differs from the default 8, Evaluate Context to
    // update it — otherwise a full 18-byte read babbles (metal: cc=0x3) on a 64-byte EP0.
    if real_mps0 != 0 && real_mps0 != mps0 {
        let mut eval_icc = InputControlContext::default();
        let _ = eval_icc.add(1); // EP0 (DCI 1) only
        let Ok(eval_ep0) = EndpointContext::control(real_mps0, ep0_ring_phys) else {
            log(b"drv-xhci: eval ep0 ctx fail\n");
            return;
        };
        write_ctx(ENUM_VA + OFF_INPUT_CTX as u64, eval_icc.words());
        write_ctx(ENUM_VA + OFF_INPUT_CTX as u64 + ep0_off as u64, eval_ep0.words());
        dsb();
        const TRB_TYPE_EVALUATE_CONTEXT: u32 = 13;
        let eval = Trb::from_words([
            input_ctx_phys as u32,
            (input_ctx_phys >> 32) as u32,
            0,
            (TRB_TYPE_EVALUATE_CONTEXT << 10) | ((slot_id as u32) << 24),
        ]);
        match submit_command(&mut io, layout, &mut *image, &mut *cmd, &*evt, eval) {
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
    data_buf.fill(0);
    dsb();
    let dev_setup = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, DEVICE_DESCRIPTOR_LEN as u8, 0x00];
    match control_in(
        &mut io, layout, &mut *image, &*evt, &mut ep0_ring, &mut *ep0_seg, slot_id, dev_setup,
        data_phys, DEVICE_DESCRIPTOR_LEN as u16,
    ) {
        Some((cc, remaining)) => {
            dsb();
            log(b"drv-xhci: desc cc=");
            log_hex(cc as u64);
            log(b" rem=");
            log_hex(remaining as u64);
            log(b" len=");
            log_hex(data_buf[0] as u64);
            log(b" type=");
            log_hex(data_buf[1] as u64);
            log(b" class=");
            log_hex(data_buf[4] as u64);
            log(b" mps0=");
            log_hex(data_buf[7] as u64);
            log(b" vid=");
            log_hex(u16::from_le_bytes([data_buf[8], data_buf[9]]) as u64);
            log(b" pid=");
            log_hex(u16::from_le_bytes([data_buf[10], data_buf[11]]) as u64);
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
    let device_class = data_buf[4];

    // --- Slice D: read the configuration descriptor and find the boot-HID interface ---
    // First read the 9-byte header to learn wTotalLength.
    data_buf.fill(0);
    dsb();
    let cfg_header_setup = [0x80, 0x06, 0x00, 0x02, 0x00, 0x00, 9, 0x00];
    if control_in(
        &mut io,
        layout,
        &mut *image,
        &*evt,
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
    let total_length = u16::from_le_bytes([data_buf[2], data_buf[3]]);
    let num_interfaces = data_buf[4];
    log(b"drv-xhci: cfg total=");
    log_hex(total_length as u64);
    log(b" numif=");
    log_hex(num_interfaces as u64);
    log(b"\n");

    // Then read the full configuration (interfaces + endpoints), capped to the buffer.
    let read_len = total_length.min(DATA_BUF_LEN as u16);
    data_buf.fill(0);
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
        &mut io,
        layout,
        &mut *image,
        &*evt,
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

    // bConfigurationValue is byte 5 of the configuration descriptor — capture before data_buf is
    // reused by later control transfers.
    let config_value = data_buf[5];

    // A hub (bDeviceClass 9) gets the hub bring-up path (U1); anything else is checked for a boot
    // HID interface.
    if device_class == kumo_hid::USB_CLASS_HUB {
        let hub_int = kumo_hid::find_interrupt_in_endpoint(
            &data_buf[..read_len as usize],
            kumo_hid::USB_CLASS_HUB,
        );
        configure_and_read_hub(
            &mut io,
            layout,
            &mut *image,
            &mut *cmd,
            &*evt,
            &mut ep0_ring,
            &mut *ep0_seg,
            slot_id,
            enum_phys,
            speed_raw,
            port_number,
            config_value,
            data_buf,
            data_phys,
            hub_int,
        );
        return;
    }

    match kumo_hid::find_hid_boot_interface(&data_buf[..read_len as usize]) {
        Some(hid) => {
            log(b"drv-xhci: hid if=");
            log_hex(hid.interface_number as u64);
            log(b" proto=");
            log_hex(hid.protocol as u64);
            log(b" ep=");
            log_hex(hid.in_endpoint_address as u64);
            log(b" mps=");
            log_hex(hid.max_packet_size as u64);
            log(b" interval=");
            log_hex(hid.interval as u64);
            log(b"\n");
            if hid.is_keyboard() {
                configure_and_read_keyboard(
                    &mut io,
                    layout,
                    &mut *image,
                    &mut *cmd,
                    &*evt,
                    &mut ep0_ring,
                    &mut *ep0_seg,
                    slot_id,
                    ENUM_VA,
                    enum_phys,
                    speed_raw,
                    slot_context_words(0, speed_raw, 1, port_number, 0, 0),
                    config_value,
                    hid,
                );
            }
        }
        None => log(b"drv-xhci: no hid boot iface\n"),
    }
}

/// Slice D2: bring a boot keyboard online — SET_CONFIGURATION, Configure Endpoint for its
/// interrupt-IN endpoint, SET_PROTOCOL(boot) — then arm the interrupt ring and log the raw HID
/// reports as keys are pressed. Bounded so it logs a burst and exits rather than pinning the CPU.
#[allow(clippy::too_many_arguments)]
fn configure_and_read_keyboard(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    cmd: &mut [Trb],
    evt: &[Trb],
    ep0_ring: &mut TransferRing,
    ep0_seg: &mut [Trb],
    slot_id: u8,
    base_va: u64,
    enum_phys: u64,
    speed_raw: u8,
    slot_words: [u32; 8],
    config_value: u8,
    hid: kumo_hid::HidBootInterface,
) {
    let int_ring_phys = enum_phys + OFF_INT_RING as u64;
    let report_phys = enum_phys + OFF_REPORT as u64;
    let mps = hid.max_packet_size;

    // --- SET_CONFIGURATION(config_value) --- bmRequestType=0, bRequest=9, wValue=config, no data.
    let set_config = [0x00, 0x09, config_value, 0x00, 0x00, 0x00, 0x00, 0x00];
    match control_no_data(io, layout, image, evt, ep0_ring, &mut *ep0_seg, slot_id, set_config) {
        Some((1, _)) => log(b"drv-xhci: set config cc=0x1\n"),
        Some((cc, _)) => return log_cc(b"drv-xhci: set config cc=", cc),
        None => return log(b"drv-xhci: set config timeout\n"),
    }

    // --- Configure Endpoint for the keyboard's interrupt-IN endpoint ---
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
        hid.in_endpoint_address,
        hid.max_packet_size,
        hid.interval,
        speed_raw,
    ) else {
        return;
    };
    log(b"drv-xhci: config ep cc=0x1\n");

    // --- SET_PROTOCOL(boot) --- HID class request to the interface: bmRequestType=0x21,
    // bRequest=0x0B, wValue=0 (boot), wIndex=interface, no data.
    let set_proto = [0x21, 0x0b, 0x00, 0x00, hid.interface_number, 0x00, 0x00, 0x00];
    match control_no_data(io, layout, image, evt, ep0_ring, &mut *ep0_seg, slot_id, set_proto) {
        Some((1, _)) => log(b"drv-xhci: set proto cc=0x1\n"),
        Some((cc, _)) => return log_cc(b"drv-xhci: set proto cc=", cc),
        None => return log(b"drv-xhci: set proto timeout\n"),
    }

    // --- Interrupt transfer ring + bounded report-read window ---
    let int_seg = unsafe {
        core::slice::from_raw_parts_mut((base_va as usize + OFF_INT_RING) as *mut Trb, INT_RING_TRBS)
    };
    let Ok(mut int_ring) = TransferRing::new(&mut *int_seg, int_ring_phys) else {
        return log(b"drv-xhci: int ring fail\n");
    };
    let report_buf =
        unsafe { core::slice::from_raw_parts_mut((base_va as usize + OFF_REPORT) as *mut u8, 8) };

    let mut decoder = kumo_hid::Decoder::new();

    // U4: IRQ-driven persistent path. A busy-poll at our priority (63) would starve Sora (64), so
    // if this controller was granted its IRQ, enable the controller interrupter and block on the
    // xHCI interrupt between reports — the driver stays resident and services the keyboard forever.
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
            log(b"drv-xhci: keyboard irq-driven port=");
            log_hex(irq_port.is_some() as u64);
            log(b"; type at the shell\n");
            // Exactly ONE transfer may be outstanding at a time. The previous shape enqueued a
            // TRB on every iteration but only reclaimed one when a MATCHING transfer event was
            // found, so every wake without a match (spurious interrupt, port-change event) leaked
            // a ring slot. After ~15 the 16-entry ring was full, `enqueue` failed, and the driver
            // RETURNED — running `quiesce_controller` and killing the keyboard for good. That is
            // the observed "window": input works for a while, then stops accepting keystrokes
            // entirely. Re-arm only once the outstanding transfer has actually completed.
            let mut pending = false;
            loop {
                if !pending {
                    report_buf.fill(0);
                    dsb();
                    let Ok(normal) =
                        Trb::normal_transfer(NormalTransfer::interrupt_in(report_phys, mps as u32))
                    else {
                        return log(b"drv-xhci: normal trb fail\n");
                    };
                    if int_ring.enqueue(&mut *int_seg, normal).is_err() {
                        // Never exit on a full ring: exiting quiesces the controller and the
                        // keyboard never comes back. Drain what the controller has finished and
                        // retry on the next interrupt instead.
                        log(b"drv-xhci: int ring full; draining\n");
                    } else {
                        pending = true;
                        dsb();
                        ring_slot_doorbell(io, layout, slot_id, dci);
                    }
                }
                // Block until the controller raises its interrupt (a report completed).
                match irq_port {
                    Some(port) => {
                        port_wait(port);
                        interrupt_wait(interrupt);
                    }
                    None => {
                        interrupt_wait(interrupt);
                    }
                }
                clear_interrupt_pending(io, layout);
                // ERDP must be re-written on EVERY serviced interrupt, not only when a matching
                // transfer event is found: the write clears EHB (Event Handler Busy), and the
                // controller will not raise another interrupt while EHB is set. Boot showed 12
                // interrupts for 5 keys — each unmatched one (port-change/spurious) previously
                // skipped the ERDP write, and after the last one the controller went silent for
                // good, which is exactly why input died once the system went idle.
                write_erdp(io, layout, image.event_ring.dequeue_iova());
                if let Some((_cc, _, trb_iova)) = poll_transfer_event_limited(
                    io,
                    layout,
                    &mut *image,
                    evt,
                    slot_id,
                    IRQ_EVENT_DRAIN_LIMIT,
                ) {
                    let _ = int_ring.reclaim_through(trb_iova);
                    pending = false;
                    dsb();
                    let report: [u8; kumo_hid::REPORT_BYTES] = [
                        report_buf[0],
                        report_buf[1],
                        report_buf[2],
                        report_buf[3],
                        report_buf[4],
                        report_buf[5],
                        report_buf[6],
                        report_buf[7],
                    ];
                    if let Ok(events) = decoder.decode(report) {
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
                }
                interrupt_complete(interrupt);
            }
        }
        log(b"drv-xhci: interrupt create fail; polling\n");
    }

    // No usable interrupt: refuse to serve rather than busy-poll. This driver runs ABOVE Sora, so
    // the polling loop below spins hard enough to starve the whole system — including a sibling
    // controller's working keyboard (observed: enabling more device slots let a hub-downstream
    // keyboard reach this path on an IRQ-less controller, and every port went dead). Every
    // controller is granted its GIC line now, so reaching here means the grant or bind failed, and
    // saying so is far more useful than freezing the machine.
    if DEVICE_IRQ.load(Ordering::Relaxed) == 0 {
        return log(b"drv-xhci: keyboard found but no irq granted; not serving\n");
    }

    // Bounded busy-poll fallback (interrupt object present but unusable).
    log(b"drv-xhci: keyboard armed; press keys\n");
    let mut received = 0u32;
    while received < KEYBOARD_REPORT_LIMIT {
        report_buf.fill(0);
        dsb();
        let Ok(normal) = Trb::normal_transfer(NormalTransfer::interrupt_in(report_phys, mps as u32))
        else {
            return log(b"drv-xhci: normal trb fail\n");
        };
        if int_ring.enqueue(&mut *int_seg, normal).is_err() {
            return log(b"drv-xhci: int enqueue fail\n");
        }
        dsb();
        ring_slot_doorbell(io, layout, slot_id, dci);

        let mut got = None;
        for _ in 0..REPORT_WAIT_ROUNDS {
            if let Some(result) = poll_transfer_event_limited(
                io,
                layout,
                &mut *image,
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
                log(b"drv-xhci: report cc=");
                log_hex(cc as u64);
                log(b" ");
                for &byte in report_buf.iter() {
                    log_hex(byte as u64);
                    log(b" ");
                }
                // Decode the 8-byte boot report into key edges and print each pressed key's
                // character — the same kumo-hid decoder that will feed the shell in U5.
                let report: [u8; kumo_hid::REPORT_BYTES] = [
                    report_buf[0],
                    report_buf[1],
                    report_buf[2],
                    report_buf[3],
                    report_buf[4],
                    report_buf[5],
                    report_buf[6],
                    report_buf[7],
                ];
                if let Ok(events) = decoder.decode(report) {
                    for event in events.as_slice() {
                        if event.state == kumo_hid::KeyState::Pressed {
                            for &byte in event.symbol.terminal_bytes().as_slice() {
                                if (0x20..0x7f).contains(&byte) {
                                    log(b" '");
                                    debug_write(&byte as *const u8, 1);
                                    log(b"'");
                                }
                            }
                        }
                    }
                }
                log(b"\n");
                received += 1;
            }
            None => {
                log(b"drv-xhci: report idle\n");
                break;
            }
        }
    }
    log(b"drv-xhci: keyboard read done\n");
}

/// Issue a no-data control transfer over EP0 (Setup + IN Status), for SET_CONFIGURATION and
/// SET_PROTOCOL. Returns `(completion_code, remaining)`.
#[allow(clippy::too_many_arguments)]
fn control_no_data(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    evt: &[Trb],
    ep0_ring: &mut TransferRing,
    ep0_seg: &mut [Trb],
    slot_id: u8,
    setup_packet: [u8; 8],
) -> Option<(u8, u32)> {
    let setup = Trb::setup_stage(setup_packet, SetupTransferType::NoData, 0).ok()?;
    // A no-data control transfer's Status stage is IN, with IOC.
    let status = Trb::status_stage(0, true, true).ok()?;
    if ep0_ring.enqueue(&mut *ep0_seg, setup).is_err()
        || ep0_ring.enqueue(&mut *ep0_seg, status).is_err()
    {
        return None;
    }
    dsb();
    ring_slot_doorbell(
        io,
        layout,
        slot_id,
        endpoint_context_index(0, false).unwrap_or(1),
    );
    let (cc, remaining, trb_iova) = poll_transfer_event(io, layout, image, evt, slot_id)?;
    let _ = ep0_ring.reclaim_through(trb_iova);
    Some((cc, remaining))
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

/// Read a device's descriptors over its EP0 and locate a boot-HID interface. Handles the
/// Full-speed `bMaxPacketSize0` dance (8-byte probe → Evaluate Context → full read), then walks
/// the configuration descriptor. Returns `(bConfigurationValue, interface)`. Shared by the
/// root-port and hub-downstream paths, which differ only in which DMA page they operate on.
#[allow(clippy::too_many_arguments)]
fn read_descriptors_and_find_hid(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    cmd: &mut [Trb],
    evt: &[Trb],
    ep0_ring: &mut TransferRing,
    ep0_seg: &mut [Trb],
    slot_id: u8,
    base_va: u64,
    base_phys: u64,
    mps0: u16,
    data_buf: &mut [u8],
) -> Option<(u8, kumo_hid::HidBootInterface)> {
    let data_phys = base_phys + OFF_DATA as u64;
    let ep0_ring_phys = base_phys + OFF_EP0_RING as u64;
    let input_ctx_phys = base_phys + OFF_INPUT_CTX as u64;

    // Phase 1: 8-byte device descriptor at the default EP0 packet size.
    data_buf.fill(0);
    dsb();
    let dev8 = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 8, 0x00];
    match control_in(io, layout, image, evt, ep0_ring, &mut *ep0_seg, slot_id, dev8, data_phys, 8) {
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
    let real_mps0 = data_buf[7] as u16;

    // Phase 2: correct EP0's max packet size if the device disagrees with our default.
    if real_mps0 != 0 && real_mps0 != mps0 {
        let mut eval_icc = InputControlContext::default();
        eval_icc.add(1).ok()?;
        let eval_ep0 = EndpointContext::control(real_mps0, ep0_ring_phys).ok()?;
        let ep0_off = ContextSize::Bytes64.input_offset(1)?;
        write_ctx(base_va + OFF_INPUT_CTX as u64, eval_icc.words());
        write_ctx(base_va + OFF_INPUT_CTX as u64 + ep0_off as u64, eval_ep0.words());
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
    data_buf.fill(0);
    dsb();
    let dev_full = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, DEVICE_DESCRIPTOR_LEN as u8, 0x00];
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
            log_hex(u16::from_le_bytes([data_buf[8], data_buf[9]]) as u64);
            log(b" pid=");
            log_hex(u16::from_le_bytes([data_buf[10], data_buf[11]]) as u64);
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

    // Configuration descriptor: header for wTotalLength, then the whole thing.
    data_buf.fill(0);
    dsb();
    let cfg_head = [0x80, 0x06, 0x00, 0x02, 0x00, 0x00, 9, 0x00];
    control_in(io, layout, image, evt, ep0_ring, &mut *ep0_seg, slot_id, cfg_head, data_phys, 9)?;
    dsb();
    let total = u16::from_le_bytes([data_buf[2], data_buf[3]]).min(DATA_BUF_LEN as u16);
    data_buf.fill(0);
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
    control_in(io, layout, image, evt, ep0_ring, &mut *ep0_seg, slot_id, cfg, data_phys, total)?;
    dsb();
    let config_value = data_buf[5];
    match kumo_hid::find_hid_boot_interface(&data_buf[..total as usize]) {
        Some(hid) => {
            log(b"drv-xhci: dev hid if=");
            log_hex(hid.interface_number as u64);
            log(b" proto=");
            log_hex(hid.protocol as u64);
            log(b" ep=");
            log_hex(hid.in_endpoint_address as u64);
            log(b"\n");
            Some((config_value, hid))
        }
        None => {
            log(b"drv-xhci: dev no hid boot iface\n");
            None
        }
    }
}

/// Issue a Configure Endpoint command adding one interrupt-IN endpoint (and bumping the slot's
/// Context Entries) at its DCI. The endpoint's transfer ring is `OFF_INT_RING`. Returns the DCI on
/// success, logging the completion code on failure. Shared by the keyboard and hub paths.
#[allow(clippy::too_many_arguments)]
fn configure_interrupt_endpoint(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    cmd: &mut [Trb],
    evt: &[Trb],
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
    let int_ctx =
        EndpointContext::interrupt_in(max_packet_size, interval, int_ring_phys, max_packet_size.max(1))
            .ok()?;
    let stride = ContextSize::Bytes64;
    let slot_off = stride.input_offset(0)?;
    let ep_off = stride.input_offset(dci)?;
    write_ctx(base_va + OFF_INPUT_CTX as u64, icc.words());
    write_ctx(base_va + OFF_INPUT_CTX as u64 + slot_off as u64, slot);
    write_ctx(base_va + OFF_INPUT_CTX as u64 + ep_off as u64, int_ctx.words());
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
    cmd: &mut [Trb],
    evt: &[Trb],
    ep0_ring: &mut TransferRing,
    ep0_seg: &mut [Trb],
    slot_id: u8,
    enum_phys: u64,
    speed_raw: u8,
    port_number: u8,
    config_value: u8,
    data_buf: &mut [u8],
    data_phys: u64,
    hub_int: Option<kumo_hid::InterruptInEndpoint>,
) {
    // --- GET_DESCRIPTOR(Hub) --- class request; descriptor type 0x2A for SuperSpeed hubs, else
    // 0x29. bNbrPorts is byte 2.
    let hub_desc_type = if speed_raw >= 4 { 0x2a } else { 0x29 };
    data_buf.fill(0);
    dsb();
    let hub_setup = [0xa0, 0x06, 0x00, hub_desc_type, 0x00, 0x00, 16, 0x00];
    match control_in(io, layout, image, evt, ep0_ring, &mut *ep0_seg, slot_id, hub_setup, data_phys, 16)
    {
        Some((1, _)) => {}
        Some((cc, _)) => return log_cc(b"drv-xhci: hub desc cc=", cc),
        None => return log(b"drv-xhci: hub desc timeout\n"),
    }
    dsb();
    let nbr_ports = data_buf[2];
    log(b"drv-xhci: hub ports=");
    log_hex(nbr_ports as u64);
    log(b"\n");
    // Everything below is one bounded pass: a driver child cannot be preempted, so the hub work
    // must promise an upper bound on how long it holds the CPU away from a live keyboard driver.
    let pass = Deadline::after(HUB_PASS_BUDGET_NS);

    // --- SET_CONFIGURATION ---
    let set_config = [0x00, 0x09, config_value, 0x00, 0x00, 0x00, 0x00, 0x00];
    match control_no_data(io, layout, image, evt, ep0_ring, &mut *ep0_seg, slot_id, set_config) {
        Some((1, _)) => log(b"drv-xhci: hub set config cc=0x1\n"),
        Some((cc, _)) => return log_cc(b"drv-xhci: hub set config cc=", cc),
        None => return log(b"drv-xhci: hub set config timeout\n"),
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
            ENUM_VA,
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
        let _ = control_no_data(io, layout, image, evt, ep0_ring, &mut *ep0_seg, slot_id, set_power);
        port += 1;
    }
    // Give the ports their power-on-to-power-good settle time before reading status. Kept short
    // for the same non-preemptible reason as the polls above.
    spin_delay(500_000);

    // --- Read one status-change report from the hub's interrupt endpoint ---
    if let Some(dci) = hub_dci {
        let report_phys = enum_phys + OFF_REPORT as u64;
        let int_ring_base = enum_phys + OFF_INT_RING as u64;
        let int_seg = unsafe {
            core::slice::from_raw_parts_mut(
                (ENUM_VA as usize + OFF_INT_RING) as *mut Trb,
                INT_RING_TRBS,
            )
        };
        if let Ok(mut int_ring) = TransferRing::new(&mut *int_seg, int_ring_base) {
            let report_buf = unsafe {
                core::slice::from_raw_parts_mut((ENUM_VA as usize + OFF_REPORT) as *mut u8, 4)
            };
            report_buf.fill(0);
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
                            for &byte in report_buf.iter() {
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

    // --- GET_STATUS each downstream port; enumerate through the first that holds a device ---
    let mut port = 1u8;
    let mut connected_port = None;
    while port <= nbr_ports {
        if pass.expired() {
            log(b"drv-xhci: hub pass budget spent (scan)\n");
            break;
        }
        data_buf.fill(0);
        dsb();
        let get_status = [0xa3, 0x00, 0x00, 0x00, port, 0x00, 0x04, 0x00];
        match control_in(
            io, layout, image, evt, ep0_ring, &mut *ep0_seg, slot_id, get_status, data_phys, 4,
        ) {
            Some((1, _)) => {
                dsb();
                let status = u16::from_le_bytes([data_buf[0], data_buf[1]]);
                log(b"drv-xhci: hub port");
                log_hex(port as u64);
                log(b" status=");
                log_hex(status as u64);
                log(b" conn=");
                log_hex((status & 1) as u64);
                log(b"\n");
                if status & 1 != 0 && connected_port.is_none() {
                    connected_port = Some(port);
                }
            }
            Some((cc, _)) => log_cc(b"drv-xhci: hub port status cc=", cc),
            None => log(b"drv-xhci: hub port status timeout\n"),
        }
        port += 1;
    }
    log(b"drv-xhci: hub scan done\n");

    // U2: reach through the hub to the device on that port.
    if let Some(down_port) = connected_port {
        if pass.expired() {
            log(b"drv-xhci: hub pass budget spent; downstream deferred\n");
        } else {
            enumerate_hub_downstream(
                io, layout, image, cmd, evt, ep0_ring, ep0_seg, slot_id, port_number, speed_raw,
                data_phys, data_buf, down_port, pass,
            );
        }
    }
}

/// Slice U2: enumerate a device attached to `down_port` of an already-configured hub. Resets the
/// downstream port through hub class requests, then addresses the device with a **route string**
/// (and, for a Full/Low-speed device behind a High-speed hub, the Transaction Translator fields)
/// so the controller can reach it through the hub — multi-tier addressing. On finding a boot
/// keyboard it hands off to the same HID path the root-port device uses.
#[allow(clippy::too_many_arguments)]
fn enumerate_hub_downstream(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    cmd: &mut [Trb],
    evt: &[Trb],
    hub_ep0_ring: &mut TransferRing,
    hub_ep0_seg: &mut [Trb],
    hub_slot_id: u8,
    hub_root_port: u8,
    hub_speed_raw: u8,
    hub_data_phys: u64,
    hub_data_buf: &mut [u8],
    down_port: u8,
    pass: Deadline,
) {
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
        io, layout, image, evt, hub_ep0_ring, &mut *hub_ep0_seg, hub_slot_id, set_reset,
    ) {
        Some((1, _)) => {}
        Some((cc, _)) => return log_cc(b"drv-xhci: hub port reset cc=", cc),
        None => return log(b"drv-xhci: hub port reset timeout\n"),
    }

    // Wait for the port to leave reset and report enabled, then read its negotiated speed.
    let mut down_speed = 0u8;
    let mut enabled = false;
    for _ in 0..HUB_RESET_POLL_ROUNDS {
        if pass.expired() {
            return log(b"drv-xhci: hub pass budget spent (reset)\n");
        }
        // Keep each settle short: this driver preempts Sora, so a long spin here stalls the
        // whole system (and a sibling controller's live keyboard) while a port comes up.
        spin_delay(200_000);
        hub_data_buf.fill(0);
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
        let status = u16::from_le_bytes([hub_data_buf[0], hub_data_buf[1]]);
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
        return log(b"drv-xhci: hub port never enabled\n");
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
        io, layout, image, evt, hub_ep0_ring, &mut *hub_ep0_seg, hub_slot_id, clear_change,
    );

    // --- A private DMA page for this device's contexts, EP0 ring and buffers ---
    let (dev_raw, dev_phys) = vmo_create_contiguous(ENUM_LEN);
    if dev_raw == u64::MAX || dev_phys == 0 {
        return log(b"drv-xhci: hub dev vmo fail\n");
    }
    if vmar_map(
        Handle(0),
        Handle(dev_raw as u32),
        0,
        DEV2_VA,
        ENUM_LEN,
        (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
    ) != 0
    {
        return log(b"drv-xhci: hub dev map fail\n");
    }

    // --- Enable a slot for the downstream device ---
    let Ok(enable) = Trb::enable_slot(0) else {
        return log(b"drv-xhci: hub dev enable build fail\n");
    };
    let dev_slot = match submit_command(io, layout, image, cmd, evt, enable) {
        Some((1, slot, _)) => slot,
        Some((cc, _, _)) => return log_cc(b"drv-xhci: hub dev enable cc=", cc),
        None => return log(b"drv-xhci: hub dev enable timeout\n"),
    };
    if dev_slot == 0 || dev_slot as usize >= DCBAA_ENTRIES {
        return log(b"drv-xhci: hub dev slot out of range\n");
    }
    log(b"drv-xhci: hub dev slot=");
    log_hex(dev_slot as u64);
    log(b"\n");

    // --- Input context: route string + TT for a FS/LS device behind a HS hub ---
    let ep0_ring_phys = dev_phys + OFF_EP0_RING as u64;
    let input_ctx_phys = dev_phys + OFF_INPUT_CTX as u64;
    let output_ctx_phys = dev_phys + OFF_OUTPUT_CTX as u64;
    let ep0_seg = unsafe {
        core::slice::from_raw_parts_mut((DEV2_VA as usize + OFF_EP0_RING) as *mut Trb, EP0_RING_TRBS)
    };
    let Ok(mut ep0_ring) = TransferRing::new(&mut *ep0_seg, ep0_ring_phys) else {
        return log(b"drv-xhci: hub dev ep0 ring fail\n");
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
    let mps0 = ep0_max_packet(down_speed);
    let Ok(ep0_ctx) = EndpointContext::control(mps0, ep0_ring_phys) else {
        return log(b"drv-xhci: hub dev ep0 ctx fail\n");
    };
    let mut icc = InputControlContext::default();
    let _ = icc.add(0);
    let _ = icc.add(1);
    let stride = ContextSize::Bytes64;
    let (Some(slot_off), Some(ep0_off)) = (stride.input_offset(0), stride.input_offset(1)) else {
        return log(b"drv-xhci: hub dev ctx offset fail\n");
    };
    write_ctx(DEV2_VA + OFF_INPUT_CTX as u64, icc.words());
    write_ctx(DEV2_VA + OFF_INPUT_CTX as u64 + slot_off as u64, slot_words);
    write_ctx(DEV2_VA + OFF_INPUT_CTX as u64 + ep0_off as u64, ep0_ctx.words());
    let dcbaa = unsafe {
        core::slice::from_raw_parts_mut((DMA_VA as usize + OFF_DCBAA) as *mut u64, DCBAA_ENTRIES)
    };
    dcbaa[dev_slot as usize] = output_ctx_phys;
    dsb();

    // --- Address Device (through the hub) ---
    let Ok(addr) = Trb::address_device(input_ctx_phys, dev_slot, false) else {
        return log(b"drv-xhci: hub dev addr build fail\n");
    };
    match submit_command(io, layout, image, cmd, evt, addr) {
        Some((1, _, _)) => log(b"drv-xhci: hub dev addr cc=0x1 ok\n"),
        Some((cc, _, _)) => return log_cc(b"drv-xhci: hub dev addr cc=", cc),
        None => return log(b"drv-xhci: hub dev addr timeout\n"),
    }

    // --- Descriptors over the downstream device's EP0, then the HID path ---
    let data_phys = dev_phys + OFF_DATA as u64;
    let data_buf = unsafe {
        core::slice::from_raw_parts_mut((DEV2_VA as usize + OFF_DATA) as *mut u8, DATA_BUF_LEN)
    };
    let _ = data_phys;
    let Some((config_value, hid)) = read_descriptors_and_find_hid(
        io,
        layout,
        image,
        cmd,
        evt,
        &mut ep0_ring,
        &mut *ep0_seg,
        dev_slot,
        DEV2_VA,
        dev_phys,
        mps0,
        data_buf,
    ) else {
        return;
    };
    if !hid.is_keyboard() {
        return log(b"drv-xhci: hub dev not a boot keyboard\n");
    }
    configure_and_read_keyboard(
        io,
        layout,
        image,
        cmd,
        evt,
        &mut ep0_ring,
        &mut *ep0_seg,
        dev_slot,
        DEV2_VA,
        dev_phys,
        down_speed,
        slot_words,
        config_value,
        hid,
    );
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
    evt: &[Trb],
    ep0_ring: &mut TransferRing,
    ep0_seg: &mut [Trb],
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
    if ep0_ring.enqueue(&mut *ep0_seg, setup).is_err()
        || ep0_ring.enqueue(&mut *ep0_seg, data).is_err()
        || ep0_ring.enqueue(&mut *ep0_seg, status).is_err()
    {
        return None;
    }
    dsb();
    ring_slot_doorbell(
        io,
        layout,
        slot_id,
        endpoint_context_index(0, false).unwrap_or(1),
    );
    let (cc, remaining, trb_iova) = poll_transfer_event(io, layout, image, evt, slot_id)?;
    // Reclaim the setup/data/status TRBs so the EP0 ring doesn't fill across many transfers.
    let _ = ep0_ring.reclaim_through(trb_iova);
    Some((cc, remaining))
}

/// Read one 32-bit word from an xHCI context in DMA memory (word index, not byte offset).
fn read_ctx_word(va: u64, word_index: usize) -> u32 {
    unsafe { core::ptr::read_volatile((va as usize + word_index * 4) as *const u32) }
}

/// Poll PORTSC for the first connected + enabled port after HCRST re-links it. Returns
/// `(port_number, speed)` (port number is 1-based).
fn wait_for_device_port(caplength: u8, max_ports: u8) -> Option<(u8, u8)> {
    // PORTSC bits. PED (1) reads enabled; writing 1 disables, so mask it out of a modify-write.
    // The change bits (17..=23, RW1C) must also be written 0 so a modify-write doesn't clear them.
    const PORTSC_PED: u32 = 1 << 1;
    const PORTSC_PR: u32 = 1 << 4;
    const PORTSC_CHANGE_BITS: u32 = 0x7f << 17;
    let mut reset_issued = [false; MAX_LOGGED_PORTS as usize];
    for _ in 0..PORT_POLL_LIMIT {
        dsb();
        let mut port = 0u8;
        while port < max_ports {
            if let Some(offset) = portsc_offset(caplength, port) {
                let raw = read32(offset);
                let status = PortStatus::new(raw);
                if status.connected() {
                    if status.enabled() {
                        return Some((port + 1, status.speed()));
                    }
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
    }
    None
}

/// Enqueue one command, ring the command doorbell, and poll the event ring for its Command
/// Completion, skipping unrelated events. Returns `(completion_code, slot_id, parameter)`.
fn submit_command(
    io: &mut Mmio,
    layout: RegisterLayout,
    image: &mut NoOpCommandImage,
    cmd: &mut [Trb],
    evt: &[Trb],
    command: Trb,
) -> Option<(u8, u8, u32)> {
    let token = image.command_ring.enqueue(cmd, command).ok()?;
    dsb();
    layout.ring_command_doorbell(io).ok()?;
    for _ in 0..EVENT_POLL_LIMIT {
        dsb();
        let popped = match image.event_ring.pop(evt) {
            Ok(popped) => popped,
            Err(_) => return None,
        };
        if let Some(trb) = popped {
            if let Some(Event::CommandCompletion {
                command_iova,
                parameter,
                completion_code,
                slot_id,
            }) = trb.decode_event()
            {
                if command_iova == token.iova {
                    let _ = image.command_ring.reclaim_through(command_iova);
                    write_erdp(io, layout, image.event_ring.dequeue_iova());
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
    evt: &[Trb],
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
    evt: &[Trb],
    slot_id: u8,
    limit: u32,
) -> Option<(u8, u32, u64)> {
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
            if let Some(Event::Transfer {
                completion_code,
                remaining,
                slot_id: event_slot,
                trb_iova,
                ..
            }) = trb.decode_event()
            {
                if event_slot == slot_id {
                    write_erdp(io, layout, image.event_ring.dequeue_iova());
                    return Some((completion_code, remaining, trb_iova));
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
fn setup_scratchpad(dma_va: u64, dma_phys: u64, dcbaa: &mut [u64], count: u32) -> bool {
    if count == 0 {
        return true;
    }
    let bytes = count as u64 * PAGE_SIZE;
    let (buffers_raw, buffers_phys) = vmo_create_contiguous(bytes);
    if buffers_raw == u64::MAX || buffers_phys == 0 {
        return false;
    }
    let array = unsafe {
        core::slice::from_raw_parts_mut(
            (dma_va as usize + OFF_SCRATCH_ARRAY) as *mut u64,
            count as usize,
        )
    };
    for (index, entry) in array.iter_mut().enumerate() {
        *entry = buffers_phys + index as u64 * PAGE_SIZE;
    }
    dcbaa[0] = dma_phys + OFF_SCRATCH_ARRAY as u64;
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

/// Default bMaxPacketSize0 for the initial descriptor read, by PORTSC speed.
fn ep0_max_packet(raw: u8) -> u16 {
    match raw {
        1 | 2 => 8,  // Full / Low: 8 until bMaxPacketSize0 is known
        3 => 64,     // High
        _ => 512,    // Super / SuperPlus (fixed)
    }
}

fn log(msg: &[u8]) {
    debug_write(msg.as_ptr(), msg.len());
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
    debug_write(b"0x".as_ptr(), 2);
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
    debug_write(digits[start..].as_ptr(), digits.len() - start);
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
