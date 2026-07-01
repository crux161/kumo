#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use kumo_abi::{Errno, Handle, Status};
use kumo_i2c_hid::{
    discover_i2c21_pinctrl, discover_i2c_hid_bus, sc8280xp_i2c21_tlmm_plan, BootMouseReport,
    HidDeviceKind, I2cHidBusTopology, KeyboardTopology, TlmmPinctrlPlan,
};
use kumo_ipc::{Message, MessageError};

pub const SORA_NAME: &str = "Sora";
pub const ROOT_SERVER_ORDINAL_ECHO: u32 = 1;
pub const POINTER_BUTTON_MASK: u8 = 0x07;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PointerPosition {
    pub x: u32,
    pub y: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PointerState {
    width: u32,
    height: u32,
    position: PointerPosition,
    buttons: u8,
    events: u32,
}

impl PointerState {
    pub const fn new(width: u32, height: u32) -> Self {
        let width = if width == 0 { 1 } else { width };
        let height = if height == 0 { 1 } else { height };
        Self {
            width,
            height,
            position: PointerPosition {
                x: width / 2,
                y: height / 2,
            },
            buttons: 0,
            events: 0,
        }
    }

    pub const fn position(self) -> PointerPosition {
        self.position
    }

    pub const fn buttons(self) -> u8 {
        self.buttons
    }

    pub const fn events(self) -> u32 {
        self.events
    }

    pub const fn width(self) -> u32 {
        self.width
    }

    pub const fn height(self) -> u32 {
        self.height
    }

    pub fn apply_boot_mouse(&mut self, report: BootMouseReport) {
        self.position.x = clamp_delta(self.position.x, report.x_delta, self.width);
        self.position.y = clamp_delta(self.position.y, report.y_delta, self.height);
        self.buttons = report.buttons.bits() & POINTER_BUTTON_MASK;
        self.events = self.events.saturating_add(1);
    }
}

impl Default for PointerState {
    fn default() -> Self {
        Self::new(1, 1)
    }
}

fn clamp_delta(position: u32, delta: i8, extent: u32) -> u32 {
    let max = extent.saturating_sub(1);
    let delta = i32::from(delta);
    let next = if delta < 0 {
        position.saturating_sub((-delta) as u32)
    } else {
        position.saturating_add(delta as u32)
    };
    next.min(max)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartPolicy {
    Never,
    OnFailure,
    Always,
}

impl RestartPolicy {
    /// Decide whether a terminated instance should be reconstructed. A persistent
    /// service disappearing outside an intentional shutdown is a failure.
    pub const fn should_restart(self, failed: bool) -> bool {
        match self {
            Self::Never => false,
            Self::OnFailure => failed,
            Self::Always => true,
        }
    }
}

/// A bounded restart budget: the floor of DESIGN/002 §5's "give up / escalate"
/// ladder. A supervisor consumes one unit per restart *attempt*; once the cap is
/// reached the service is given up rather than respawned, so a crash-looping
/// instance cannot be restarted forever. The count is transient supervisor state
/// (DESIGN/002 §4) — a Sora restart re-initialises the service plane and resets it,
/// which is acceptable because it holds no critical durable truth.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartBudget {
    max_attempts: u32,
    used: u32,
}

impl RestartBudget {
    pub const fn new(max_attempts: u32) -> Self {
        Self {
            max_attempts,
            used: 0,
        }
    }

    /// Record one restart attempt. Returns true if it was within budget (the caller
    /// may proceed to rebuild) and false once the cap is reached (the caller must give
    /// up). Calling it while already exhausted stays false and consumes nothing.
    pub fn try_consume(&mut self) -> bool {
        if self.used >= self.max_attempts {
            return false;
        }
        self.used += 1;
        true
    }

    /// Whether the budget is spent: every future `try_consume` will refuse.
    pub const fn exhausted(&self) -> bool {
        self.used >= self.max_attempts
    }
}

/// Capped exponential delay between restart attempts. Keeping the schedule as
/// pure supervisor state makes the policy host-testable; the freestanding binary
/// delivers each delay through a one-shot Timer bound to its permanent Port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartBackoff {
    next_delay_ns: u64,
    max_delay_ns: u64,
}

impl RestartBackoff {
    pub const fn new(initial_delay_ns: u64, max_delay_ns: u64) -> Self {
        Self {
            next_delay_ns: if initial_delay_ns < max_delay_ns {
                initial_delay_ns
            } else {
                max_delay_ns
            },
            max_delay_ns,
        }
    }

    /// Return this attempt's delay and advance the next delay, saturating at the cap.
    pub fn next_delay_ns(&mut self) -> u64 {
        let delay = self.next_delay_ns;
        self.next_delay_ns = self.next_delay_ns.saturating_mul(2).min(self.max_delay_ns);
        delay
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServerRecipe<'a> {
    pub name: &'a str,
    pub image_path: &'a str,
    pub restart: RestartPolicy,
}

/// The capabilities Sora retains for one running supervised server.
/// The bootstrap endpoint is moved into the child during `ProcessRun`, so it is
/// deliberately absent here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupervisedServer {
    pub process: Handle,
    pub client: Handle,
}

/// One persistent supervisor record: the construction policy needed to rebuild a
/// service and the capabilities for its current live instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupervisedService<'a> {
    pub recipe: ServerRecipe<'a>,
    pub instance: SupervisedServer,
}

pub struct Sora;

impl Sora {
    pub const fn new() -> Self {
        Self
    }

    pub fn echo<'a>(&self, request: Message<'a>) -> Result<Message<'a>, MessageError> {
        Message::new(ROOT_SERVER_ORDINAL_ECHO, request.bytes, request.handles)
    }
}

impl kumo_rt::Server for Sora {
    fn name(&self) -> &'static str {
        SORA_NAME
    }

    fn dispatch(&mut self, _channel: Handle, _message: &[u8]) -> Status {
        Errno::Ok.status()
    }
}

/// Close every present handle, continuing after an error so one failed close
/// cannot strand the remaining authority. Returns whether every close succeeded.
///
/// The closer is injected to keep the ownership policy host-testable; freestanding
/// Sora passes `kumo_rt::handle_close`.
pub fn close_handles(handles: &[Option<Handle>], mut close: impl FnMut(Handle) -> Status) -> bool {
    close_handles_except(handles, &[], &mut close)
}

/// Close every present handle except those explicitly transferred to the caller.
/// This makes a constructor's ownership handoff visible: failed construction keeps
/// nothing, while success preserves only the returned handles.
pub fn close_handles_except(
    handles: &[Option<Handle>],
    keep: &[Handle],
    mut close: impl FnMut(Handle) -> Status,
) -> bool {
    let mut all_closed = true;
    for handle in handles.iter().flatten() {
        if keep.contains(handle) {
            continue;
        }
        if close(*handle) != Errno::Ok.status() {
            all_closed = false;
        }
    }
    all_closed
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct I2cHidBusSummary {
    pub total: usize,
    pub keyboards: usize,
    pub touchpads: usize,
    pub unknown: usize,
}

pub fn summarize_i2c_hid_bus(topology: &I2cHidBusTopology) -> I2cHidBusSummary {
    let mut summary = I2cHidBusSummary::default();
    for device in topology.devices() {
        summary.total += 1;
        match device.kind {
            HidDeviceKind::Keyboard => summary.keyboards += 1,
            HidDeviceKind::Touchpad => summary.touchpads += 1,
            HidDeviceKind::Unknown => summary.unknown += 1,
        }
    }
    summary
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct I2cHidRuntimeTopology {
    pub bus: I2cHidBusTopology,
    pub pinctrl_plan: Option<TlmmPinctrlPlan>,
}

/// Discover the HID-over-I2C bus and its board pinctrl plan from one immutable DTB snapshot.
/// Sora owns only policy here: this does not map or write TLMM MMIO.
pub fn discover_i2c_hid_runtime_topology(dtb: &[u8]) -> Option<I2cHidRuntimeTopology> {
    let bus = discover_i2c_hid_bus(dtb)?;
    let pinctrl_plan =
        discover_i2c21_pinctrl(dtb).and_then(|topology| sc8280xp_i2c21_tlmm_plan(&topology).ok());
    Some(I2cHidRuntimeTopology { bus, pinctrl_plan })
}

/// The current runtime can safely launch one HID-over-I2C child: the internal keyboard.
/// Touchpad probing still needs shared-controller arbitration, so this policy remains explicit.
pub fn select_i2c_hid_keyboard(topology: I2cHidBusTopology) -> Option<KeyboardTopology> {
    topology.keyboard()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kumo_i2c_hid::{BootMouseReport, MouseButtons};

    #[test]
    fn root_logic_is_host_testable() {
        let sora = Sora::new();
        let request = Message::call(ROOT_SERVER_ORDINAL_ECHO, b"ping", &[]).unwrap();
        let reply = sora.echo(request).unwrap();
        assert_eq!(reply.bytes, b"ping");
        assert_eq!(reply.header.ordinal, ROOT_SERVER_ORDINAL_ECHO);

        let supervised = SupervisedServer {
            process: Handle(3),
            client: Handle(5),
        };
        assert_eq!(supervised.process, Handle(3));
        assert_eq!(supervised.client, Handle(5));

        let service = SupervisedService {
            recipe: ServerRecipe {
                name: "ttyd",
                image_path: "bin/ttyd",
                restart: RestartPolicy::OnFailure,
            },
            instance: supervised,
        };
        assert_eq!(service.recipe.image_path, "bin/ttyd");
        assert_eq!(service.recipe.restart, RestartPolicy::OnFailure);
        assert_eq!(service.instance.process, Handle(3));
        assert_eq!(service.instance.client, Handle(5));
        assert!(!RestartPolicy::Never.should_restart(false));
        assert!(!RestartPolicy::Never.should_restart(true));
        assert!(!RestartPolicy::OnFailure.should_restart(false));
        assert!(RestartPolicy::OnFailure.should_restart(true));
        assert!(RestartPolicy::Always.should_restart(false));
        assert!(RestartPolicy::Always.should_restart(true));

        let handles = [Some(Handle(7)), None, Some(Handle(11))];
        let mut seen = [Handle(0); 2];
        let mut count = 0;
        let all_closed = close_handles(&handles, |handle| {
            seen[count] = handle;
            count += 1;
            if handle == Handle(7) {
                Errno::BadHandle.status()
            } else {
                Errno::Ok.status()
            }
        });
        assert!(!all_closed);
        assert_eq!(seen, [Handle(7), Handle(11)]);

        let mut closed = Handle(0);
        assert!(close_handles_except(&handles, &[Handle(7)], |handle| {
            closed = handle;
            Errno::Ok.status()
        }));
        assert_eq!(closed, Handle(11));
    }

    #[test]
    fn pointer_state_starts_centered_and_sanitizes_empty_bounds() {
        let pointer = PointerState::new(1920, 1080);
        assert_eq!(pointer.width(), 1920);
        assert_eq!(pointer.height(), 1080);
        assert_eq!(pointer.position(), PointerPosition { x: 960, y: 540 });
        assert_eq!(pointer.buttons(), 0);
        assert_eq!(pointer.events(), 0);

        let empty = PointerState::new(0, 0);
        assert_eq!(empty.width(), 1);
        assert_eq!(empty.height(), 1);
        assert_eq!(empty.position(), PointerPosition { x: 0, y: 0 });
    }

    #[test]
    fn pointer_state_applies_signed_deltas_buttons_and_clamps() {
        let mut pointer = PointerState::new(4, 3);
        pointer.apply_boot_mouse(BootMouseReport {
            buttons: MouseButtons::from_bits(MouseButtons::LEFT | 0x80),
            x_delta: 1,
            y_delta: -1,
        });
        assert_eq!(pointer.position(), PointerPosition { x: 3, y: 0 });
        assert_eq!(pointer.buttons(), MouseButtons::LEFT);
        assert_eq!(pointer.events(), 1);

        pointer.apply_boot_mouse(BootMouseReport {
            buttons: MouseButtons::from_bits(MouseButtons::RIGHT | MouseButtons::MIDDLE),
            x_delta: 100,
            y_delta: 100,
        });
        assert_eq!(pointer.position(), PointerPosition { x: 3, y: 2 });
        assert_eq!(
            pointer.buttons(),
            MouseButtons::RIGHT | MouseButtons::MIDDLE
        );
        assert_eq!(pointer.events(), 2);

        pointer.apply_boot_mouse(BootMouseReport {
            buttons: MouseButtons::from_bits(0),
            x_delta: -100,
            y_delta: -100,
        });
        assert_eq!(pointer.position(), PointerPosition { x: 0, y: 0 });
        assert_eq!(pointer.buttons(), 0);
        assert_eq!(pointer.events(), 3);
    }

    #[test]
    fn i2c_hid_policy_summarizes_bus_and_selects_keyboard_only() {
        let dtb = include_bytes!("../../../sc8280xp-lenovo-thinkpad-x13s.dtb");
        let discovery =
            discover_i2c_hid_runtime_topology(dtb).expect("X13s i2c21 HID runtime topology");

        assert_eq!(
            summarize_i2c_hid_bus(&discovery.bus),
            I2cHidBusSummary {
                total: 3,
                keyboards: 1,
                touchpads: 2,
                unknown: 0,
            }
        );

        let plan = discovery.pinctrl_plan.expect("X13s i2c21 TLMM plan");
        assert_eq!(plan.updates().len(), 15);
        assert_eq!(plan.updates()[0].offset, 0x51000);
        assert_eq!(plan.updates()[14].offset, 0xb6000);

        let keyboard = select_i2c_hid_keyboard(discovery.bus).expect("keyboard child");
        assert_eq!(keyboard.i2c_address, 0x68);
        assert_eq!(keyboard.hid_descriptor_register, 1);
        assert_eq!(keyboard.keyboard_interrupt.pin, 104);
    }

    #[test]
    fn restart_budget_gives_up_after_its_cap() {
        // A zero cap refuses immediately: a service that may not be restarted is given
        // up on its first death, consuming nothing.
        let mut none = RestartBudget::new(0);
        assert!(none.exhausted());
        assert!(!none.try_consume());

        // A cap of two grants exactly two attempts, then refuses every further one and
        // reports exhaustion. try_consume on a spent budget stays false and is a no-op.
        let mut budget = RestartBudget::new(2);
        assert!(!budget.exhausted());
        assert!(budget.try_consume());
        assert!(!budget.exhausted());
        assert!(budget.try_consume());
        assert!(budget.exhausted());
        assert!(!budget.try_consume());
        assert!(!budget.try_consume());
        assert!(budget.exhausted());
    }

    #[test]
    fn restart_backoff_doubles_and_stays_capped() {
        let mut backoff = RestartBackoff::new(50, 200);
        assert_eq!(backoff.next_delay_ns(), 50);
        assert_eq!(backoff.next_delay_ns(), 100);
        assert_eq!(backoff.next_delay_ns(), 200);
        assert_eq!(backoff.next_delay_ns(), 200);

        // An initial delay above the cap starts at the cap; multiplication cannot wrap.
        let mut capped = RestartBackoff::new(u64::MAX, 7);
        assert_eq!(capped.next_delay_ns(), 7);
        assert_eq!(capped.next_delay_ns(), 7);
    }
}
