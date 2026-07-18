#![no_std]
//j434
//j468

//! `kumo-bsp` — the Board Support Package: the **runtime** board-specific parameters the
//! generic arm64 kernel and HAL must not hardcode.
//!
//! KUMO builds one aarch64 kernel/HAL that runs on several boards (QEMU `virt`, the ThinkPad
//! X13s, the Raspberry Pi 5, the Orange Pi 5 Plus). Those boards used to differ by scattered
//! hardcodes and feature sniffing; this crate makes each board's parameters a single typed,
//! host-testable [`BoardSpec`], keyed by a [`Board`] identity, so the generic code asks the BSP
//! instead of guessing.
//!
//! The board set mirrors `imager::HardwareTarget`'s aarch64 targets. An image is built for one
//! board and bakes its [`Board`] id into `BootInfo` — the imager writes `board = <id>` into the
//! staged `nijigumo.conf`, the loader stamps it, and the kernel resolves it with
//! [`Board::from_id`] (J460). This crate is `no_std` and allocation-free.
//!
//! **This crate is a table, not a consumer, and the HAL does not depend on it.** The kernel
//! resolves the board and *injects* what the HAL needs (`console_set_pl011_base`,
//! `gic_set_no_dtb_fallback`), so the HAL names no board address — see `DESIGN/017` §2.1 for why
//! that direction, and not the reverse, is the point.
//!
//! Where a device tree exists it is richer than this table and **wins**; the BSP carries only the
//! few fixed facts needed before or instead of DTB parsing (`DESIGN/017` §5).
//!
//! See `DESIGN/017-board-support-package.md` for the roadmap, what has actually landed, and what
//! remains owed on metal (§6).

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
    /// Xunlong Orange Pi 5 Plus (Rockchip RK3588) — the PLAN_VII documented-silicon target.
    OrangePi5Plus,
}

/// The early/boot console a board exposes before userspace drivers take over.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Console {
    /// A PL011 UART at this MMIO physical base. The kernel's serial shell drives it directly.
    Pl011 { base: u64 },
    /// A Synopsys DW-APB UART (16550-class) at this MMIO physical base, with 32-bit registers
    /// at stride 4 (`reg-io-width = <4>`, `reg-shift = <2>`) — the Rockchip debug UART shape.
    /// The HAL 8250 backend that drives it is PLAN_VII R3; until it lands this variant only
    /// records the board fact.
    Dw8250 { base: u64 },
    /// No usable early UART reachable at a known fixed base; the boot console paints the
    /// UEFI GOP framebuffer instead. Fault dumps go to the glass (see the HAL QR path).
    Framebuffer,
}

impl Console {
    /// The PL011 base if this board boots on a PL011, else `None`.
    pub const fn pl011_base(self) -> Option<u64> {
        match self {
            Console::Pl011 { base } => Some(base),
            Console::Dw8250 { .. } | Console::Framebuffer => None,
        }
    }

    /// The DW-APB/16550-class base if this board boots on one, else `None`.
    pub const fn dw8250_base(self) -> Option<u64> {
        match self {
            Console::Dw8250 { base } => Some(base),
            Console::Pl011 { .. } | Console::Framebuffer => None,
        }
    }
}

/// The GIC major version a board's interrupt controller speaks. Both are implemented: GICv3 for
/// the X13s/QEMU and GICv2 (GIC-400) for the Pi 5, since J107/J176/J212.
///
/// **This is the board's documented version, not the authority.** When the firmware publishes a
/// device tree, the GIC node's `compatible` names the version and wins — it knows more than this
/// table does. Without a DT, a board that names only GICC or only GICR fixes its interface by
/// construction. If it names both, the HAL reads `ID_AA64PFR0_EL1.GIC` rather than trusting this
/// field: QEMU's version is a *launch* property (`-machine virt,gic-version=`), not a board
/// property. Treat this as documentation and as a cross-check, never as the selector (J462).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GicVersion {
    V2,
    V3,
}

/// Where a board's GIC lives when the firmware publishes **no** device tree — the one case where
/// the BSP must carry MMIO bases, because there is no richer source to read them from
/// (DESIGN/017 §5: "the few fixed parameters needed before/instead of DTB parsing").
///
/// A board may populate one secondary when its controller is fixed, or both when the runtime
/// environment chooses between architectures (QEMU). The HAL resolves the latter from the PE.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GicFallback {
    /// GIC distributor base. Architecture selection comes from the available secondary-interface
    /// shape, with the PE capability resolving only a board that supplies both GICC and GICR.
    pub distributor_base: u64,
    /// GICv3 redistributor base; `None` if this board never exposes a GICv3.
    pub redistributor_base: Option<u64>,
    /// GICv2 CPU-interface base; `None` if this board never exposes a GICv2.
    pub cpu_base: Option<u64>,
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
    /// Interrupt-controller major version, as documented for this board. Not the selector — see
    /// [`GicVersion`].
    pub gic: GicVersion,
    /// Where the GIC lives when the firmware publishes no device tree. `None` for boards whose
    /// boot path guarantees one. A present DTB is richer and stays authoritative
    /// (DESIGN/017 §5); this is only the board-scoped escape hatch when the handoff omits it.
    pub gic_fallback: Option<GicFallback>,
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
                // QEMU virt under OVMF publishes NO device tree ("device tree : absent"), so this
                // is the one board that must name its GIC here. Addresses are QEMU's fixed virt
                // memmap: VIRT_GIC_DIST, VIRT_GIC_CPU, VIRT_GIC_REDIST. Both secondaries are
                // populated because `-machine virt,gic-version=2|3` picks which exists at run
                // time; the HAL reads the PE's GIC system-register capability to find out (J462).
                gic_fallback: Some(GicFallback {
                    distributor_base: 0x0800_0000,
                    redistributor_base: Some(0x080a_0000),
                    cpu_base: Some(0x0801_0000),
                }),
            },
            Board::ThinkPadX13sGen1 => BoardSpec {
                id: "thinkpad-x13s-gen1",
                // The X13s has no UART at a known fixed base; boot output paints the GOP
                // framebuffer (the console history that drove the framebuffer path).
                console: Console::Framebuffer,
                gic: GicVersion::V3,
                // Nijigumo stages the X13s DTB on the ESP, so a device tree is always present.
                gic_fallback: None,
            },
            Board::RaspberryPi5 => BoardSpec {
                id: "raspberry-pi-5",
                // The Pi 5 has TWO UARTs and this entry used to know only the wrong one. It said
                // "the 40-pin PL011 lives behind the RP1 southbridge and is not a fixed-base
                // early UART, so boot uses the glass" — true of `uart0`, and it misses `uart10`.
                //
                // Per the official `bcm2712-rpi-5-b.dtb` (resources/TARTLET):
                //   console = "/soc/serial@7d001000";                       <- uart10, SoC
                //   uart0   = "/axi/pcie@120000/rp1/serial@30000";          <- 40-pin, behind RP1
                // and uart10 is `compatible = "arm,pl011"`, `status = "okay"`, `reg =
                // <0x7d001000 0x200>`. The soc node's `ranges = <0x7c000000 0x10 0x7c000000
                // 0x4000000>` translates that to CPU-physical 0x10_7D00_1000. It is the SoC's own
                // PL011 on the dedicated 3-pin debug connector — a fixed base, reachable with no
                // PCIe and no RP1, and the board's *designated* console.
                //
                // EDK2 owns it as its console before we do, so the HAL inherits its baud rather
                // than reprogramming a clock it cannot know (see `pl011_init`). The GOP is still
                // published and still painted — the console writes to both sinks (J465).
                console: Console::Pl011 {
                    base: 0x10_7D00_1000,
                },
                // BCM2712 uses GIC-400 (GICv2), implemented since J107/J176/J212.
                gic: GicVersion::V2,
                // Raspberry Pi firmware loads bcm2712-rpi-5-b.dtb, but RPi5 UEFI can boot in an
                // ACPI-only table mode and then publish no EFI DT configuration table to KUMO.
                // Keep the DT authoritative whenever it is handed off; otherwise use the fixed
                // BCM2712 GIC-400 bases from the official board DT: distributor
                // 0x10_7fff9000 and CPU interface 0x10_7fffa000. There is no GICv3
                // redistributor on this board. — KESTREL 2026-07-17
                gic_fallback: Some(GicFallback {
                    distributor_base: 0x10_7fff_9000,
                    redistributor_base: None,
                    cpu_base: Some(0x10_7fff_a000),
                }),
            },
            Board::OrangePi5Plus => BoardSpec {
                id: "orange-pi-5-plus",
                // RK3588 debug UART: uart2 @ 0xfeb50000, snps,dw-apb-uart, boot-chain baud
                // 1500000 (TRM part1 ch19; rk3588-base.dtsi). The accessible 3-pin header is
                // the whole point of this board (PLAN_VII §0). — CORVUS
                console: Console::Dw8250 { base: 0xfeb5_0000 },
                // GIC600 (GICv3): GICD 0xfe600000, GICR 0xfe680000.
                gic: GicVersion::V3,
                // EDK2-rk3588 / U-Boot publish a device tree; PLAN_VII R1 bundles one besides.
                gic_fallback: None,
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
            Board::OrangePi5Plus,
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
        // The X13s is the only board with no UART at any fixed base, and so the reason the HAL's
        // UART sink must be inert by default: it is the board J182 actually hard-hung. Sentinel.
        let spec = Board::ThinkPadX13sGen1.spec();
        assert_eq!(spec.console, Console::Framebuffer);
        assert_eq!(spec.console.pl011_base(), None);
    }

    /// The Pi 5's designated console is its **SoC** PL011 (`uart10`), not the 40-pin UART behind
    /// RP1. Per the official `bcm2712-rpi-5-b.dtb`: `console = "/soc/serial@7d001000"`, while
    /// `uart0 = "/axi/pcie@120000/rp1/serial@30000"`; the soc node's
    /// `ranges = <0x7c000000 0x10 0x7c000000 0x4000000>` puts uart10 at CPU-physical
    /// 0x10_7D00_1000.
    ///
    /// This entry claimed `Framebuffer` until J465 — the comment described `uart0` and never knew
    /// `uart10` existed — which cost the Pi 5 a serial console it has had all along.
    #[test]
    fn pi5_boots_on_its_soc_debug_pl011_not_the_rp1_uart() {
        let spec = Board::RaspberryPi5.spec();
        assert_eq!(
            spec.console,
            Console::Pl011 {
                base: 0x10_7D00_1000
            }
        );
        assert_eq!(spec.console.pl011_base(), Some(0x10_7D00_1000));
        // Not QEMU's base, and not an RP1 offset.
        assert_ne!(spec.console.pl011_base(), Some(0x0900_0000));
    }

    /// QEMU and Pi 5 can both reach KUMO without an EFI DT configuration table. QEMU names both
    /// secondary interfaces because `gic-version=` decides which exists; the Pi 5 names only its
    /// documented GIC-400 CPU interface. Boards with guaranteed DT handoff stay empty.
    #[test]
    fn only_boards_with_a_real_no_dtb_path_carry_gic_fallbacks() {
        let qemu = Board::QemuVirtAarch64.spec().gic_fallback.unwrap();
        assert_eq!(qemu.distributor_base, 0x0800_0000);
        assert_eq!(qemu.redistributor_base, Some(0x080a_0000));
        assert_eq!(qemu.cpu_base, Some(0x0801_0000));

        let pi5 = Board::RaspberryPi5.spec().gic_fallback.unwrap();
        assert_eq!(pi5.distributor_base, 0x10_7fff_9000);
        assert_eq!(pi5.redistributor_base, None);
        assert_eq!(pi5.cpu_base, Some(0x10_7fff_a000));

        // These boot paths guarantee a DT and therefore need no lower-authority fallback.
        for board in [Board::ThinkPadX13sGen1, Board::OrangePi5Plus] {
            assert_eq!(board.spec().gic_fallback, None, "board {}", board.id());
        }
    }

    #[test]
    fn pi5_records_the_gicv2_divergence() {
        assert_eq!(Board::RaspberryPi5.spec().gic, GicVersion::V2);
        // The other supported metal/test targets are GICv3.
        assert_eq!(Board::ThinkPadX13sGen1.spec().gic, GicVersion::V3);
        assert_eq!(Board::QemuVirtAarch64.spec().gic, GicVersion::V3);
        assert_eq!(Board::OrangePi5Plus.spec().gic, GicVersion::V3);
    }

    #[test]
    fn orange_pi_5_plus_boots_on_the_rk3588_debug_uart() {
        // PLAN_VII R1: the RK3588 debug UART is uart2 @ 0xfeb50000, a DW-APB 16550-class
        // device — NOT a PL011 — per TRM part1 ch19 and rk3588-base.dtsi.
        let spec = Board::OrangePi5Plus.spec();
        assert_eq!(spec.console, Console::Dw8250 { base: 0xfeb5_0000 });
        assert_eq!(spec.console.dw8250_base(), Some(0xfeb5_0000));
        assert_eq!(spec.console.pl011_base(), None);
        // And the PL011 boards report no DW-APB base.
        assert_eq!(Board::QemuVirtAarch64.spec().console.dw8250_base(), None);
    }

    #[test]
    fn ids_are_unique_and_round_trip_through_from_id() {
        let boards = [
            Board::QemuVirtAarch64,
            Board::ThinkPadX13sGen1,
            Board::RaspberryPi5,
            Board::OrangePi5Plus,
        ];
        for board in boards {
            assert_eq!(Board::from_id(board.id()), Some(board));
        }
        // Distinct ids.
        for (i, a) in boards.iter().enumerate() {
            for b in boards.iter().skip(i + 1) {
                assert_ne!(a.id(), b.id());
            }
        }
        // Unknown id resolves to None.
        assert_eq!(Board::from_id("generic-uefi-x86_64"), None);
    }
}
