//j834
//j843
//j835
//j838
//j839
//j840
//! NET4c-3: the ax88's resident RX and status lanes on the one controller owner. It is
//! default-off: without KUMO_AX88_RESIDENT_PROOF the adapter stays unconfigured, as
//! since NET0.
//!
//! One custodied 64 KiB backing (NET4c-1) holds a 32 KiB RX window that crosses no
//! 64 KiB boundary, plus the two rings and the status buffer. Nothing is shared with
//! EP0, HID, storage or the device page. The lanes are armed in the resident loop
//! beside HID and are never waited on. A cycle ends them through NET4c-2's verified
//! stop, deconfigures the endpoints, reads Disabled back, and only then rewrites the
//! rings, before configuring and arming again. `drv_xhci::nic::NicLifecycle` enforces
//! that order; a refused or failed step retains everything. Without NET5b the chip
//! is not initialized, so RX is expected to stay pending; no frame is transmitted.
//! KUMO_AX88_LINK_PROOF (NET5b) reuses these lanes and the proof tick to pace
//! `kumo_ax88::link` on this owner's EP0 instead of the cycling timeline; MAC
//! receive stays off there too. KUMO_AX88_RX_PROOF (NET6) runs the same sequence with
//! the receive goal and tallies every RX aggregate with `kumo_ax88::probe`; the peer's
//! END frame concludes the window. KUMO_AX88_TX_PROOF (NET7a) also configures the bulk
//! OUT lane: numbered frames from a ring and buffer in the same custodied page, each
//! written only after the previous transfer's exact completion. Recovery class:
//! stateless; the custody parent holds the DMA. — TIMBERDOODLE

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use drv_xhci::nic::{completed_length, NicLane, NicLifecycle, NicPhase};
use kumo_abi::{Handle, VmarFlags};
use kumo_ax88::control::{decode_mac, MacRegister, Request};
use kumo_ax88::link::{Goal, Link, Port, StatusView};
use kumo_ax88::probe::{self, Tally, Verdict};
use kumo_ax88::profile::{Profile, Speed};
use kumo_ax88::rx::{Aggregate, Packet};
use kumo_rt::{clock_get, vmar_map};
use kumo_xhci::{
    boundary_window, endpoint_context_index, CompletionStats, ContextSize, ControlPipe, DmaBuf,
    DmaTrbs, EndpointContext, EndpointSnapshot, EndpointState, InputControlContext,
    NoOpCommandImage, NormalTransfer, RegisterLayout, RouteError, RoutedTransfer, SlotSnapshot,
    SlotState, TransferRing, TransferTicket, Trb,
};

use super::ax88_link::{self, Ep0Port};
use super::{
    ax88_record, completion_actors, control_in, control_no_data, create_custodied_dma, dsb,
    endpoint_context_va, enqueue_reserved, quarantine_transfer, quiesce_endpoint, read_ctx_word,
    read_endpoint_snapshot, reserve_transfer, ring_slot_doorbell, submit_command, write_ctx,
    xhci_interval, Mmio, CONTROLLER_BASE, DMA_CUSTODY_SEQUENCE, EP0_RING_TRBS, OFF_DATA,
    OFF_EP0_RING, OFF_INPUT_CTX, OFF_OUTPUT_CTX,
};

pub(super) const ENABLED: bool =
    flag(option_env!("KUMO_AX88_RESIDENT_PROOF")) || ax88_link::ENABLED || ax88_link::RX_ENABLED;
/// NET6 concludes its receive window this long after the peer's first END frame.
const RX_SETTLE_NS: u64 = 2_000_000_000;

const fn flag(value: Option<&str>) -> bool {
    match value {
        Some(value) => matches!(value.as_bytes(), [b'1']),
        None => false,
    }
}

const NIC_DMA_VA: u64 = 0x0000_0000_1700_0000;
const NIC_DMA_LEN: u64 = 0x1_0000;
const NIC_RX_LEN: u64 = kumo_ax88::AGGREGATE_MAX as u64;
const RING_TRBS: usize = 16;
const RX_RING: u64 = 0x000;
const STATUS_RING: u64 = 0x100;
const STATUS_BUF: u64 = 0x200;
/// NET7a: the bulk OUT ring and one frame buffer (8-byte prefix + 1514), in the same
/// page as the other rings, so neither can straddle a 64 KiB boundary.
const TX_RING: u64 = 0x300;
const TX_BUF: u64 = 0x400;
const TX_BUF_LEN: u64 = 0x600;
const RINGS_LEN: u64 = 0xa00;
/// Exact-phase TX frames go one per proof tick from this long after receive turns on;
/// the burst starts later, to overlap the peer's RX phases.
const TX_EXACT_DELAY_NS: u64 = 2_000_000_000;
const TX_BURST_DELAY_NS: u64 = 11_000_000_000;
/// J838 experiment: the exact phase's inter-frame gap. The 10 ms pace puts all 32
/// frames inside one second, separating a time-based transmit wall from a count-based
/// one (J836: every boot transmitted exactly 11 frames).
const TX_EXACT_GAP_NS: u64 = 10_000_000;
/// J839 experiment: ask for the reference TX re-arm every this many completions; the
/// link tick performs it and records the chip's FIFO word.
const TX_REARM_EVERY: u32 = 1;
/// A submitted transfer whose exact completion never arrives must not wedge the proof.
/// Full-duplex load starves OUT completions for tens of seconds (12.2 s and 32.5 s
/// measured at seq 623 on metal), so the bound sits far above that before the schedule
/// stops, once, with the endpoint state recorded.
const TX_COMPLETION_TIMEOUT_NS: u64 = 120_000_000_000;
const STATUS_LEN: u32 = kumo_ax88::status::STATUS_LEN as u32;

/// Proof timeline, one tick per 100 ms: thirty idle ticks with both lanes pending and one
/// exact EP0 read each; twenty full stop/disable/reclaim/start cycles, one per tick; then
/// thirty settled idle ticks like the first. Each window records the loop's own longest
/// busy stretch per wake apart from how late its ticks and HID reports arrived, so a
/// stall inside this owner is told apart from the CPU being taken by another process.
pub(super) const TICK_NS: u64 = 100_000_000;
const IDLE_TICKS: u32 = 30;
const CYCLES: u32 = 20;
const SETTLED_TICKS: u32 = 30;
/// SET_IDLE in 4 ms units sent to the boot keyboard in proof images: it then reports
/// every 100 ms without a key press, which makes HID liveness measurable.
pub(super) const HID_IDLE_4MS: u8 = 25;

static HID_REPORTS: AtomicU32 = AtomicU32::new(0);
static HID_LAST_NS: AtomicU64 = AtomicU64::new(0);
static HID_MAX_GAP_NS: AtomicU64 = AtomicU64::new(0);

/// Called by the resident loop for every retired HID report.
pub(super) fn note_hid_report() {
    let now = clock_get();
    let last = HID_LAST_NS.swap(now, Ordering::Relaxed);
    if last != 0 {
        HID_MAX_GAP_NS.fetch_max(now.saturating_sub(last), Ordering::Relaxed);
    }
    HID_REPORTS.fetch_add(1, Ordering::Relaxed);
}

/// NET7a: what was handed to the bus, frame by frame. Each frame is written into the
/// TX buffer only while the lane is idle, i.e. after the previous transfer's exact
/// completion; an error or short completion stops the schedule.
#[derive(Clone, Copy, Default)]
struct TxProof {
    node: Option<[u8; 6]>,
    receive_ns: u64,
    next: u32,
    submitted: u32,
    completed: u32,
    bytes: u64,
    pad_frames: u32,
    submit_ns: u64,
    max_ns: u64,
    errors: u32,
}

impl TxProof {
    fn finished(&self) -> bool {
        self.errors > 0 || self.completed == probe::TX_EXACT + probe::TX_BURST
    }
}

struct TxScratch(core::cell::UnsafeCell<([u8; 1514], [u8; 1522])>);
// SAFETY: drv-xhci is single-threaded, and `tx_pump` is the only way in.
unsafe impl Sync for TxScratch {}
static TX_SCRATCH: TxScratch = TxScratch(core::cell::UnsafeCell::new(([0; 1514], [0; 1522])));

struct RxSnapshot(core::cell::UnsafeCell<[u8; NIC_RX_LEN as usize]>);
// SAFETY: drv-xhci is single-threaded, and `rx_snapshot` is the only way in.
unsafe impl Sync for RxSnapshot {}
static RX_SNAPSHOT: RxSnapshot = RxSnapshot(core::cell::UnsafeCell::new([0; NIC_RX_LEN as usize]));

/// An inventoried ax88 handed to the resident loop instead of being left unconfigured.
#[derive(Clone, Copy)]
pub(super) struct NicHandoff {
    slot_id: u8,
    page_va: u64,
    page_phys: u64,
    speed_raw: u8,
    slot_words: [u32; 8],
    configuration: u8,
    profile: Profile,
    device: [u8; 18],
    ep0_ring: TransferRing,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn handoff(
    slot_id: u8,
    page_va: u64,
    page_phys: u64,
    speed_raw: u8,
    slot_words: [u32; 8],
    device: [u8; 18],
    config: &[u8],
    configuration: u8,
    ep0_ring: TransferRing,
) -> Option<NicHandoff> {
    if !ENABLED {
        return None;
    }
    let speed = match speed_raw {
        1 => Speed::Full,
        3 => Speed::High,
        4 => Speed::Super,
        _ => return None,
    };
    match Profile::from_descriptors(&device, config, speed) {
        Ok(profile) => Some(NicHandoff {
            slot_id,
            page_va,
            page_phys,
            speed_raw,
            slot_words,
            configuration,
            profile,
            device,
            ep0_ring,
        }),
        Err(error) => {
            ax88_record(format_args!(
                "NET4c3 nic refused profile={:?}; adapter left unconfigured\n",
                error
            ));
            None
        }
    }
}

struct Lane {
    ring: TransferRing,
    ring_va: u64,
    ring_phys: u64,
    buffer: DmaBuf,
    len: u32,
    dci: u8,
    address: u8,
    ticket: Option<TransferTicket>,
    token: u64,
}

impl Lane {
    fn segment(&self) -> DmaTrbs {
        // SAFETY: inside the custodied NIC backing, mapped for the driver's lifetime.
        unsafe { DmaTrbs::new(self.ring_va as *mut Trb, RING_TRBS) }
    }

    fn context_va(&self, page_va: u64) -> Option<u64> {
        endpoint_context_va(page_va, self.dci)
    }
}

/// How a cycle's verified stop ended one lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Ended {
    Idle,
    Stopped(u8),
    Raced,
}

pub(super) struct NicResident {
    /// The reference TX priming loop has completed since HELLO.
    frame_primed: bool,
    dev: NicHandoff,
    rx: Lane,
    status: Lane,
    tx: Lane,
    tx_proof: TxProof,
    life: NicLifecycle,
    link: Option<bool>,
    /// When the status lane's link bit last changed (NET5b's agreement check).
    status_changed_ns: u64,
    /// NET6 receive-window counts and when the peer's END frame first arrived.
    rx_tally: Tally,
    rx_end_ns: u64,
    status_reports: u32,
    rx_completions: u32,
    rx_frames: u32,
    hid_idle_cc: Option<u8>,
    /// Wall-clock split of the last start: Configure Endpoint with its readback, then the
    /// two CLEAR_FEATURE(ENDPOINT_HALT) requests.
    last_start_ns: (u64, u64),
    proof: Proof,
    /// NET5b: the exact vendor-request pipe and the pure link sequence it carries.
    pipe: ControlPipe,
    link_proof: Link,
}

#[derive(Clone, Copy, Default)]
struct Window {
    start_ns: u64,
    hid_start: u32,
    control_ok: u32,
    control_total: u32,
    late_max_ns: u64,
    busy_max_ns: u64,
    control_max_ns: u64,
}

#[derive(Clone, Copy, Default)]
struct Proof {
    tick: u32,
    due_ns: u64,
    window: Window,
    control_ok: u32,
    control_total: u32,
    hid_start: u32,
    max_cycle_ns: u64,
    custody: u32,
    lanes: usize,
    before: Option<CompletionStats>,
    done: bool,
}

fn lane_dci(address: u8) -> Option<u8> {
    endpoint_context_index(address & 0x0f, address & 0x80 != 0)
}

impl NicResident {
    pub(super) fn bring_up(
        mut dev: NicHandoff,
        io: &mut Mmio,
        layout: RegisterLayout,
        image: &mut NoOpCommandImage,
        evt: &DmaTrbs,
        hid_idle_cc: Option<u8>,
    ) -> Option<Self> {
        let controller = CONTROLLER_BASE.load(Ordering::Relaxed);
        let (raw, phys) = create_custodied_dma(NIC_DMA_LEN);
        if raw == u64::MAX || phys == 0 {
            ax88_record(format_args!(
                "NET4c3 nic FAIL stage=custody; adapter left unconfigured\n"
            ));
            return None;
        }
        if vmar_map(
            Handle(0),
            Handle(raw as u32),
            0,
            NIC_DMA_VA,
            NIC_DMA_LEN,
            (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
        ) != 0
        {
            ax88_record(format_args!("NET4c3 nic FAIL stage=map; DMA retained\n"));
            return None;
        }
        let rx_off = boundary_window(phys, NIC_DMA_LEN, NIC_RX_LEN)?;
        let rings = if rx_off == 0 { NIC_RX_LEN } else { 0 };
        if rings + RINGS_LEN > NIC_DMA_LEN
            || (rings < rx_off + NIC_RX_LEN && rx_off < rings + RINGS_LEN)
        {
            return None;
        }
        let (status_address, rx_address) = (dev.profile.status().address, dev.profile.rx().address);
        let tx_address = dev.profile.tx().address;
        let tx_dci = lane_dci(tx_address)?;
        let (status_dci, rx_dci) = (lane_dci(status_address)?, lane_dci(rx_address)?);
        let lane =
            |ring_off: u64, buffer_off: u64, len: u64, dci: u8, address: u8| -> Option<Lane> {
                let ring_va = NIC_DMA_VA + rings + ring_off;
                let ring_phys = phys + rings + ring_off;
                // SAFETY: inside the mapped NIC backing; ring and buffer regions are disjoint.
                let mut segment = unsafe { DmaTrbs::new(ring_va as *mut Trb, RING_TRBS) };
                let ring = TransferRing::new(&mut segment, ring_phys).ok()?;
                let buffer = unsafe {
                    DmaBuf::new(
                        (NIC_DMA_VA + buffer_off) as *mut u8,
                        phys + buffer_off,
                        len as usize,
                    )
                };
                Some(Lane {
                    ring,
                    ring_va,
                    ring_phys,
                    buffer,
                    len: len as u32,
                    dci,
                    address,
                    ticket: None,
                    token: 0,
                })
            };
        let rx = lane(RX_RING, rx_off, NIC_RX_LEN, rx_dci, rx_address)?;
        let tx = lane(TX_RING, rings + TX_BUF, TX_BUF_LEN, tx_dci, tx_address)?;
        let status = lane(
            STATUS_RING,
            rings + STATUS_BUF,
            STATUS_LEN as u64,
            status_dci,
            status_address,
        )?;
        // SAFETY: the adapter's own page holds its EP0 ring for the driver's lifetime.
        let ep0_seg = &mut unsafe {
            DmaTrbs::new(
                (dev.page_va as usize + OFF_EP0_RING) as *mut Trb,
                EP0_RING_TRBS,
            )
        };
        let set_config = [0x00, 0x09, dev.configuration, 0, 0, 0, 0, 0];
        let configured = control_no_data(
            io,
            layout,
            image,
            evt,
            &mut dev.ep0_ring,
            ep0_seg,
            dev.slot_id,
            set_config,
        );
        if !matches!(configured, Some((1, _))) {
            ax88_record(format_args!(
                "NET4c3 nic FAIL stage=set-config result={:?}; DMA retained\n",
                configured
            ));
            return None;
        }
        let pipe = ControlPipe::new(dev.slot_id).ok()?;
        ax88_record(format_args!(
            "NET4c3 nic ready controller={:#x} slot={} config={} rx=dci{}@{:#x}/{} status=dci{}@{:#x} rings={:#x} backing={:#x} custody={}\n",
            controller,
            dev.slot_id,
            dev.configuration,
            rx.dci,
            rx.buffer.phys(),
            rx.len,
            status.dci,
            status.buffer.phys(),
            phys + rings,
            phys,
            DMA_CUSTODY_SEQUENCE.load(Ordering::Relaxed)
        ));
        Some(Self {
            dev,
            rx,
            status,
            tx,
            tx_proof: TxProof::default(),
            life: NicLifecycle::new(),
            link: None,
            status_changed_ns: 0,
            rx_tally: Tally::new(),
            rx_end_ns: 0,
            status_reports: 0,
            rx_completions: 0,
            rx_frames: 0,
            hid_idle_cc,
            last_start_ns: (0, 0),
            proof: Proof::default(),
            pipe,
            frame_primed: false,
            link_proof: if ax88_link::RX_ENABLED {
                Link::with_goal(Goal::Receive)
            } else {
                Link::new()
            },
        })
    }

    fn lane_mut(&mut self, lane: NicLane) -> &mut Lane {
        match lane {
            NicLane::Rx => &mut self.rx,
            NicLane::Status => &mut self.status,
            NicLane::Tx => &mut self.tx,
        }
    }

    fn ep0(&mut self) -> DmaTrbs {
        // SAFETY: the adapter's own page holds its EP0 ring for the driver's lifetime.
        unsafe {
            DmaTrbs::new(
                (self.dev.page_va as usize + OFF_EP0_RING) as *mut Trb,
                EP0_RING_TRBS,
            )
        }
    }

    fn slot_snapshot(&self) -> SlotSnapshot {
        dsb();
        let va = self.dev.page_va + OFF_OUTPUT_CTX as u64;
        SlotSnapshot::from_words([0, 1, 2, 3].map(|word| read_ctx_word(va, word)))
    }

    fn endpoint(&self, lane: &Lane) -> Option<EndpointSnapshot> {
        lane.context_va(self.dev.page_va)
            .map(read_endpoint_snapshot)
    }

    /// Configure both endpoints, read Running back at the fresh rings, and reset the
    /// device-side data toggles with CLEAR_FEATURE(ENDPOINT_HALT).
    pub(super) fn start(
        &mut self,
        io: &mut Mmio,
        layout: RegisterLayout,
        image: &mut NoOpCommandImage,
        cmd: &mut DmaTrbs,
        evt: &DmaTrbs,
    ) -> Result<(), &'static str> {
        if !self.life.may_configure() {
            return Err("phase");
        }
        let stride = ContextSize::Bytes64;
        // The bulk OUT endpoint context must exist whenever the resident loop may carry the
        // frame channel, not only in TX-proof images: an unconfigured DCI swallows every
        // arming, which stalled the entire frame-channel TX path with no completion and no
        // error. The status and RX endpoints are configured unconditionally for the same
        // reason. — TIMBERDOODLE
        let tx = ENABLED;
        let (Some(slot_off), Some(status_off), Some(rx_off), Some(tx_off)) = (
            stride.input_offset(0),
            stride.input_offset(self.status.dci),
            stride.input_offset(self.rx.dci),
            stride.input_offset(self.tx.dci),
        ) else {
            return Err("context offset");
        };
        if status_off.max(rx_off).max(if tx { tx_off } else { 0 }) + 32 > OFF_OUTPUT_CTX {
            return Err("input window");
        }
        let mut icc = InputControlContext::default();
        if icc.add(0).is_err()
            || icc.add(self.status.dci).is_err()
            || icc.add(self.rx.dci).is_err()
            || (tx && icc.add(self.tx.dci).is_err())
        {
            return Err("input flags");
        }
        let entries = self
            .status
            .dci
            .max(self.rx.dci)
            .max(if tx { self.tx.dci } else { 0 });
        let mut slot = self.dev.slot_words;
        slot[0] = (slot[0] & !(0x1f << 27)) | ((entries as u32) << 27);
        let status_ep = self.dev.profile.status();
        let status_ctx = EndpointContext::interrupt_in(
            status_ep.max_packet_size,
            xhci_interval(self.dev.speed_raw, status_ep.interval),
            self.status.ring_phys,
            status_ep.max_packet_size,
        )
        .map_err(|_| "status context")?;
        let rx_ep = self.dev.profile.rx();
        let burst = rx_ep.companion.map_or(0, |c| c.max_burst);
        let rx_ctx = EndpointContext::bulk_in(rx_ep.max_packet_size, burst, self.rx.ring_phys)
            .map_err(|_| "rx context")?;
        let input = self.dev.page_va + OFF_INPUT_CTX as u64;
        write_ctx(input, icc.words());
        write_ctx(input + slot_off as u64, slot);
        write_ctx(input + status_off as u64, status_ctx.words());
        write_ctx(input + rx_off as u64, rx_ctx.words());
        if tx {
            let tx_ep = self.dev.profile.tx();
            let burst = tx_ep.companion.map_or(0, |c| c.max_burst);
            let tx_ctx = EndpointContext::bulk_out(tx_ep.max_packet_size, burst, self.tx.ring_phys)
                .map_err(|_| "tx context")?;
            write_ctx(input + tx_off as u64, tx_ctx.words());
        }
        dsb();
        let configure_start = clock_get();
        let configure =
            Trb::configure_endpoint(self.dev.page_phys + OFF_INPUT_CTX as u64, self.dev.slot_id)
                .map_err(|_| "configure trb")?;
        match submit_command(io, layout, image, cmd, evt, configure) {
            Some((1, _, _)) => {}
            Some(_) => return Err("configure cc"),
            None => return Err("configure timeout"),
        }
        let slot = self.slot_snapshot();
        if slot.state != SlotState::Configured || slot.context_entries != entries {
            return Err("configured slot readback");
        }
        for lane in [&self.status, &self.rx, &self.tx]
            .into_iter()
            .take(if tx { 3 } else { 2 })
        {
            let restart = lane.ring.restart();
            let running = self.endpoint(lane).ok_or("endpoint va")?;
            if running.state != EndpointState::Running
                || running.dequeue_iova != restart.dequeue_iova
                || running.dequeue_cycle != restart.cycle
            {
                return Err("running endpoint readback");
            }
        }
        self.life.configured().map_err(|_| "lifecycle configure")?;
        let clear_start = clock_get();
        let addresses = [self.status.address, self.rx.address, self.tx.address];
        for &address in &addresses[..if tx { 3 } else { 2 }] {
            let clear_halt = [0x02, 0x01, 0x00, 0x00, address, 0x00, 0x00, 0x00];
            let mut ep0 = self.ep0();
            match control_no_data(
                io,
                layout,
                image,
                evt,
                &mut self.dev.ep0_ring,
                &mut ep0,
                self.dev.slot_id,
                clear_halt,
            ) {
                Some((1, _)) => {}
                _ => return Err("clear halt"),
            }
        }
        let end = clock_get();
        self.last_start_ns = (
            clear_start.saturating_sub(configure_start),
            end.saturating_sub(clear_start),
        );
        Ok(())
    }

    /// Arm every Running lane without a transfer in flight. Never waits.
    pub(super) fn arm_idle(
        &mut self,
        io: &mut Mmio,
        layout: RegisterLayout,
        image: &mut NoOpCommandImage,
        evt: &DmaTrbs,
    ) {
        if crate::frame_chan::needs_mac() && self.link_proof.receiving() {
            // NET7b: the frame provider needs the chip's node address for HELLO; read it once,
            // on the same EP0 pipe the re-arm pump uses.
            let segment = self.ep0();
            // SAFETY: the adapter's own page holds its EP0 data buffer for life.
            let buffer = unsafe {
                DmaBuf::new(
                    (self.dev.page_va as usize + OFF_DATA) as *mut u8,
                    self.dev.page_phys + OFF_DATA as u64,
                    8,
                )
            };
            let mut port = Ep0Port {
                io: &mut *io,
                layout,
                image: &mut *image,
                evt,
                ring: &mut self.dev.ep0_ring,
                segment,
                slot: self.dev.slot_id,
                pipe: &mut self.pipe,
                buffer,
            };
            let mut bytes = [0u8; 6];
            if port
                .exchange(&Request::mac_read(MacRegister::NodeId), &mut bytes)
                .is_ok()
            {
                if let Ok(mac) = decode_mac(&bytes) {
                    crate::frame_chan::set_mac(mac);
                }
            }
        }
        if crate::frame_chan::active() {
            if !self.frame_primed && self.link_proof.receiving() {
                let segment = self.ep0();
                // SAFETY: the adapter's own page holds its EP0 data buffer for life.
                let buffer = unsafe {
                    DmaBuf::new(
                        (self.dev.page_va as usize + OFF_DATA) as *mut u8,
                        self.dev.page_phys + OFF_DATA as u64,
                        8,
                    )
                };
                let mut port = Ep0Port {
                    io: &mut *io,
                    layout,
                    image: &mut *image,
                    evt,
                    ring: &mut self.dev.ep0_ring,
                    segment,
                    slot: self.dev.slot_id,
                    pipe: &mut self.pipe,
                    buffer,
                };
                if kumo_ax88::link::prime_tx(&mut port) {
                    self.frame_primed = true;
                }
            }
            self.tx_frame_submit(io, layout);
        } else if ax88_link::TX_ENABLED {
            self.tx_pump(io, layout);
            // The per-wake re-arm pump: the link tick runs at 10 Hz while frames flow at
            // ~270/s, so the kick belongs here. Guarded to the receiving link.
            if self.link_proof.receiving() {
                let segment = self.ep0();
                // SAFETY: the adapter's own page holds its EP0 data buffer for life.
                let buffer = unsafe {
                    DmaBuf::new(
                        (self.dev.page_va as usize + OFF_DATA) as *mut u8,
                        self.dev.page_phys + OFF_DATA as u64,
                        8,
                    )
                };
                let mut port = Ep0Port {
                    io: &mut *io,
                    layout,
                    image: &mut *image,
                    evt,
                    ring: &mut self.dev.ep0_ring,
                    segment,
                    slot: self.dev.slot_id,
                    pipe: &mut self.pipe,
                    buffer,
                };
                let _ = kumo_ax88::link::rearm_take(&mut port);
            }
        }
        for lane in [NicLane::Rx, NicLane::Status] {
            if self.life.phase() != NicPhase::Running || self.life.is_pending(lane) {
                continue;
            }
            if self.arm(lane, io, layout).is_err() {
                self.life.fail();
                ax88_record(format_args!(
                    "NET4c3 nic FAIL stage=arm lane={:?}; lanes retained\n",
                    lane
                ));
            }
        }
    }

    fn arm(&mut self, which: NicLane, io: &mut Mmio, layout: RegisterLayout) -> Result<(), ()> {
        let slot = self.dev.slot_id;
        let lane = self.lane_mut(which);
        let config = match which {
            NicLane::Rx => NormalTransfer::bulk_in(lane.buffer.phys(), lane.len),
            NicLane::Status => NormalTransfer::interrupt_in(lane.buffer.phys(), lane.len),
            NicLane::Tx => NormalTransfer::bulk_out(lane.buffer.phys(), lane.len),
        };
        let trb = Trb::normal_transfer(config).map_err(|_| ())?;
        let mut segment = lane.segment();
        let ticket = reserve_transfer(&lane.ring, &segment, slot, lane.dci, &[trb]).ok_or(())?;
        let [token] = lane.ring.preview::<1>().map_err(|_| ())?;
        if which == NicLane::Status {
            lane.buffer.zero();
        }
        enqueue_reserved(&mut lane.ring, &mut segment, ticket, &[trb]).ok_or(())?;
        lane.ticket = Some(ticket);
        lane.token = token;
        let dci = lane.dci;
        self.life.armed(which).map_err(|_| ())?;
        dsb();
        ring_slot_doorbell(io, layout, slot, dci);
        Ok(())
    }

    pub(super) fn ready(&self) -> bool {
        [&self.rx, &self.status, &self.tx]
            .into_iter()
            .filter_map(|lane| lane.ticket)
            .any(|ticket| completion_actors().has_ready(ticket).unwrap_or(false))
    }

    /// Consume this owner's routed completions. Never waits for one.
    pub(super) fn service(&mut self) {
        for which in [NicLane::Rx, NicLane::Status, NicLane::Tx] {
            let Some(ticket) = self.lane_mut(which).ticket else {
                continue;
            };
            match completion_actors().take(ticket) {
                Ok(Some(event)) => self.complete(which, ticket, event),
                Ok(None) => {}
                Err(error) => {
                    self.life.fail();
                    ax88_record(format_args!(
                        "NET4c3 nic FAIL stage=take lane={:?} error={:?}; lanes retained\n",
                        which, error
                    ));
                }
            }
        }
    }

    fn complete(&mut self, which: NicLane, ticket: TransferTicket, event: RoutedTransfer) {
        let lane = self.lane_mut(which);
        let actual = completed_length(lane.len, event.completion_code, event.remaining);
        if actual.is_none() || event.key.trb_iova != lane.token {
            quarantine_transfer(ticket, RouteError::Unknown);
            self.life.fail();
            ax88_record(format_args!(
                "NET4c3 nic FAIL stage=complete lane={:?} cc={} trb={:#x} rem={}; lanes retained\n",
                which, event.completion_code, event.key.trb_iova, event.remaining
            ));
            return;
        }
        if completion_actors().finish(ticket).is_err()
            || lane.ring.reclaim_through(event.key.trb_iova).is_err()
        {
            completion_actors().reject(event.event(), RouteError::Unknown);
            self.life.fail();
            ax88_record(format_args!(
                "NET4c3 nic FAIL stage=retire lane={:?}; lanes retained\n",
                which
            ));
            return;
        }
        lane.ticket = None;
        if self.life.completed(which).is_err() {
            return;
        }
        self.deliver(which, actual.expect("validated completion length"));
    }

    fn deliver(&mut self, which: NicLane, actual: usize) {
        match which {
            NicLane::Tx => self.tx_done(actual),
            NicLane::Status => {
                self.status_reports += 1;
                let bytes: [u8; 8] = self.status.buffer.read(0);
                if let Ok(status) = kumo_ax88::status::InterruptStatus::parse(&bytes[..actual]) {
                    if self.link != Some(status.link_up()) {
                        self.link = Some(status.link_up());
                        self.status_changed_ns = clock_get();
                        crate::frame_chan::link(status.link_up());
                        ax88_record(format_args!(
                            "NET4c3 nic status link={} words={:#010x}/{:#010x}\n",
                            status.link_up() as u8,
                            status.words()[0],
                            status.words()[1]
                        ));
                    }
                }
            }
            NicLane::Rx => {
                self.rx_completions += 1;
                // SAFETY: single-threaded; no earlier snapshot slice is in use.
                let snapshot = unsafe { &mut *RX_SNAPSHOT.0.get() };
                self.rx.buffer.read_into(0, &mut snapshot[..actual]);
                if crate::frame_chan::active() {
                    crate::frame_chan::forward_rx(&snapshot[..actual]);
                    return;
                }
                if ax88_link::RX_ENABLED {
                    return self.receive(&snapshot[..actual]);
                }
                let frames = kumo_ax88::rx::Aggregate::parse(&snapshot[..actual])
                    .map(|aggregate| aggregate.entries());
                if let Ok(entries) = frames {
                    self.rx_frames += entries as u32;
                }
                ax88_record(format_args!(
                    "NET4c3 nic rx bytes={} parse={:?}\n",
                    actual, frames
                ));
            }
        }
    }

    /// NET7b: hand the frame provider's oldest TX_FRAME to the bulk OUT lane.
    fn tx_frame_submit(&mut self, io: &mut Mmio, layout: RegisterLayout) {
        if self.life.phase() != NicPhase::Running || self.life.is_pending(NicLane::Tx) {
            return;
        }
        // SAFETY: single-threaded; no earlier scratch borrow is live.
        let (frame, out) = unsafe { &mut *TX_SCRATCH.0.get() };
        let Some(len) = crate::frame_chan::next_tx(frame) else {
            return;
        };
        let mps = self.dev.profile.tx().max_packet_size;
        let encoded = kumo_ax88::tx::encode(&frame[..len], mps, out);
        let Ok(n) = encoded else {
            crate::frame_chan::note_submitted(0);
            crate::frame_chan::note_complete(usize::MAX);
            return;
        };
        self.tx.buffer.write_from(0, &out[..n]);
        self.tx.len = n as u32;
        crate::ax88_record(format_args!("NET7b tx submit transfer={}\n", n));
        crate::frame_chan::note_submitted(n);
        if self.arm(NicLane::Tx, io, layout).is_err() {
            crate::frame_chan::note_complete(usize::MAX);
            self.life.fail();
            ax88_record(format_args!(
                "NET7b tx FAIL stage=arm; frame channel retained\n"
            ));
        }
    }

    /// NET7a: hand the next due frame to the bulk OUT lane. Never waits.
    fn tx_pump(&mut self, io: &mut Mmio, layout: RegisterLayout) {
        let p = self.tx_proof;
        let Some(node) = p.node else {
            return;
        };
        let seq = p.next;
        let now = clock_get();
        if p.errors == 0
            && self.life.is_pending(NicLane::Tx)
            && p.submitted > p.completed
            && now.saturating_sub(p.submit_ns) >= TX_COMPLETION_TIMEOUT_NS
        {
            let state = self.endpoint(&self.tx).map(|snapshot| snapshot.state);
            self.tx_proof.errors += 1;
            return ax88_record(format_args!(
                "NET7 tx FAIL seq={} stage=deadline pending_ms={} submitted={} completed={} ep_state={:?}\n",
                seq,
                now.saturating_sub(p.submit_ns) / 1_000_000,
                p.submitted,
                p.completed,
                state
            ));
        }
        if p.errors > 0
            || seq >= probe::TX_EXACT + probe::TX_BURST
            || self.life.phase() != NicPhase::Running
            || self.life.is_pending(NicLane::Tx)
        {
            return;
        }
        let due = p.receive_ns
            + if seq < probe::TX_EXACT {
                TX_EXACT_DELAY_NS + u64::from(seq) * TX_EXACT_GAP_NS
            } else {
                TX_BURST_DELAY_NS
            };
        if now < due {
            return;
        }
        let mps = self.dev.profile.tx().max_packet_size;
        // SAFETY: single-threaded; no earlier scratch borrow is live.
        let (frame, out) = unsafe { &mut *TX_SCRATCH.0.get() };
        let encoded = probe::tx_frame(seq, node, frame)
            .ok_or(kumo_ax88::tx::Error::OutputShort)
            .and_then(|len| kumo_ax88::tx::encode(&frame[..len], mps, out));
        let Ok(n) = encoded else {
            self.tx_proof.errors += 1;
            return ax88_record(format_args!("NET7 tx FAIL seq={} stage=encode\n", seq));
        };
        self.tx.buffer.write_from(0, &out[..n]);
        self.tx.len = n as u32;
        if self.arm(NicLane::Tx, io, layout).is_err() {
            self.tx_proof.errors += 1;
            self.life.fail();
            return ax88_record(format_args!(
                "NET7 tx FAIL seq={} stage=arm; lanes retained\n",
                seq
            ));
        }
        let p = &mut self.tx_proof;
        p.submitted += 1;
        p.submit_ns = now;
        if n.is_multiple_of(usize::from(mps)) {
            p.pad_frames += 1;
        }
    }

    /// NET7a: one exact OUT completion; the buffer may now take the next frame.
    fn tx_done(&mut self, actual: usize) {
        if crate::frame_chan::note_complete(actual) {
            return;
        }
        let len = self.tx.len as usize;
        let mps = usize::from(self.dev.profile.tx().max_packet_size);
        let p = &mut self.tx_proof;
        let seq = p.next;
        let took = clock_get().saturating_sub(p.submit_ns);
        p.max_ns = p.max_ns.max(took);
        if took >= 1_000_000_000 {
            ax88_record(format_args!(
                "NET7 tx slow seq={} ms={}\n",
                seq,
                took / 1_000_000
            ));
        }
        if actual != len {
            p.errors += 1;
            return ax88_record(format_args!(
                "NET7 tx FAIL seq={} stage=complete actual={} transfer={}\n",
                seq, actual, len
            ));
        }
        p.completed += 1;
        p.bytes += (len - kumo_ax88::tx::PREFIX_LEN) as u64;
        p.next += 1;
        // J839 experiment: the request only sets a flag; the work runs in the link
        // tick's frame, never in proof_tick's (a prior addition overflowed it).
        if ax88_link::TX_ENABLED && p.next.is_multiple_of(TX_REARM_EVERY) {
            kumo_ax88::link::request_rearm();
        }
        if seq >= probe::TX_EXACT && (seq - probe::TX_EXACT + 1).is_multiple_of(256) {
            ax88_record(format_args!(
                "NET7 tx progress seq={} completed={} ms={}\n",
                seq,
                p.completed,
                clock_get().saturating_sub(p.receive_ns) / 1_000_000
            ));
        }
        if seq < probe::TX_EXACT {
            ax88_record(format_args!(
                "NET7 tx seq={} len={} transfer={} pad={} us={}\n",
                seq,
                len - kumo_ax88::tx::PREFIX_LEN,
                len,
                len.is_multiple_of(mps) as u8,
                took / 1_000
            ));
        }
    }

    /// NET6: count one completed aggregate. Exact-phase, END and invalid test frames are
    /// logged one per line; burst frames only reach the tally. Never blocks.
    fn receive(&mut self, bytes: &[u8]) {
        let parsed = Aggregate::parse(bytes);
        self.rx_tally
            .aggregate(bytes.len(), parsed.map(|a| a.counts()));
        let aggregate = match parsed {
            Ok(aggregate) => aggregate,
            Err(error) => {
                return ax88_record(format_args!(
                    "NET6 aggregate n={} bytes={} parse={:?}\n",
                    self.rx_tally.aggregates,
                    bytes.len(),
                    error
                ))
            }
        };
        let mut logged = false;
        for packet in aggregate.packets() {
            let Packet::Frame { bytes: frame, .. } = packet else {
                continue;
            };
            let Some(test) = self.rx_tally.frame(frame) else {
                continue;
            };
            if test.seq < probe::EXACT || test.seq == probe::END || test.verdict != Verdict::Valid {
                logged = true;
                ax88_record(format_args!(
                    "NET6 frame seq={} len={} fnv={:016x} trailing={} fcs={} verdict={:?} hdr={}\n",
                    test.seq,
                    frame.len(),
                    probe::fnv64(frame),
                    test.trailing,
                    test.fcs as u8,
                    test.verdict,
                    Hex(&frame[..frame.len().min(42)])
                ));
            }
        }
        if logged {
            let c = aggregate.counts();
            ax88_record(format_args!(
                "NET6 aggregate n={} bytes={} frames={} dropped={} dummies={}\n",
                self.rx_tally.aggregates,
                bytes.len(),
                c.frames,
                c.dropped,
                c.dummy
            ));
        }
    }

    /// End pending lanes through NET4c-2. Wait only for bounded command completion,
    /// never for packet arrival. — TIMBERDOODLE
    fn stop(
        &mut self,
        io: &mut Mmio,
        layout: RegisterLayout,
        image: &mut NoOpCommandImage,
        cmd: &mut DmaTrbs,
        evt: &DmaTrbs,
    ) -> Result<[Ended; 2], &'static str> {
        let page_va = self.dev.page_va;
        let slot = self.dev.slot_id;
        let mut ended = [Ended::Idle; 2];
        for (n, which) in [NicLane::Rx, NicLane::Status, NicLane::Tx]
            .into_iter()
            .enumerate()
            .take(2)
        {
            if !self.life.is_pending(which) {
                continue;
            }
            let lane = self.lane_mut(which);
            let (Some(ticket), Some(context_va)) = (lane.ticket, lane.context_va(page_va)) else {
                return Err("stop state");
            };
            let token = lane.token;
            let done = quiesce_endpoint(
                io,
                layout,
                image,
                cmd,
                evt,
                &mut lane.ring,
                slot,
                lane.dci,
                Some((ticket, &[token])),
                context_va,
            )
            .map_err(|_| "quiesce")?;
            let actual = done
                .completed
                .map(|event| {
                    if event.key.trb_iova != token {
                        return Err("stop race token");
                    }
                    completed_length(lane.len, event.completion_code, event.remaining)
                        .ok_or("stop race length/code")
                })
                .transpose()?;
            lane.ticket = None;
            self.life.cancelled(which).map_err(|_| "lifecycle cancel")?;
            ended[n] = match actual {
                Some(actual) => {
                    self.deliver(which, actual);
                    Ended::Raced
                }
                None => Ended::Stopped(done.report.stop.completion_code),
            };
        }
        self.life.stopped().map_err(|_| "lifecycle stop")?;
        Ok(ended)
    }

    /// Deconfigure, read Disabled/Addressed back, then rewrite both rings.
    fn disable_and_reclaim(
        &mut self,
        io: &mut Mmio,
        layout: RegisterLayout,
        image: &mut NoOpCommandImage,
        cmd: &mut DmaTrbs,
        evt: &DmaTrbs,
    ) -> Result<(), &'static str> {
        let deconfigure =
            Trb::deconfigure_endpoints(self.dev.slot_id).map_err(|_| "deconfigure trb")?;
        match submit_command(io, layout, image, cmd, evt, deconfigure) {
            Some((1, _, _)) => {}
            Some(_) => return Err("deconfigure cc"),
            None => return Err("deconfigure timeout"),
        }
        if self.slot_snapshot().state != SlotState::Addressed {
            return Err("addressed slot readback");
        }
        for lane in [&self.status, &self.rx] {
            if self.endpoint(lane).map(|e| e.state) != Some(EndpointState::Disabled) {
                return Err("disabled endpoint readback");
            }
        }
        self.life.disabled().map_err(|_| "lifecycle disable")?;
        for which in [NicLane::Rx, NicLane::Status] {
            let lane = self.lane_mut(which);
            let mut segment = lane.segment();
            lane.ring = TransferRing::new(&mut segment, lane.ring_phys).map_err(|_| "ring")?;
        }
        self.life.reclaimed().map_err(|_| "lifecycle reclaim")
    }

    /// Exact EP0 descriptor read while both lanes stay pending.
    fn control_read(
        &mut self,
        io: &mut Mmio,
        layout: RegisterLayout,
        image: &mut NoOpCommandImage,
        evt: &DmaTrbs,
    ) -> bool {
        let data = unsafe {
            DmaBuf::new(
                (self.dev.page_va as usize + OFF_DATA) as *mut u8,
                self.dev.page_phys + OFF_DATA as u64,
                18,
            )
        };
        let mut ep0 = self.ep0();
        let read = control_in(
            io,
            layout,
            image,
            evt,
            &mut self.dev.ep0_ring,
            &mut ep0,
            self.dev.slot_id,
            [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 18, 0x00],
            data,
            18,
        );
        dsb();
        matches!(read, Some((1, 0))) && data.read::<18>(0) == self.dev.device
    }

    pub(super) fn proof_due(&self) -> bool {
        ENABLED && !self.proof.done && self.life.phase() != NicPhase::Failed
    }

    pub(super) fn schedule(&mut self) {
        self.proof.due_ns = clock_get().saturating_add(TICK_NS);
    }

    /// The resident loop reports how long it worked after each wake.
    pub(super) fn note_busy(&mut self, busy_ns: u64) {
        let window = &mut self.proof.window;
        window.busy_max_ns = window.busy_max_ns.max(busy_ns);
    }

    fn open_window(&mut self, now: u64) {
        self.proof.window = Window {
            start_ns: now,
            hid_start: HID_REPORTS.load(Ordering::Relaxed),
            ..Window::default()
        };
        HID_LAST_NS.store(now, Ordering::Relaxed);
        HID_MAX_GAP_NS.store(0, Ordering::Relaxed);
    }

    fn close_window(&mut self, name: &str, controller: u64) {
        let w = self.proof.window;
        ax88_record(format_args!(
            "NET4c3 window={} controller={:#x} ms={} control={}/{} control_max_us={} hid_reports={} hid_max_gap_ms={} busy_max_us={} tick_late_max_ms={} status_reports={} rx_completions={} rx_pending={} status_pending={}\n",
            name,
            controller,
            clock_get().saturating_sub(w.start_ns) / 1_000_000,
            w.control_ok,
            w.control_total,
            w.control_max_ns / 1_000,
            HID_REPORTS.load(Ordering::Relaxed).wrapping_sub(w.hid_start),
            HID_MAX_GAP_NS.load(Ordering::Relaxed) / 1_000_000,
            w.busy_max_ns / 1_000,
            w.late_max_ns / 1_000_000,
            self.status_reports,
            self.rx_completions,
            self.life.is_pending(NicLane::Rx) as u8,
            self.life.is_pending(NicLane::Status) as u8
        ));
    }

    fn idle_tick(
        &mut self,
        io: &mut Mmio,
        layout: RegisterLayout,
        image: &mut NoOpCommandImage,
        evt: &DmaTrbs,
    ) {
        let pending = self.life.is_pending(NicLane::Rx) && self.life.is_pending(NicLane::Status);
        let started = clock_get();
        let ok = pending && self.control_read(io, layout, image, evt);
        let window = &mut self.proof.window;
        window.control_max_ns = window
            .control_max_ns
            .max(clock_get().saturating_sub(started));
        self.proof.window.control_total += 1;
        self.proof.window.control_ok += ok as u32;
        self.proof.control_total += 1;
        self.proof.control_ok += ok as u32;
    }

    /// One proof tick. Returns whether another tick is wanted.
    pub(super) fn proof_tick(
        &mut self,
        io: &mut Mmio,
        layout: RegisterLayout,
        image: &mut NoOpCommandImage,
        cmd: &mut DmaTrbs,
        evt: &DmaTrbs,
    ) -> bool {
        if ax88_link::ENABLED || ax88_link::RX_ENABLED {
            return self.link_tick(io, layout, image, evt);
        }
        let controller = CONTROLLER_BASE.load(Ordering::Relaxed);
        let now = clock_get();
        let late = now.saturating_sub(self.proof.due_ns);
        self.proof.tick += 1;
        let tick = self.proof.tick;
        if tick == 1 {
            self.proof.custody = DMA_CUSTODY_SEQUENCE.load(Ordering::Relaxed);
            self.proof.lanes = completion_actors().occupied();
            self.proof.before = Some(completion_actors().stats());
            self.proof.hid_start = HID_REPORTS.load(Ordering::Relaxed);
            ax88_record(format_args!(
                "NET4c3 resident start controller={:#x} slot={} idle_ticks={} cycles={} settled_ticks={} tick_ms={} hid_idle_cc={:?} lanes={} custody={}\n",
                controller,
                self.dev.slot_id,
                IDLE_TICKS,
                CYCLES,
                SETTLED_TICKS,
                TICK_NS / 1_000_000,
                self.hid_idle_cc,
                self.proof.lanes,
                self.proof.custody
            ));
            self.open_window(now);
        } else {
            // A window closes when the next phase's first tick arrives, so each window
            // holds exactly its own ticks' work and the wait before the next phase.
            if tick == IDLE_TICKS + 1 {
                self.close_window("idle", controller);
                self.open_window(now);
            } else if tick == IDLE_TICKS + CYCLES + 1 {
                self.close_window("cycles", controller);
                self.open_window(now);
            }
            self.proof.window.late_max_ns = self.proof.window.late_max_ns.max(late);
        }
        if tick <= IDLE_TICKS {
            self.idle_tick(io, layout, image, evt);
            return true;
        }
        if tick > IDLE_TICKS + CYCLES {
            self.idle_tick(io, layout, image, evt);
            if tick < IDLE_TICKS + CYCLES + SETTLED_TICKS {
                return true;
            }
            self.close_window("settled", controller);
            self.proof.done = true;
            self.finish(controller);
            return false;
        }
        let cycle = tick - IDLE_TICKS;
        // Both lanes pending going in, so every cycle has exactly two verified stops.
        self.arm_idle(io, layout, image, evt);
        let start = clock_get();
        self.last_start_ns = (0, 0);
        let result = self.stop(io, layout, image, cmd, evt);
        let stopped_at = clock_get();
        let result = result.and_then(|ended| {
            self.disable_and_reclaim(io, layout, image, cmd, evt)?;
            Ok(ended)
        });
        let disabled_at = clock_get();
        let result = result.and_then(|ended| {
            self.start(io, layout, image, cmd, evt)?;
            Ok(ended)
        });
        if result.is_ok() {
            self.arm_idle(io, layout, image, evt);
        }
        let elapsed = clock_get().saturating_sub(start);
        self.proof.max_cycle_ns = self.proof.max_cycle_ns.max(elapsed);
        let census = (
            DMA_CUSTODY_SEQUENCE.load(Ordering::Relaxed),
            completion_actors().occupied(),
            image.command_ring.queued(),
            self.rx.ring.queued(),
            self.status.ring.queued(),
        );
        let flat = census == (self.proof.custody, self.proof.lanes, 0, 1, 1);
        match result {
            Ok(ended) if flat => {
                ax88_record(format_args!(
                    "NET4c3 cycle={} controller={:#x} slot={} rx={:?} status={:?} off=Addressed/Disabled/Disabled on=Configured/Running/Running clear_halt=2 queued=1/1 commands=0 us={} stop_us={} off_us={} on_us={} clear_us={} PASS\n",
                    cycle,
                    controller,
                    self.dev.slot_id,
                    ended[0],
                    ended[1],
                    elapsed / 1_000,
                    stopped_at.saturating_sub(start) / 1_000,
                    disabled_at.saturating_sub(stopped_at) / 1_000,
                    self.last_start_ns.0 / 1_000,
                    self.last_start_ns.1 / 1_000
                ));
            }
            other => {
                self.proof.done = true;
                self.life.fail();
                ax88_record(format_args!(
                    "NET4c3 resident FAIL controller={:#x} cycle={} result={:?} census={:?}; lanes retained\n",
                    controller,
                    cycle,
                    other.map(|_| ()),
                    census
                ));
                return false;
            }
        }
        true
    }

    fn finish(&mut self, controller: u64) {
        let p = self.proof;
        let after = completion_actors().stats();
        let before = p.before.unwrap_or(after);
        let router = (
            after.stopped - before.stopped,
            after.cancelled - before.cancelled,
            after.faults - before.faults,
        );
        let counts = self.life.counts();
        let hid = HID_REPORTS
            .load(Ordering::Relaxed)
            .wrapping_sub(p.hid_start);
        let n = CYCLES;
        let controls = IDLE_TICKS + SETTLED_TICKS;
        let pass = p.control_ok == controls
            && p.control_total == controls
            && (self.hid_idle_cc != Some(1) || hid > 0)
            && counts.configures == n + 1
            && counts.stops == n
            && counts.disables == n
            && counts.reclaims == n
            && counts.cancels == 2 * n
            && router == (2 * n as u64, 2 * n as u64, 0)
            && completion_actors().occupied() == p.lanes
            && DMA_CUSTODY_SEQUENCE.load(Ordering::Relaxed) == p.custody
            && self.life.is_pending(NicLane::Rx)
            && self.life.is_pending(NicLane::Status);
        ax88_record(format_args!(
            "NET4c3 census controller={:#x} configures={} arms={} completions={} cancels={} stops={} disables={} reclaims={} router_stopped={} router_cancelled={} router_faults={} lanes={}/{} custody={}/{}\n",
            controller,
            counts.configures,
            counts.arms,
            counts.completions,
            counts.cancels,
            counts.stops,
            counts.disables,
            counts.reclaims,
            router.0,
            router.1,
            router.2,
            p.lanes,
            completion_actors().occupied(),
            p.custody,
            DMA_CUSTODY_SEQUENCE.load(Ordering::Relaxed)
        ));
        ax88_record(format_args!(
            "NET4c3 resident {} controller={:#x} slot={} cycles={} control={}/{} hid_reports={} status_reports={} rx_completions={} rx_frames={} link={:?} max_cycle_us={} rx_pending={} status_pending={}\n",
            if pass { "PASS" } else { "FAIL" },
            controller,
            self.dev.slot_id,
            n,
            p.control_ok,
            p.control_total,
            hid,
            self.status_reports,
            self.rx_completions,
            self.rx_frames,
            self.link,
            p.max_cycle_ns / 1_000,
            self.life.is_pending(NicLane::Rx) as u8,
            self.life.is_pending(NicLane::Status) as u8
        ));
    }

    /// NET5b: one paced step of the pure link sequence on this owner's exact EP0 pipe.
    /// The NET4c-3 cycling timeline does not run in this mode; both lanes stay armed.
    fn link_tick(
        &mut self,
        io: &mut Mmio,
        layout: RegisterLayout,
        image: &mut NoOpCommandImage,
        evt: &DmaTrbs,
    ) -> bool {
        let controller = CONTROLLER_BASE.load(Ordering::Relaxed);
        let now = clock_get();
        let tx_done = !ax88_link::TX_ENABLED || self.tx_proof.finished();
        if ax88_link::RX_ENABLED && self.rx_tally.end > 0 && tx_done {
            if self.rx_end_ns == 0 {
                self.rx_end_ns = now;
            } else if now.saturating_sub(self.rx_end_ns) >= RX_SETTLE_NS {
                self.link_proof.conclude();
            }
        }
        self.proof.tick += 1;
        if self.proof.tick == 1 {
            self.proof.custody = DMA_CUSTODY_SEQUENCE.load(Ordering::Relaxed);
            self.proof.lanes = completion_actors().occupied();
            self.proof.before = Some(completion_actors().stats());
            self.proof.hid_start = HID_REPORTS.load(Ordering::Relaxed);
            ax88_record(format_args!(
                "NET5b resident controller={:#x} slot={} tick_ms={} hid_idle_cc={:?} lanes={} custody={}\n",
                controller,
                self.dev.slot_id,
                TICK_NS / 1_000_000,
                self.hid_idle_cc,
                self.proof.lanes,
                self.proof.custody
            ));
            self.open_window(now);
        } else {
            let late = now.saturating_sub(self.proof.due_ns);
            self.proof.window.late_max_ns = self.proof.window.late_max_ns.max(late);
        }
        let status = StatusView {
            reports: self.status_reports,
            link: self.link,
            changed_ns: self.status_changed_ns,
        };
        let segment = self.ep0();
        // SAFETY: the adapter's own page holds its EP0 data buffer for the driver's lifetime.
        let buffer = unsafe {
            DmaBuf::new(
                (self.dev.page_va as usize + OFF_DATA) as *mut u8,
                self.dev.page_phys + OFF_DATA as u64,
                8,
            )
        };
        let mut port = Ep0Port {
            io: &mut *io,
            layout,
            image: &mut *image,
            evt,
            ring: &mut self.dev.ep0_ring,
            segment,
            slot: self.dev.slot_id,
            pipe: &mut self.pipe,
            buffer,
        };
        let more = self.link_proof.tick(&mut port, status);
        // NET7a: frames carry the node address the chip reports, read once receive is on.
        if ax88_link::TX_ENABLED
            && more
            && self.tx_proof.node.is_none()
            && self.tx_proof.errors == 0
            && self.link_proof.receiving()
        {
            let mut bytes = [0u8; 6];
            let read = port
                .exchange(&Request::mac_read(MacRegister::NodeId), &mut bytes)
                .ok()
                .and_then(|()| decode_mac(&bytes).ok());
            match read {
                Some(mac) => {
                    self.tx_proof.node = Some(mac);
                    crate::frame_chan::set_mac(mac);
                    self.tx_proof.receive_ns = clock_get();
                    ax88_record(format_args!(
                        "NET7 tx node={} exact_in_ms={} burst_in_ms={} mps={}\n",
                        Hex(&mac),
                        TX_EXACT_DELAY_NS / 1_000_000,
                        TX_BURST_DELAY_NS / 1_000_000,
                        self.dev.profile.tx().max_packet_size
                    ));
                }
                None => {
                    self.tx_proof.errors += 1;
                    ax88_record(format_args!("NET7 tx FAIL stage=node\n"));
                }
            }
        }
        if !more {
            self.proof.done = true;
            self.link_finish(controller);
        }
        more
    }

    fn link_finish(&mut self, controller: u64) {
        let p = self.proof;
        let w = p.window;
        let after = completion_actors().stats();
        let before = p.before.unwrap_or(after);
        let hid = HID_REPORTS
            .load(Ordering::Relaxed)
            .wrapping_sub(p.hid_start);
        ax88_record(format_args!(
            "NET5b service controller={:#x} ticks={} ms={} hid_reports={} hid_max_gap_ms={} busy_max_us={} tick_late_max_ms={} status_reports={} rx_completions={}\n",
            controller,
            p.tick,
            clock_get().saturating_sub(w.start_ns) / 1_000_000,
            hid,
            HID_MAX_GAP_NS.load(Ordering::Relaxed) / 1_000_000,
            w.busy_max_ns / 1_000,
            w.late_max_ns / 1_000_000,
            self.status_reports,
            self.rx_completions
        ));
        let lanes = completion_actors().occupied();
        let custody = DMA_CUSTODY_SEQUENCE.load(Ordering::Relaxed);
        let ep0_queued = self.dev.ep0_ring.queued();
        let faults = after.faults - before.faults;
        let r = self.rx_tally;
        // NET5b keeps MAC receive off, so any RX completion is a finding there. NET6
        // needs every slow frame, some burst, the END frame and no invalid record.
        let x = self.tx_proof;
        // Enabling TX adds one resident completion lane for the driver's life; the
        // baseline was captured before its first arm.
        let expected_lanes = p.lanes + usize::from(ax88_link::TX_ENABLED && x.submitted > 0);
        let sent = !ax88_link::TX_ENABLED
            || (x.errors == 0
                && x.submitted == x.completed
                && x.completed == probe::TX_EXACT + probe::TX_BURST
                && !self.life.is_pending(NicLane::Tx));
        let traffic = sent
            && if ax88_link::RX_ENABLED {
                r.parse_errors == 0
                    && r.invalid == 0
                    && r.duplicates == 0
                    && r.unexpected_seq == 0
                    && r.exact_received() == probe::EXACT
                    && r.burst_received > 0
                    && r.end > 0
            } else {
                self.rx_completions == 0
            };
        if ax88_link::RX_ENABLED {
            self.receive_summary();
        }
        if ax88_link::TX_ENABLED {
            let node = x.node.unwrap_or([0; 6]);
            ax88_record(format_args!(
                "NET7 tx summary node={} exact={}/{} burst={}/{} submitted={} completed={} bytes={} pad_frames={} errors={} max_us={} tx_pending={}\n",
                Hex(&node),
                x.completed.min(probe::TX_EXACT),
                probe::TX_EXACT,
                x.completed.saturating_sub(probe::TX_EXACT),
                probe::TX_BURST,
                x.submitted,
                x.completed,
                x.bytes,
                x.pad_frames,
                x.errors,
                x.max_ns / 1_000,
                self.life.is_pending(NicLane::Tx) as u8
            ));
        }
        let pass = self.link_proof.outcome() == Some(true)
            && faults == 0
            && traffic
            && ep0_queued == 0
            && !self.pipe.is_quarantined()
            && lanes == expected_lanes
            && custody == p.custody
            && self.life.is_pending(NicLane::Rx)
            && self.life.is_pending(NicLane::Status)
            && (self.hid_idle_cc != Some(1) || hid > 0);
        ax88_record(format_args!(
            "{} census controller={:#x} slot={} registered={} routed={} consumed={} retired={} faults={} ep0_queued={} lanes={}/{} custody={}/{} rx_pending={} status_pending={} {}\n",
            if ax88_link::TX_ENABLED {
                "NET7"
            } else if ax88_link::RX_ENABLED {
                "NET6"
            } else {
                "NET5b"
            },
            controller,
            self.dev.slot_id,
            after.registered - before.registered,
            after.routed - before.routed,
            after.consumed - before.consumed,
            after.retired - before.retired,
            faults,
            ep0_queued,
            expected_lanes,
            lanes,
            p.custody,
            custody,
            self.life.is_pending(NicLane::Rx) as u8,
            self.life.is_pending(NicLane::Status) as u8,
            if pass { "PASS" } else { "FAIL" }
        ));
    }

    /// NET6's receive-window counts, then the burst bitmap eight words per line.
    fn receive_summary(&self) {
        let r = self.rx_tally;
        ax88_record(format_args!(
            "NET6 rx aggregates={} parse_errors={} frames={} dropped={} dummies={} max_frames={} max_bytes={} arp={} ipv4={} ipv6={} other={} rx_completions={}\n",
            r.aggregates,
            r.parse_errors,
            r.frames,
            r.dropped,
            r.dummies,
            r.max_frames_per_aggregate,
            r.max_aggregate_bytes,
            r.arp,
            r.ipv4,
            r.ipv6,
            r.other,
            self.rx_completions
        ));
        ax88_record(format_args!(
            "NET6 rx test exact={}/{} burst={}/{} digest={:016x} end={} duplicates={} invalid={} unexpected={} fcs={} no_fcs={}\n",
            r.exact_received(),
            probe::EXACT,
            r.burst_received,
            probe::BURST,
            r.burst_digest,
            r.end,
            r.duplicates,
            r.invalid,
            r.unexpected_seq,
            r.fcs,
            r.no_fcs
        ));
        for (n, words) in r.burst_bits.chunks(8).enumerate() {
            let mut line = [0u8; 128];
            for (i, w) in words.iter().enumerate() {
                for (j, b) in w.to_be_bytes().iter().enumerate() {
                    let at = i * 16 + j * 2;
                    line[at] = b"0123456789abcdef"[usize::from(b >> 4)];
                    line[at + 1] = b"0123456789abcdef"[usize::from(b & 15)];
                }
            }
            ax88_record(format_args!(
                "NET6 rx bitmap line={} words={}\n",
                n,
                core::str::from_utf8(&line).unwrap_or("?")
            ));
        }
    }

    pub(super) fn fail_start(&mut self, stage: &str) {
        self.life.fail();
        ax88_record(format_args!(
            "NET4c3 nic FAIL stage={}; lanes retained\n",
            stage
        ));
    }
}

struct Hex<'a>(&'a [u8]);
impl core::fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}
