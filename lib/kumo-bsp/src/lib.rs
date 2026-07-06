#![no_std]

//! `kumo-bsp` — the Board Support Package: the **runtime** board-specific parameters the
//! generic arm64 kernel and HAL must not hardcode.
//!
//! KUMO builds one aarch64 kernel/HAL that runs on several boards (QEMU `virt`, the ThinkPad
//! X13s, the Raspberry Pi 5). Today those boards differ by scattered hardcodes and feature
//! sniffing — `PL011_BASE = 0x0900_0000` baked into the HAL (only true on QEMU), the
//! console chosen by "is a framebuffer present?", the GIC version assumed to be v3. This crate
//! makes each board's parameters a single typed, host-testable [`BoardSpec`], keyed by a
//! [`Board`] identity, so the generic code asks the BSP instead of guessing.
//!
//! The board set mirrors `imager::HardwareTarget`'s aarch64 targets: an image is built for one
//! board and (in a later slice) bakes its [`Board`] identity into `BootInfo`, from which the
//! kernel selects the matching [`BoardSpec`]. This crate is `no_std` and allocation-free so
//! both the kernel and the HAL can depend on it.
//!
//! See `DESIGN/017-board-support-package.md` for the decoupling roadmap (which hardcodes move
//! here, in what order) and the board-identity mechanism.

/// A board KUMO's aarch64 build can run on. Mirrors the aarch64 variants of
/// `imager::HardwareTarget`; [`Board::id`] matches that target's `id` string so build-time and
/// runtime identity line up.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Board {
    /// QEMU `virt` machine — the aarch64 test target.
    QemuVirtAarch64,
    /// Lenovo ThinkPad X13s Gen 1 (Qualcomm SC8280XP).
    ThinkPadX13sGen1,
    /// Raspberry Pi 5 (Broadcom BCM2712).
    RaspberryPi5,
}

/// The early/boot console a board exposes before userspace drivers take over.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Console {
    /// A PL011 UART at this MMIO physical base. The kernel's serial shell drives it directly.
    Pl011 { base: u64 },
    /// No usable early UART reachable at a known fixed base; the boot console paints the
    /// UEFI GOP framebuffer instead. Fault dumps go to the glass (see the HAL QR path).
    Framebuffer,
}

impl Console {
    /// The PL011 base if this board boots on a UART, else `None` (framebuffer console).
    pub const fn pl011_base(self) -> Option<u64> {
        match self {
            Console::Pl011 { base } => Some(base),
            Console::Framebuffer => None,
        }
    }
}

/// The GIC major version a board's interrupt controller speaks. The kernel currently
/// implements GICv3 only; [`GicVersion::V2`] boards (Pi 5 / GIC-400) are recorded here so the
/// gap is explicit rather than an assumption.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GicVersion {
    V2,
    V3,
}

/// The runtime board-specific parameters the generic arm64 kernel/HAL consumes instead of
/// hardcoding. Grows one field per decoupled hardcode (DESIGN/017); starts with the two
/// clearest divergences — the early console and the interrupt-controller version.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoardSpec {
    /// Stable identifier, matching `imager::HardwareTarget`'s `id`.
    pub id: &'static str,
    /// Early/boot console.
    pub console: Console,
    /// Interrupt-controller major version.
    pub gic: GicVersion,
}

impl Board {
    /// The board's runtime support parameters.
    pub const fn spec(self) -> BoardSpec {
        match self {
            Board::QemuVirtAarch64 => BoardSpec {
                id: "qemu-virt-aarch64",
                // QEMU virt exposes PL011 UART0 at 0x0900_0000 (matches the historical HAL
                // hardcode this table supersedes).
                console: Console::Pl011 { base: 0x0900_0000 },
                gic: GicVersion::V3,
            },
            Board::ThinkPadX13sGen1 => BoardSpec {
                id: "thinkpad-x13s-gen1",
                // The X13s has no UART at a known fixed base; boot output paints the GOP
                // framebuffer (the console history that drove the framebuffer path).
                console: Console::Framebuffer,
                gic: GicVersion::V3,
            },
            Board::RaspberryPi5 => BoardSpec {
                id: "raspberry-pi-5",
                // UEFI GOP is the reliable early console; the 40-pin PL011 lives behind the
                // RP1 southbridge and is not a fixed-base early UART, so boot uses the glass.
                console: Console::Framebuffer,
                // BCM2712 uses GIC-400 (GICv2) — the known divergence from the GICv3 kernel.
                gic: GicVersion::V2,
            },
        }
    }

    /// The board's stable identifier (its [`BoardSpec::id`]).
    pub const fn id(self) -> &'static str {
        self.spec().id
    }

    /// Resolve a board from its identifier string (the build-time → runtime handshake).
    pub fn from_id(id: &str) -> Option<Board> {
        [
            Board::QemuVirtAarch64,
            Board::ThinkPadX13sGen1,
            Board::RaspberryPi5,
        ]
        .into_iter()
        .find(|board| board.id() == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qemu_virt_boots_on_pl011_at_the_historical_base() {
        let spec = Board::QemuVirtAarch64.spec();
        assert_eq!(spec.console, Console::Pl011 { base: 0x0900_0000 });
        assert_eq!(spec.console.pl011_base(), Some(0x0900_0000));
        assert_eq!(spec.gic, GicVersion::V3);
    }

    #[test]
    fn framebuffer_console_boards_have_no_pl011_base() {
        for board in [Board::ThinkPadX13sGen1, Board::RaspberryPi5] {
            let spec = board.spec();
            assert_eq!(spec.console, Console::Framebuffer);
            assert_eq!(spec.console.pl011_base(), None);
        }
    }

    #[test]
    fn pi5_records_the_gicv2_divergence() {
        assert_eq!(Board::RaspberryPi5.spec().gic, GicVersion::V2);
        // The two currently-supported metal/test targets are GICv3.
        assert_eq!(Board::ThinkPadX13sGen1.spec().gic, GicVersion::V3);
        assert_eq!(Board::QemuVirtAarch64.spec().gic, GicVersion::V3);
    }

    #[test]
    fn ids_are_unique_and_round_trip_through_from_id() {
        let boards = [
            Board::QemuVirtAarch64,
            Board::ThinkPadX13sGen1,
            Board::RaspberryPi5,
        ];
        for board in boards {
            assert_eq!(Board::from_id(board.id()), Some(board));
        }
        // Distinct ids.
        assert_ne!(Board::QemuVirtAarch64.id(), Board::ThinkPadX13sGen1.id());
        assert_ne!(Board::ThinkPadX13sGen1.id(), Board::RaspberryPi5.id());
        // Unknown id resolves to None.
        assert_eq!(Board::from_id("generic-uefi-x86_64"), None);
    }
}
