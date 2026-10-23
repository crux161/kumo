#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

//j559
//j562
//j563
//j564
//j568

use drv_i2c_hid::{OptionalProbeCandidates, ProbeConfig, ProbePlan};
use kumo_abi::{Errno, Handle, Status};
use kumo_i2c_hid::{
    discover_i2c21_pinctrl, discover_i2c_hid_bus, sc8280xp_i2c21_tlmm_plan, BootMouseReport,
    HidDeviceKind, I2cHidBusTopology, TlmmPinctrlPlan,
};
use kumo_ipc::{Message, MessageError};

/// Relay one stdout message through the bounded DebugWrite sink. Short writes
/// consume only their acknowledged prefix; a failed sink ends this relay.
pub fn relay_console_output(bytes: &[u8], mut write: impl FnMut(&[u8]) -> u64) -> bool {
    // Channel messages grew to 4096 bytes in J545, but DebugWrite still accepts
    // at most 256. A successful syscall does not acknowledge the tail. — KESTREL
    const DEBUG_WRITE_MAX: usize = 256;
    let mut offset = 0;
    while offset < bytes.len() {
        let end = (offset + DEBUG_WRITE_MAX).min(bytes.len());
        let chunk = &bytes[offset..end];
        let written = write(chunk);
        if written == 0 || written > chunk.len() as u64 {
            return false;
        }
        offset += written as usize;
    }
    true
}

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

/// Longest `/media/<name>` component: an 11-byte FAT label, or a 9-byte `XXXX-XXXX` serial.
pub const MEDIA_NAME_MAX: usize = 11;

/// The directory a removable volume mounts under in `/media` (DESIGN/012 §2.1, J545): its label
/// with the padding trimmed, else its serial written `XXXX-XXXX` the way Linux shows a FAT
/// volume's UUID. Bytes a shell would need quoted, and a leading `.` that would hide the mount,
/// become `_`, so `/media/<name>` is always one plain path component.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaName {
    bytes: [u8; MEDIA_NAME_MAX],
    len: usize,
}

impl MediaName {
    pub fn new(label: Option<[u8; 11]>, serial: u32) -> MediaName {
        let mut name = MediaName {
            bytes: [0; MEDIA_NAME_MAX],
            len: 0,
        };
        if let Some(label) = label {
            let trimmed = label.trim_ascii_end();
            let trimmed = &trimmed[..trimmed.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1)];
            for (index, byte) in trimmed.iter().enumerate() {
                let plain = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.');
                name.bytes[index] = if plain && !(index == 0 && *byte == b'.') {
                    *byte
                } else {
                    b'_'
                };
            }
            name.len = trimmed.len();
        }
        if name.len == 0 {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            for (index, shift) in [28, 24, 20, 16, 12, 8, 4, 0].into_iter().enumerate() {
                let at = if index < 4 { index } else { index + 1 };
                name.bytes[at] = HEX[(serial >> shift) as usize & 0xf];
            }
            name.bytes[4] = b'-';
            name.len = 9;
        }
        name
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// Where a shell path lands in the `/media` tree (DESIGN/012 §2.1, J547).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaRoute<'a> {
    /// Not under `/media`: the path means what it meant before.
    Elsewhere,
    /// `/media` itself: the mounted volumes.
    Root,
    /// Inside the volume mounted as `name`, at `path` from its root; `path` starts with `/`.
    Volume { name: &'a [u8], path: &'a [u8] },
}

/// Split a canonical absolute shell path into its `/media` route.
pub fn media_route(path: &[u8]) -> MediaRoute<'_> {
    let Some(rest) = path.strip_prefix(b"/media") else {
        return MediaRoute::Elsewhere;
    };
    if rest.is_empty() || rest == b"/" {
        return MediaRoute::Root;
    }
    let Some(rest) = rest.strip_prefix(b"/") else {
        // `/mediafoo` is not under `/media`.
        return MediaRoute::Elsewhere;
    };
    match rest.iter().position(|byte| *byte == b'/') {
        None => MediaRoute::Volume {
            name: rest,
            path: b"/",
        },
        Some(slash) => MediaRoute::Volume {
            name: &rest[..slash],
            path: &rest[slash..],
        },
    }
}

/// Whether a shell program may be handed `path` to read (J562). `/etc/shadow` holds password
/// hashes, and `cat`/`wc` receive the whole read-only initrd to read it from, so Sora refuses
/// that one path itself until the identity service is the only holder of the shadow file
/// (PLAN/012). `path` is canonical and absolute, as the shell's working directory resolves it.
pub fn shell_may_read(path: &[u8]) -> bool {
    path != b"/etc/shadow"
}

/// `id`'s line for an account, as coreutils prints it: `uid=1000(crux) gid=1000(crux)
/// groups=1000(crux)`. Only the primary group until `/etc/group` membership is served.
pub fn write_id(
    subject: &kumo_identity::protocol::Subject<'_>,
    out: &mut impl core::fmt::Write,
) -> core::fmt::Result {
    let group = |out: &mut dyn core::fmt::Write| {
        if subject.group.is_empty() {
            write!(out, "{}", subject.gid)
        } else {
            write!(out, "{}({})", subject.gid, subject.group)
        }
    };
    write!(out, "uid={}({}) gid=", subject.uid, subject.name)?;
    group(out)?;
    write!(out, " groups=")?;
    group(out)?;
    writeln!(out)
}

/// A short string held by value, for session state that outlives the reply it arrived in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Text<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> Text<N> {
    pub const fn empty() -> Self {
        Text {
            bytes: [0; N],
            len: 0,
        }
    }

    /// `None` when `text` does not fit.
    pub fn new(text: &str) -> Option<Self> {
        let mut out = Self::empty();
        out.bytes
            .get_mut(..text.len())?
            .copy_from_slice(text.as_bytes());
        out.len = text.len();
        Some(out)
    }

    pub fn as_str(&self) -> &str {
        // Only ever filled from a `&str`, whole.
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

/// Who the console session is (PLAN/012 item 4), copied out of `identityd`'s reply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionUser {
    pub name: Text<32>,
    pub group: Text<32>,
    pub home: Text<128>,
    pub shell: Text<64>,
    pub uid: u32,
    pub gid: u32,
    pub root: bool,
}

impl SessionUser {
    pub fn new(subject: &kumo_identity::protocol::Subject<'_>) -> Option<Self> {
        Some(SessionUser {
            name: Text::new(subject.name)?,
            group: Text::new(subject.group)?,
            home: Text::new(subject.home)?,
            shell: Text::new(subject.shell)?,
            uid: subject.uid,
            gid: subject.gid,
            root: subject.root,
        })
    }

    pub fn subject(&self) -> kumo_identity::protocol::Subject<'_> {
        kumo_identity::protocol::Subject {
            name: self.name.as_str(),
            uid: self.uid,
            gid: self.gid,
            group: self.group.as_str(),
            home: self.home.as_str(),
            shell: self.shell.as_str(),
            root: self.root,
            may_log_in: true,
        }
    }
}

/// Failed logins allowed before the console pauses (PLAN/012).
pub const LOGIN_TRIES: u8 = 3;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LoginStage {
    /// `INDRA login: `, echo on.
    #[default]
    Name,
    /// `Password: `, echo off.
    Password,
    /// The tries are spent; input is ignored until the pause ends.
    Paused,
}

/// What the console should do with the line just submitted at the login prompt.
#[derive(Debug, Eq, PartialEq)]
pub enum LoginStep<'a> {
    /// Ask for the name again: the line was empty.
    AskName,
    /// Turn echo off and ask for the password.
    AskPassword,
    /// Check `password` for `name`. `name` is empty when what was typed cannot be an account,
    /// which the check refuses like any other failure: the prompt never says which part was
    /// wrong.
    Check { name: &'a str, password: &'a [u8] },
    /// Paused; drop the line.
    Ignore,
}

/// The console login (PLAN/012 item 4), as `login(1)` runs it: a name, then a password whatever
/// the name was, then `Login incorrect` or a session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Login {
    name: [u8; 32],
    name_len: usize,
    stage: LoginStage,
    failures: u8,
}

impl Login {
    pub const fn new() -> Login {
        Login {
            name: [0; 32],
            name_len: 0,
            stage: LoginStage::Name,
            failures: 0,
        }
    }

    pub const fn stage(&self) -> LoginStage {
        self.stage
    }

    pub fn line<'a>(&'a mut self, line: &'a [u8]) -> LoginStep<'a> {
        match self.stage {
            LoginStage::Paused => LoginStep::Ignore,
            LoginStage::Name => {
                let typed = line.trim_ascii();
                if typed.is_empty() {
                    return LoginStep::AskName;
                }
                let name = core::str::from_utf8(typed)
                    .ok()
                    .filter(|name| kumo_identity::valid_name(name));
                self.name_len = match name {
                    Some(name) => {
                        self.name[..name.len()].copy_from_slice(name.as_bytes());
                        name.len()
                    }
                    None => 0,
                };
                self.stage = LoginStage::Password;
                LoginStep::AskPassword
            }
            LoginStage::Password => {
                self.stage = LoginStage::Name;
                let name = core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("");
                LoginStep::Check {
                    name,
                    password: line,
                }
            }
        }
    }

    /// A check failed. Returns whether that spent the last try, pausing the console.
    pub fn failed(&mut self) -> bool {
        self.failures = self.failures.saturating_add(1);
        if self.failures >= LOGIN_TRIES {
            self.failures = 0;
            self.stage = LoginStage::Paused;
            true
        } else {
            false
        }
    }

    /// The pause is over: ask for a name, with the tries renewed.
    pub fn resume(&mut self) {
        self.stage = LoginStage::Name;
    }

    /// Start the prompt over at the name without forgiving failures, as after the terminal
    /// restarted mid-password.
    pub fn restart(&mut self) {
        if self.stage == LoginStage::Password {
            self.stage = LoginStage::Name;
        }
    }
}

/// The hostname from `/etc/hostname`: its first line, if that is a plain hostname, else `kumo`.
pub fn hostname(file: &[u8]) -> Text<64> {
    let first = file
        .split(|&b| b == b'\n')
        .next()
        .unwrap_or(&[])
        .trim_ascii();
    let plain = !first.is_empty()
        && first
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'));
    core::str::from_utf8(first)
        .ok()
        .filter(|_| plain)
        .and_then(Text::new)
        .unwrap_or_else(|| Text::new("kumo").unwrap_or(Text::empty()))
}

/// The boot-local writable FAT scratch volume is visible at `/tmp`, separate from the
/// read-only initrd root. Only a whole mount component selects its capability. — KESTREL
pub fn scratch_path(path: &[u8]) -> Option<&[u8]> {
    match path.strip_prefix(b"/tmp")? {
        b"" => Some(b"/"),
        rest if rest.starts_with(b"/") => Some(rest),
        _ => None,
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
    pub probe_plan: Option<ProbePlan>,
}

/// Discover the HID-over-I2C bus and its board pinctrl plan from one immutable DTB snapshot.
/// Sora owns only policy here: this does not map or write TLMM MMIO.
pub fn discover_i2c_hid_runtime_topology(dtb: &[u8]) -> Option<I2cHidRuntimeTopology> {
    let bus = discover_i2c_hid_bus(dtb)?;
    let pinctrl_plan =
        discover_i2c21_pinctrl(dtb).and_then(|topology| sc8280xp_i2c21_tlmm_plan(&topology).ok());
    let probe_plan = ProbePlan::for_x13s(bus).ok();
    Some(I2cHidRuntimeTopology {
        bus,
        pinctrl_plan,
        probe_plan,
    })
}

/// The current runtime starts the required keyboard probe while retaining the full child plan.
/// Optional touchpad candidates can be skipped until shared-controller arbitration is ready.
pub fn select_i2c_hid_keyboard_probe(discovery: &I2cHidRuntimeTopology) -> Option<ProbeConfig> {
    discovery
        .probe_plan
        .as_ref()?
        .keyboard()
        .map(|device| device.config)
}

pub fn select_i2c_hid_optional_probe_candidates(
    discovery: &I2cHidRuntimeTopology,
) -> OptionalProbeCandidates {
    match discovery.probe_plan.as_ref() {
        Some(plan) => plan.optional_probe_candidates(),
        None => OptionalProbeCandidates::EMPTY,
    }
}

pub fn select_i2c_hid_optional_attention_irq(discovery: &I2cHidRuntimeTopology) -> Option<u32> {
    select_i2c_hid_optional_probe_candidates(discovery).shared_attention_irq()
}

/// The kernel log levels by name, most urgent first, as `dmesg -n` takes them (J559).
const LOG_LEVEL_NAMES: [&[u8]; 8] = [
    b"emerg", b"alert", b"crit", b"err", b"warn", b"notice", b"info", b"debug",
];

/// `dmesg -n`'s argument: a digit 0-7 or a level name (`warning` is accepted for `warn`).
pub fn parse_log_level(text: &[u8]) -> Option<u8> {
    match text {
        [digit @ b'0'..=b'7'] => Some(digit - b'0'),
        b"warning" => Some(4),
        _ => LOG_LEVEL_NAMES
            .iter()
            .position(|name| *name == text)
            .map(|level| level as u8),
    }
}

/// The name `dmesg -n` reports for a console level the kernel returned.
pub fn log_level_name(level: u64) -> Option<&'static [u8]> {
    usize::try_from(level)
        .ok()
        .and_then(|level| LOG_LEVEL_NAMES.get(level))
        .copied()
}

/// The text cell drv-fb draws: 8x16 pixels (`drv_fb::GLYPH_W`/`GLYPH_H`, pinned by a test).
pub const CONSOLE_CELL_PX: (u32, u32) = (8, 16);

/// Restore default colors, clear the entire display, and show the cursor at home.
pub const CONSOLE_SESSION_RESET: &[u8] = b"\x1b[?25l\x1b[0m\x1b[2J\x1b[H\x1b[?25h";

/// The grid drv-fb's console has on a `width` x `height` framebuffer, as the window size a
/// full-screen program should paint (J557). `None` when not even one cell fits, or the grid
/// cannot be expressed in a `winsize`.
pub fn console_window(width: u32, height: u32) -> Option<ttyd::WindowSize> {
    // The presenter reserves a view band above the text console (F1a). Both sides derive the
    // console grid from the same shared layout, so a full-screen program never paints past
    // the rows drv-fb actually owns.
    let region = kumo_graphics::console_region(width, height);
    let columns = u16::try_from(region.width / CONSOLE_CELL_PX.0).ok()?;
    let rows = u16::try_from(region.height / CONSOLE_CELL_PX.1).ok()?;
    (rows > 0 && columns > 0).then_some(ttyd::WindowSize { rows, columns })
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    extern crate std;

    use super::*;
    use alloc::vec::Vec;
    use drv_fb::{Console, BG, GLYPH_H, GLYPH_W};
    use kumo_i2c_hid::{BootMouseReport, MouseButtons};
    use std::alloc::{GlobalAlloc, Layout, System};

    struct TestFramebuffer {
        pixels: *mut u32,
        layout: Layout,
        len: usize,
    }

    impl TestFramebuffer {
        fn new(len: usize) -> Self {
            let layout = Layout::array::<u32>(len).unwrap();
            // The linked KUMO runtime installs a 256 KiB bootstrap allocator;
            // host framebuffers must not invoke its target growth SVC. — KESTREL
            let pixels = unsafe { System.alloc_zeroed(layout) as *mut u32 };
            assert!(!pixels.is_null());
            Self {
                pixels,
                layout,
                len,
            }
        }

        fn pixels(&self) -> &[u32] {
            unsafe { core::slice::from_raw_parts(self.pixels, self.len) }
        }
    }

    impl Drop for TestFramebuffer {
        fn drop(&mut self) {
            unsafe { System.dealloc(self.pixels as *mut u8, self.layout) };
        }
    }

    #[test]
    fn full_kilo_repaint_reaches_pixels_and_final_cursor_through_short_writes() {
        check_kilo_repaint(80, 24);
        check_kilo_repaint(80, 30);
        check_kilo_repaint(200, 56);
    }

    fn check_kilo_repaint(columns: usize, rows: usize) {
        let width = columns * GLYPH_W;
        let height = rows * GLYPH_H;
        let mut repaint = Vec::from(b"\x1b[?25l\x1b[H".as_slice());
        for row in 0..rows - 2 {
            repaint.resize(repaint.len() + columns, b'A' + row as u8);
            repaint.extend_from_slice(b"\x1b[39m\x1b[0K\r\n");
        }
        repaint.extend_from_slice(b"\x1b[0K\x1b[7m");
        repaint.resize(repaint.len() + columns, b'S');
        repaint.extend_from_slice(
            b"\x1b[0m\r\n\x1b[0KHELP: Ctrl-S = save | Ctrl-Q = quit\x1b[2;3H\x1b[?25h",
        );
        assert!(repaint.len() > ttyd::REPLY_BUF_BYTES);
        let actual = TestFramebuffer::new(width * height);
        let mut console = unsafe { Console::new(actual.pixels, width, height, width) };
        let mut relayed = Vec::new();
        // Match relibc's bounded stdout messages, then the console's short writes.
        // At 200x56 the status and final cursor are in the third message. — KESTREL
        for payload in repaint.chunks(kumo_ipc::MAX_INLINE_BYTES) {
            let message = Message::new(1, payload, &[]).unwrap();
            let mut receive = [0u8; kumo_ipc::MAX_INLINE_BYTES];
            receive[..message.bytes.len()].copy_from_slice(message.bytes);
            assert!(relay_console_output(
                &receive[..message.bytes.len()],
                |chunk| {
                    assert!(chunk.len() <= 256);
                    let accepted = chunk.len().min(73);
                    console.write(&chunk[..accepted]);
                    relayed.extend_from_slice(&chunk[..accepted]);
                    accepted as u64
                }
            ));
        }
        assert_eq!(relayed, repaint);
        let expected = TestFramebuffer::new(width * height);
        let mut reference = unsafe { Console::new(expected.pixels, width, height, width) };
        reference.write(&repaint);
        assert!(actual.pixels().iter().eq(expected.pixels()));
        assert_eq!(actual.pixels()[GLYPH_H * width + 2 * GLYPH_W], 0x00ff_ffff);
        assert!(actual.pixels()[(rows - 1) * GLYPH_H * width..]
            .iter()
            .any(|&p| p != BG));
    }

    #[test]
    fn session_reset_clears_the_whole_screen_and_homes_a_default_colored_prompt() {
        for (width, height) in [(1600, 900), (640, 480)] {
            let actual = TestFramebuffer::new(width * height);
            let mut console = unsafe { Console::new(actual.pixels, width, height, width) };
            console.write(b"\x1b[31;44m\x1b[7mold editor text\x1b[2;3H\x1b[?25h");
            // Include the four sub-cell scanlines at the bottom of a 900px display.
            // Nothing from the previous screen may survive the session. — KESTREL
            unsafe { core::slice::from_raw_parts_mut(actual.pixels, actual.len).fill(0x00ab_cdef) };
            console.write(CONSOLE_SESSION_RESET);
            console.write(b"\x1b[?25l");
            assert!(actual.pixels().iter().all(|&p| p == BG));
            console.write(b"\x1b[?25h");
            kumoza::WorkingDirectory::default().write_prompt(|bytes| console.write(bytes));
            assert!(actual.pixels()[..width * GLYPH_H].iter().any(|&p| p != BG));
            assert!(actual.pixels()[width * GLYPH_H..].iter().all(|&p| p == BG));

            let expected = TestFramebuffer::new(width * height);
            let mut reference = unsafe { Console::new(expected.pixels, width, height, width) };
            reference.write(b"\x1b[?25h[MUREX /]> ");
            assert!(actual.pixels().iter().eq(expected.pixels()));
        }
    }

    #[test]
    fn console_relay_stops_on_zero_error_or_invalid_acknowledgement() {
        for result in [0, u64::MAX, 5] {
            let mut calls = 0;
            assert!(!relay_console_output(b"tail", |_| {
                calls += 1;
                result
            }));
            assert_eq!(calls, 1);
        }
    }

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
    fn i2c_hid_policy_builds_multi_device_probe_plan_and_selects_keyboard() {
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

        let probe_plan = discovery.probe_plan.expect("X13s i2c21 probe plan");
        assert_eq!(probe_plan.devices().len(), 3);
        assert_eq!(probe_plan.required_count(), 1);
        assert_eq!(probe_plan.optional_count(), 2);
        assert!(probe_plan.can_skip_missing_address(0x15));
        assert!(probe_plan.can_skip_missing_address(0x2c));
        assert!(!probe_plan.can_skip_missing_address(0x68));

        let keyboard = select_i2c_hid_keyboard_probe(&discovery).expect("keyboard probe config");
        assert_eq!(keyboard.i2c_address, 0x68);
        assert_eq!(keyboard.hid_descriptor_register, 1);
        assert_eq!(keyboard.attention_irq, kumo_abi::tlmm_gpio_irq(104, 8));

        let optional = select_i2c_hid_optional_probe_candidates(&discovery);
        assert_eq!(optional.candidates().len(), 2);
        assert_eq!(
            select_i2c_hid_optional_attention_irq(&discovery),
            Some(kumo_abi::tlmm_gpio_irq(182, 8))
        );
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

    #[test]
    fn a_labelled_stick_mounts_under_its_label() {
        assert_eq!(
            MediaName::new(Some(*b"STICKY     "), 0x4567_13f4).as_bytes(),
            b"STICKY"
        );
        assert_eq!(
            MediaName::new(Some(*b"KUMO_DATA.1"), 0).as_bytes(),
            b"KUMO_DATA.1"
        );
    }

    #[test]
    fn an_unnamed_stick_mounts_under_its_serial() {
        // CRUX's J543 stick's data volume serial, as Linux would show it.
        assert_eq!(MediaName::new(None, 0x4567_13f4).as_bytes(), b"4567-13F4");
        assert_eq!(MediaName::new(None, 0).as_bytes(), b"0000-0000");
        assert_eq!(
            MediaName::new(Some([b' '; 11]), 0xdead_beef).as_bytes(),
            b"DEAD-BEEF"
        );
        assert_eq!(MediaName::new(Some([0; 11]), 0x1).as_bytes(), b"0000-0001");
    }

    #[test]
    fn a_label_never_needs_quoting_or_hides_itself() {
        assert_eq!(
            MediaName::new(Some(*b"MY DISK    "), 0).as_bytes(),
            b"MY_DISK"
        );
        assert_eq!(
            MediaName::new(Some(*b"A/B*C?     "), 0).as_bytes(),
            b"A_B_C_"
        );
        assert_eq!(
            MediaName::new(Some(*b".HIDDEN    "), 0).as_bytes(),
            b"_HIDDEN"
        );
        assert_eq!(
            MediaName::new(Some(*b"\xc9T\xc9        "), 0).as_bytes(),
            b"_T_"
        );
    }

    #[test]
    fn media_paths_route_to_their_volume_and_path() {
        assert_eq!(media_route(b"/media"), MediaRoute::Root);
        assert_eq!(media_route(b"/media/"), MediaRoute::Root);
        assert_eq!(
            media_route(b"/media/KUMO"),
            MediaRoute::Volume {
                name: b"KUMO",
                path: b"/"
            }
        );
        assert_eq!(
            media_route(b"/media/KUMO/"),
            MediaRoute::Volume {
                name: b"KUMO",
                path: b"/"
            }
        );
        assert_eq!(
            media_route(b"/media/KUMO/LOREM_JP.TXT"),
            MediaRoute::Volume {
                name: b"KUMO",
                path: b"/LOREM_JP.TXT"
            }
        );
        assert_eq!(
            media_route(b"/media/4567-13F4/EFI/BOOT"),
            MediaRoute::Volume {
                name: b"4567-13F4",
                path: b"/EFI/BOOT"
            }
        );
    }

    #[test]
    fn other_paths_are_not_media() {
        assert_eq!(media_route(b"etc/autoexec"), MediaRoute::Elsewhere);
        assert_eq!(media_route(b"/mediafoo"), MediaRoute::Elsewhere);
        assert_eq!(media_route(b"media/KUMO"), MediaRoute::Elsewhere);
        assert_eq!(media_route(b""), MediaRoute::Elsewhere);
    }

    #[test]
    fn scratch_mount_and_relative_paths_stay_in_the_canonical_namespace() {
        assert_eq!(scratch_path(b"/tmp"), Some(b"/".as_slice()));
        assert_eq!(
            scratch_path(b"/tmp/HELLO.TXT"),
            Some(b"/HELLO.TXT".as_slice())
        );
        assert_eq!(scratch_path(b"/tmp-other"), None);
        assert_eq!(scratch_path(b"tmp"), None);
        let mut cwd = kumoza::WorkingDirectory::default();
        cwd.change("/media/KUMO", |_| Ok(())).unwrap();
        let path = cwd.resolve("LOREM_JP.TXT").unwrap();
        assert_eq!(
            media_route(path.as_bytes()),
            MediaRoute::Volume {
                name: b"KUMO",
                path: b"/LOREM_JP.TXT",
            }
        );
        let scratch = cwd.resolve("../../tmp/HELLO.TXT").unwrap();
        assert_eq!(
            scratch_path(scratch.as_bytes()),
            Some(b"/HELLO.TXT".as_slice())
        );
        assert_eq!(media_route(scratch.as_bytes()), MediaRoute::Elsewhere);
    }

    #[test]
    fn console_window_is_drv_fbs_own_grid() {
        // J557: the size Kilo is told must be the grid drv-fb actually draws.
        assert_eq!(CONSOLE_CELL_PX, (GLYPH_W as u32, GLYPH_H as u32));
        let window = |w, h| console_window(w, h).map(|s| (s.rows, s.columns));
        assert_eq!(window(1600, 900), Some((46, 200)));
        assert_eq!(window(800, 600), Some((27, 100)));
        assert_eq!(window(640, 480), Some((20, 80)));
        assert_eq!(window(7, 900), None);
        assert_eq!(window(u32::MAX, 900), None);
        // Too short for the view band: the console keeps the whole display.
        assert_eq!(window(800, 287), Some((17, 100)));
    }

    #[test]
    fn dmesg_levels_read_as_digits_or_names() {
        assert_eq!(parse_log_level(b"0"), Some(0));
        assert_eq!(parse_log_level(b"7"), Some(7));
        assert_eq!(parse_log_level(b"notice"), Some(5));
        assert_eq!(parse_log_level(b"warning"), Some(4));
        assert_eq!(parse_log_level(b"debug"), Some(7));
        for bad in [b"8".as_slice(), b"", b"loud", b"77"] {
            assert_eq!(parse_log_level(bad), None);
        }
        assert_eq!(log_level_name(5), Some(b"notice".as_slice()));
        assert_eq!(log_level_name(8), None);
        assert_eq!(log_level_name(u64::MAX), None);
        for level in 0..8u8 {
            assert_eq!(
                parse_log_level(log_level_name(level.into()).unwrap()),
                Some(level)
            );
        }
    }

    #[test]
    fn id_prints_like_coreutils() {
        use kumo_identity::protocol::Subject;
        let mut subject = Subject {
            name: "crux",
            uid: 1000,
            gid: 1000,
            group: "crux",
            home: "/home/crux",
            shell: "/bin/kumoza",
            root: false,
            may_log_in: true,
        };
        let mut out = alloc::string::String::new();
        write_id(&subject, &mut out).unwrap();
        assert_eq!(out, "uid=1000(crux) gid=1000(crux) groups=1000(crux)\n");
        subject.group = "";
        out.clear();
        write_id(&subject, &mut out).unwrap();
        assert_eq!(out, "uid=1000(crux) gid=1000 groups=1000\n");
    }

    #[test]
    fn login_asks_name_then_password_and_pauses_after_three_failures() {
        let mut login = Login::default();
        assert_eq!(login.line(b"  "), LoginStep::AskName);
        assert_eq!(login.line(b" crux "), LoginStep::AskPassword);
        assert_eq!(login.stage(), LoginStage::Password);
        assert_eq!(
            login.line(b"pw"),
            LoginStep::Check {
                name: "crux",
                password: b"pw"
            }
        );
        assert_eq!(login.stage(), LoginStage::Name);
        assert!(!login.failed());
        // Something that cannot be an account still gets a password prompt, then a check with
        // no name, which fails like any other.
        assert_eq!(login.line(b"Robert'); DROP"), LoginStep::AskPassword);
        assert_eq!(
            login.line(b""),
            LoginStep::Check {
                name: "",
                password: b""
            }
        );
        assert!(!login.failed());
        assert!(login.failed());
        assert_eq!(login.stage(), LoginStage::Paused);
        assert_eq!(login.line(b"crux"), LoginStep::Ignore);
        login.resume();
        assert_eq!(login.line(b"crux"), LoginStep::AskPassword);
        // A terminal restart mid-password goes back to the name and forgives nothing.
        login.restart();
        assert_eq!(login.stage(), LoginStage::Name);
        assert!(!login.failed() && !login.failed() && login.failed());
    }

    #[test]
    fn a_session_user_keeps_what_id_needs() {
        let subject = kumo_identity::protocol::Subject {
            name: "root",
            uid: 0,
            gid: 0,
            group: "root",
            home: "/root",
            shell: "/bin/kumoza",
            root: true,
            may_log_in: true,
        };
        let user = SessionUser::new(&subject).unwrap();
        assert_eq!(user.subject(), subject);
        let long = "/".repeat(129);
        assert_eq!(
            SessionUser::new(&kumo_identity::protocol::Subject {
                home: &long,
                ..subject
            }),
            None
        );
    }

    #[test]
    fn the_hostname_is_the_first_plain_line() {
        assert_eq!(hostname(b"INDRA\n").as_str(), "INDRA");
        assert_eq!(hostname(b"  indra.local \nextra").as_str(), "indra.local");
        for bad in [&b""[..], b"\n", b"bad host\n", b"a/b", b"\xff"] {
            assert_eq!(hostname(bad).as_str(), "kumo");
        }
    }

    #[test]
    fn the_shell_may_not_read_the_shadow_file() {
        assert!(!shell_may_read(b"/etc/shadow"));
        for path in [
            b"/etc/passwd".as_slice(),
            b"/etc/group",
            b"/etc/hostname",
            b"/etc/shadowy",
        ] {
            assert!(shell_may_read(path));
        }
    }
}
