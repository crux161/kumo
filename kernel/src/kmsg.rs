//j559

//! The kernel log (J559): one ring of timestamped lines that `dmesg` reads, fed by the kernel's
//! `klog!` and by userland `LogWrite`, with a console level deciding which lines also reach the
//! screen as they are written.
//!
//! Before this, every diagnostic went straight to the console: about 350 lines a boot on the
//! opi5, the late ones landing on top of the shell prompt. Now a line is always kept here, and
//! echoed only when its level is at or above the console's. Terminal output (`DebugWrite`, the
//! kernel shell, the escape hatch) and the TOWER never pass through this module.
//!
//! Lines are assembled per source before they are kept, because a source writes one line in
//! several pieces (Sora's `ls` + `: proc koid=` + `0xf8` + `\n`). A line's level and timestamp
//! are those of its first piece. — TIMBERDOODLE

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

pub use kumo_abi::sys::log_level::{DEBUG, EMERG, ERR, INFO, NOTICE, WARNING};

/// Bytes the ring keeps: a whole opi5 boot is about 20 KiB once stamped.
pub const RING_BYTES: usize = 64 * 1024;
/// Sources that may each have one unfinished line at a time.
const PENDING_SLOTS: usize = 8;
/// What the console showed before drv-fb took the screen, kept for replay there (see
/// [`take_early`]).
const EARLY_BYTES: usize = 8 * 1024;
/// Longest line kept; a longer one is cut there and continued on the next.
const LINE_BYTES: usize = 240;
/// The kernel's own source id. Processes use their koid, which is never this.
pub const KERNEL_SOURCE: u64 = 0;
const FREE: u64 = u64::MAX;

/// The console level a build starts with: NOTICE and more urgent reach the screen. Building with
/// `KUMO_CONSOLE_LOGLEVEL=7` puts back the full output every boot printed before J559.
pub const DEFAULT_CONSOLE_LEVEL: u8 = match option_env!("KUMO_CONSOLE_LOGLEVEL") {
    Some(level) => match level.as_bytes() {
        [digit @ b'0'..=b'7'] => *digit - b'0',
        _ => NOTICE,
    },
    None => NOTICE,
};

static CONSOLE_LEVEL: AtomicU8 = AtomicU8::new(DEFAULT_CONSOLE_LEVEL);

/// Host tests that change the console level hold this, since the test harness runs in threads.
#[cfg(test)]
pub(crate) static LEVEL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
extern crate std;

/// Whether a line at `level` is echoed to the console now.
pub fn visible(level: u8) -> bool {
    level <= CONSOLE_LEVEL.load(Ordering::Relaxed)
}

pub fn console_level() -> u8 {
    CONSOLE_LEVEL.load(Ordering::Relaxed)
}

/// Set the console level; returns the previous one, or `None` for a level past DEBUG.
pub fn set_console_level(level: u8) -> Option<u8> {
    (level <= DEBUG).then(|| CONSOLE_LEVEL.swap(level, Ordering::Relaxed))
}

#[derive(Clone, Copy)]
struct Pending {
    source: u64,
    stamp_ns: u64,
    len: usize,
    buf: [u8; LINE_BYTES],
}

impl Pending {
    const EMPTY: Pending = Pending {
        source: FREE,
        stamp_ns: 0,
        len: 0,
        buf: [0; LINE_BYTES],
    };
}

/// The ring and its unfinished lines. Plain data, so the host tests drive it directly.
pub struct Log {
    ring: [u8; RING_BYTES],
    /// Bytes ever written; the ring holds the last `RING_BYTES` of them.
    written: u64,
    pending: [Pending; PENDING_SLOTS],
    /// Arrival order of pending slots, so a full table gives up its oldest line.
    order: [u64; PENDING_SLOTS],
    next_order: u64,
    early: [u8; EARLY_BYTES],
    early_len: usize,
    early_open: bool,
}

impl Log {
    pub const fn new() -> Log {
        Log {
            ring: [0; RING_BYTES],
            written: 0,
            pending: [Pending::EMPTY; PENDING_SLOTS],
            order: [0; PENDING_SLOTS],
            next_order: 0,
            early: [0; EARLY_BYTES],
            early_len: 0,
            early_open: true,
        }
    }

    /// Keep bytes the console is echoing, until [`Log::take_early`] closes the window.
    pub fn keep_early(&mut self, bytes: &[u8]) {
        if !self.early_open {
            return;
        }
        let n = bytes.len().min(EARLY_BYTES - self.early_len);
        self.early[self.early_len..self.early_len + n].copy_from_slice(&bytes[..n]);
        self.early_len += n;
    }

    /// Close the early window and hand what it kept to `replay`, in pieces of at most `piece`.
    pub fn take_early(&mut self, piece: usize, mut replay: impl FnMut(&[u8])) {
        if !self.early_open {
            return;
        }
        self.early_open = false;
        for chunk in self.early[..self.early_len].chunks(piece.max(1)) {
            replay(chunk);
        }
        self.early_len = 0;
    }

    /// Take `bytes` from `source` at `now_ns`. Complete lines are stamped and kept; an
    /// unfinished one waits for the rest of its source's next write.
    pub fn append(&mut self, source: u64, bytes: &[u8], now_ns: u64) {
        for &byte in bytes {
            let slot = self.slot_for(source, now_ns);
            if byte == b'\n' {
                self.commit(slot);
                continue;
            }
            if byte == b'\r' {
                continue;
            }
            let pending = &mut self.pending[slot];
            pending.buf[pending.len] = byte;
            pending.len += 1;
            if pending.len == LINE_BYTES {
                self.commit(slot);
            }
        }
    }

    fn slot_for(&mut self, source: u64, now_ns: u64) -> usize {
        if let Some(slot) = self.pending.iter().position(|p| p.source == source) {
            return slot;
        }
        let slot = match self.pending.iter().position(|p| p.source == FREE) {
            Some(slot) => slot,
            None => {
                // Every slot holds someone's unfinished line: keep the oldest as it stands.
                let oldest = (0..PENDING_SLOTS)
                    .min_by_key(|&slot| self.order[slot])
                    .unwrap_or(0);
                self.commit(oldest);
                oldest
            }
        };
        self.pending[slot] = Pending {
            source,
            stamp_ns: now_ns,
            len: 0,
            buf: [0; LINE_BYTES],
        };
        self.order[slot] = self.next_order;
        self.next_order += 1;
        slot
    }

    fn commit(&mut self, slot: usize) {
        let pending = self.pending[slot];
        self.pending[slot].source = FREE;
        let mut stamp = [0u8; 16];
        let stamp_len = format_stamp(pending.stamp_ns, &mut stamp);
        self.push(&stamp[..stamp_len]);
        self.push(&pending.buf[..pending.len]);
        self.push(b"\n");
    }

    fn push(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.ring[(self.written % RING_BYTES as u64) as usize] = byte;
            self.written += 1;
        }
    }

    /// Copy kept lines from byte `offset` (0 = the start of the log) into `dest`. Returns the
    /// bytes copied and the offset to read from next. Lines that have since been overwritten are
    /// skipped up to the first whole line still kept.
    pub fn read(&self, offset: u64, dest: &mut [u8]) -> (usize, u64) {
        let oldest = self.written.saturating_sub(RING_BYTES as u64);
        let mut start = offset.min(self.written);
        if start < oldest {
            start = oldest;
            if oldest > 0 {
                while start < self.written && self.byte(start) != b'\n' {
                    start += 1;
                }
                start = (start + 1).min(self.written);
            }
        }
        let n = dest.len().min((self.written - start) as usize);
        for (i, out) in dest[..n].iter_mut().enumerate() {
            *out = self.byte(start + i as u64);
        }
        (n, start + n as u64)
    }

    fn byte(&self, at: u64) -> u8 {
        self.ring[(at % RING_BYTES as u64) as usize]
    }
}

impl Default for Log {
    fn default() -> Self {
        Self::new()
    }
}

/// `[sssss.uuuuuu] `, Linux's `dmesg` stamp: seconds since power-on, then microseconds.
fn format_stamp(ns: u64, out: &mut [u8; 16]) -> usize {
    let secs = ns / 1_000_000_000;
    let micros = (ns % 1_000_000_000) / 1_000;
    out[0] = b'[';
    let mut digits = [b' '; 5];
    let mut value = secs;
    for slot in digits.iter_mut().rev() {
        *slot = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    out[1..6].copy_from_slice(&digits);
    out[6] = b'.';
    let mut value = micros;
    for index in (7..13).rev() {
        out[index] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    out[13] = b']';
    out[14] = b' ';
    15
}

struct LogCell(UnsafeCell<Log>);

// SAFETY: Ziwei is single-core and every access goes through `BUSY` below.
unsafe impl Sync for LogCell {}

static LOG: LogCell = LogCell(UnsafeCell::new(Log::new()));
/// Held while the ring is changed or read. A writer that finds it held (an interrupt arriving
/// mid-update) skips keeping its bytes rather than corrupt the ring; the echo is unaffected.
static BUSY: AtomicBool = AtomicBool::new(false);

fn with_log<R>(f: impl FnOnce(&mut Log) -> R) -> Option<R> {
    if BUSY.swap(true, Ordering::Acquire) {
        return None;
    }
    let result = f(unsafe { &mut *LOG.0.get() });
    BUSY.store(false, Ordering::Release);
    Some(result)
}

/// Keep `bytes` from `source` at `level`. Returns whether the caller should also echo them to
/// the console now. The caller echoes, so this never touches the console or the Sora borrow.
pub fn record(source: u64, level: u8, bytes: &[u8]) -> bool {
    let now = since_first_record(kumo_hal::active::monotonic_nanos());
    let shown = visible(level);
    let _ = with_log(|log| {
        log.append(source, bytes, now);
        if shown {
            log.keep_early(bytes);
        }
    });
    shown
}

/// The console's first lines, for drv-fb when it claims the screen (J559). drv-fb starts by
/// clearing the glass, so without a replay HDMI would lose the boot lines serial still shows.
/// Called once, from the claim; later calls find the window closed and do nothing. `replay`
/// must not log.
pub fn take_early(piece: usize, replay: impl FnMut(&[u8])) {
    let _ = with_log(|log| log.take_early(piece, replay));
}

/// The counter at the kernel's first record. The opi5's counter runs from power-on, through
/// firmware and netboot, so raw stamps began near 47 s; stamps count from the kernel's first
/// line instead, as Linux's count from its start.
static FIRST_RECORD_NS: AtomicU64 = AtomicU64::new(0);

fn since_first_record(now: u64) -> u64 {
    let base =
        match FIRST_RECORD_NS.compare_exchange(0, now.max(1), Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => now.max(1),
            Err(base) => base,
        };
    now.saturating_sub(base)
}

/// Read the log for `dmesg`; see [`Log::read`].
pub fn read(offset: u64, dest: &mut [u8]) -> (usize, u64) {
    with_log(|log| log.read(offset, dest)).unwrap_or((0, offset))
}

/// `klog!` and friends: format, keep, and echo what the console level allows.
pub fn kernel_fmt(level: u8, args: core::fmt::Arguments<'_>) {
    use core::fmt::Write;

    struct Sink {
        level: u8,
        buf: [u8; 256],
        pos: usize,
    }

    impl Sink {
        fn flush(&mut self) {
            if self.pos == 0 {
                return;
            }
            if record(KERNEL_SOURCE, self.level, &self.buf[..self.pos]) {
                crate::bootstrap::console::write(&self.buf[..self.pos]);
            }
            self.pos = 0;
        }
    }

    impl Write for Sink {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            for &byte in text.as_bytes() {
                if self.pos == self.buf.len() {
                    self.flush();
                }
                self.buf[self.pos] = byte;
                self.pos += 1;
            }
            Ok(())
        }
    }

    let mut sink = Sink {
        level,
        buf: [0; 256],
        pos: 0,
    };
    let _ = sink.write_fmt(args);
    sink.flush();
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::boxed::Box;
    use std::vec::Vec;

    fn log() -> Box<Log> {
        Box::new(Log::new())
    }

    fn all(log: &Log) -> Vec<u8> {
        let mut out = std::vec![0u8; RING_BYTES + 64];
        let (n, next) = log.read(0, &mut out);
        assert_eq!(next, log.written);
        out.truncate(n);
        out
    }

    #[test]
    fn lines_are_stamped_from_their_first_piece() {
        let mut log = log();
        log.append(7, b"ls", 1_500_000_000);
        log.append(7, b": proc koid=0xf8", 1_600_000_000);
        log.append(7, b"\n", 2_000_000_000);
        assert_eq!(all(&log), b"[    1.500000] ls: proc koid=0xf8\n");
    }

    #[test]
    fn interleaved_sources_keep_their_own_lines() {
        // J558's "lsfatfs starting": two writers' pieces must not mix.
        let mut log = log();
        log.append(7, b"ls", 0);
        log.append(9, b"fatfs starting\n", 1_000);
        log.append(7, b": proc koid=0x120\n", 2_000);
        assert_eq!(
            all(&log),
            b"[    0.000001] fatfs starting\n[    0.000000] ls: proc koid=0x120\n"
        );
    }

    #[test]
    fn a_full_table_gives_up_its_oldest_unfinished_line() {
        let mut log = log();
        for source in 1..=PENDING_SLOTS as u64 {
            log.append(source, b"x", source);
        }
        log.append(99, b"y\n", 100);
        let text = all(&log);
        assert!(text.starts_with(b"[    0.000000] x\n"));
        assert!(text.ends_with(b"[    0.000000] y\n"));
    }

    #[test]
    fn long_lines_are_cut_and_carriage_returns_dropped() {
        let mut log = log();
        let long = std::vec![b'a'; LINE_BYTES + 3];
        log.append(1, &long, 0);
        log.append(1, b"\r\n", 0);
        let text = all(&log);
        let lines: Vec<&[u8]> = text
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), 15 + LINE_BYTES);
        assert_eq!(lines[1], b"[    0.000000] aaa");
    }

    #[test]
    fn a_wrapped_ring_reads_from_its_first_whole_line_and_resumes() {
        let mut log = log();
        let line = [b'z'; 100];
        for n in 0..1000u64 {
            log.append(1, &line, n * 1_000_000);
            log.append(1, b"\n", 0);
        }
        let text = all(&log);
        assert!(text.len() <= RING_BYTES);
        assert!(text.starts_with(b"["));
        assert!(text.ends_with(b"\n"));
        // Reading in small pieces gives the same bytes.
        let mut pieces = Vec::new();
        let mut offset = 0;
        loop {
            let mut buf = [0u8; 97];
            let (n, next) = log.read(offset, &mut buf);
            if n == 0 {
                break;
            }
            pieces.extend_from_slice(&buf[..n]);
            offset = next;
        }
        assert_eq!(pieces, text);
    }

    #[test]
    fn the_early_console_is_kept_once_for_the_screen_owner() {
        let mut log = log();
        log.keep_early(b"banner\n");
        log.keep_early(b"sora: supervisor started\n");
        let mut replayed = Vec::new();
        log.take_early(5, |piece| {
            assert!(piece.len() <= 5);
            replayed.extend_from_slice(piece);
        });
        assert_eq!(replayed, b"banner\nsora: supervisor started\n");
        // After the claim the console is drv-fb's own: nothing more is kept or replayed.
        log.keep_early(b"late\n");
        log.take_early(5, |_| panic!("replayed twice"));
        let mut full = Box::new(Log::new());
        full.keep_early(&std::vec![b'x'; EARLY_BYTES + 10]);
        let mut n = 0;
        full.take_early(4096, |piece| n += piece.len());
        assert_eq!(n, EARLY_BYTES);
    }

    #[test]
    fn console_level_filters_by_urgency() {
        let _level = LEVEL_TEST_LOCK.lock().unwrap();
        let before = console_level();
        assert_eq!(set_console_level(NOTICE), Some(before));
        assert!(visible(EMERG) && visible(ERR) && visible(NOTICE));
        assert!(!visible(INFO) && !visible(DEBUG));
        assert_eq!(set_console_level(8), None);
        assert_eq!(set_console_level(DEBUG), Some(NOTICE));
        assert!(visible(DEBUG));
        set_console_level(before);
    }

    #[test]
    fn the_default_is_notice_unless_the_build_asks() {
        if option_env!("KUMO_CONSOLE_LOGLEVEL").is_none() {
            assert_eq!(DEFAULT_CONSOLE_LEVEL, NOTICE);
        }
        assert_eq!(WARNING, 4);
    }
}
