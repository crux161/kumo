//! Turning a fault's raw numbers into a sentence.
//!
//! Diagnosing the j-series stack fault took four manual steps at a host shell: disassemble the
//! staged ELF to find what `ELR` pointed at, pull the section table to find where `.bss` ended,
//! locate the heap's backing array by symbol, and subtract. Every one of those numbers was already
//! known to the kernel at the moment it died. It printed them in hex and said nothing about them.
//!
//! TempleOS's debugger is the standing argument that this is a choice rather than a constraint: it
//! reports `!!! Bad MAlloc !!!` with a task name and a resolved call chain, on the machine, at the
//! moment of death. This module is the first two lessons from it — **say what the syndrome means**,
//! and **say where each address lives** — both as pure functions over values the fault path already
//! has, so they are host-testable and cannot themselves fault.
//!
//! Symbolisation and the frame-pointer walk are the other two lessons and are planned separately;
//! they need build-side support this does not.

//j493

/// The exception class and, for aborts, what actually went wrong.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Syndrome {
    /// The exception class in English.
    pub class: &'static str,
    /// For aborts, the fault status in English; empty when the class says everything.
    pub detail: &'static str,
    /// For data aborts: true on write, false on read. `None` when not applicable.
    pub write: Option<bool>,
}

/// Decode an AArch64 `ESR_EL1`.
///
/// Only the classes this kernel can actually take are named; anything else reports its raw class so
/// an unrecognised syndrome is still identifiable rather than silently mislabelled.
pub fn decode_esr(esr: u64) -> Syndrome {
    let ec = (esr >> 26) & 0x3f;
    let iss = esr & 0x01ff_ffff;
    let is_abort = matches!(ec, 0x20 | 0x21 | 0x24 | 0x25);
    let class = match ec {
        0x00 => "unknown / undefined instruction",
        0x0e => "illegal execution state",
        0x15 => "SVC from EL0",
        0x18 => "trapped MSR/MRS",
        0x20 => "instruction abort from EL0",
        0x21 => "instruction abort from EL1",
        0x22 => "PC alignment fault",
        0x24 => "data abort from EL0",
        0x25 => "data abort from EL1",
        0x26 => "SP alignment fault",
        0x2c => "floating-point exception",
        0x30 | 0x31 => "breakpoint",
        0x3c => "BRK instruction",
        _ => "unrecognised exception class",
    };
    let detail = if is_abort {
        fault_status(iss & 0x3f)
    } else {
        ""
    };
    // WnR (ISS bit 6) is only meaningful for data aborts, and only when the fault is not on a
    // stage-2 walk — which this kernel never takes.
    let write = if matches!(ec, 0x24 | 0x25) {
        Some(iss & (1 << 6) != 0)
    } else {
        None
    };
    Syndrome {
        class,
        detail,
        write,
    }
}

/// DFSC/IFSC — the abort's fault status code.
fn fault_status(dfsc: u64) -> &'static str {
    match dfsc {
        0x00 => "address size fault, level 0",
        0x01 => "address size fault, level 1",
        0x02 => "address size fault, level 2",
        0x03 => "address size fault, level 3",
        0x04 => "translation fault, level 0 (nothing mapped)",
        0x05 => "translation fault, level 1 (nothing mapped)",
        0x06 => "translation fault, level 2 (nothing mapped)",
        0x07 => "translation fault, level 3 (nothing mapped)",
        0x09 => "access flag fault, level 1",
        0x0a => "access flag fault, level 2",
        0x0b => "access flag fault, level 3",
        0x0d => "permission fault, level 1",
        0x0e => "permission fault, level 2",
        0x0f => "permission fault, level 3",
        0x10 => "synchronous external abort",
        0x21 => "alignment fault",
        0x30 => "TLB conflict",
        _ => "unrecognised fault status",
    }
}

/// Whether the fault came from EL0 (userspace) rather than from kernel code.
///
/// Load-bearing for how a fault is *described*. The kernel's region map covers the kernel image,
/// its heap and its stacks — none of which say anything about a user address. Applying it to an EL0
/// fault produces three confident "UNMAPPED - below every known region" lines and, worse, fires the
/// out-of-arena stack warning at a *user* stack pointer, blaming the allocator for what is usually
/// a userspace bug. That is exactly what happened on the first `threads` boot: the shout said
/// "suspect the free list" when the real fault was a stack top computed as a base.
pub const fn is_from_el0(esr: u64) -> bool {
    // 0x20 = instruction abort from a lower EL, 0x24 = data abort from a lower EL.
    matches!((esr >> 26) & 0x3f, 0x20 | 0x24)
}

/// A named span of the kernel's address space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Region {
    Image,
    BootStack,
    Heap,
    Physmap,
}

impl Region {
    pub const fn name(self) -> &'static str {
        match self {
            Region::Image => "kernel image",
            Region::BootStack => "boot stack",
            Region::Heap => "kernel heap",
            Region::Physmap => "physmap",
        }
    }
}

/// Where the kernel's regions actually are. Supplied by the target; a plain struct so every
/// judgement about an address is a pure function that can be tested against real crash values.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RegionMap {
    pub image: (u64, u64),
    pub boot_stack: (u64, u64),
    pub heap: (u64, u64),
    /// Physmap has no end the kernel can state, so it is a base only.
    pub physmap_base: u64,
}

/// What an address turned out to be.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Placement {
    /// Inside a known region, at `offset` from its start.
    Inside { region: Region, offset: u64 },
    /// Not in any region, but `distance` bytes past the end of the nearest one below it. This is
    /// the case that names an overrun: a stack pointer that walked out of the heap, a pointer that
    /// ran off the end of the image.
    Past { region: Region, distance: u64 },
    /// Below everything the kernel knows about.
    Nowhere,
}

/// Place `addr` in the kernel's address space.
///
/// Regions are checked innermost-first: the boot stack and the heap both live inside the image's
/// span, and reporting "kernel image" for a heap address would throw away the distinction that
/// matters. When nothing contains it, the *nearest region below* is reported with the distance
/// past its end — which is the form that turns "an address" into "an overrun of something".
pub fn place(map: &RegionMap, addr: u64) -> Placement {
    for (region, (start, end)) in [
        (Region::BootStack, map.boot_stack),
        (Region::Heap, map.heap),
        (Region::Image, map.image),
    ] {
        if start != end && addr >= start && addr < end {
            return Placement::Inside {
                region,
                offset: addr - start,
            };
        }
    }
    if map.physmap_base != 0 && addr >= map.physmap_base {
        return Placement::Inside {
            region: Region::Physmap,
            offset: addr - map.physmap_base,
        };
    }
    // Nearest region below, so an overrun is described as an overrun of the thing it left.
    let mut best: Option<(Region, u64)> = None;
    for (region, (start, end)) in [
        (Region::BootStack, map.boot_stack),
        (Region::Heap, map.heap),
        (Region::Image, map.image),
    ] {
        if start == end || addr < end {
            continue;
        }
        let distance = addr - end;
        if best.map(|(_, d)| distance < d).unwrap_or(true) {
            best = Some((region, distance));
        }
    }
    match best {
        Some((region, distance)) => Placement::Past { region, distance },
        None => Placement::Nowhere,
    }
}

/// Whether a kernel stack pointer is anywhere it could legitimately be.
///
/// `place` cannot know that an address is *supposed* to be a stack — it reports geography, not
/// intent. Only the fault path knows `SP` is a stack pointer, and that single extra fact converts
/// "an address outside the image" into a named finding: kernel thread stacks are heap-allocated
/// `Vec<u8>`s, so a kernel `SP` outside both the heap and the boot stack means the allocator
/// handed out a block it did not own. That is this kernel's `!!! Bad MAlloc !!!`.
pub fn kernel_stack_is_sane(map: &RegionMap, sp: u64) -> bool {
    matches!(
        place(map, sp),
        Placement::Inside {
            region: Region::Heap | Region::BootStack,
            ..
        }
    )
}

/// The boot stack's span, published by the entry stub (which is the only code that knows it).
/// `(0, 0)` until then; an unset region is simply not matched.
static BOOT_STACK: (core::sync::atomic::AtomicU64, core::sync::atomic::AtomicU64) = (
    core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0),
);

pub fn set_boot_stack(lo: u64, hi: u64) {
    BOOT_STACK
        .0
        .store(lo, core::sync::atomic::Ordering::Relaxed);
    BOOT_STACK
        .1
        .store(hi, core::sync::atomic::Ordering::Relaxed);
}

/// This kernel's live region map.
///
/// Every value here is a link-time constant or a published static — no allocation, no locks, no
/// borrows — because this runs inside the fault path, where the one unforgivable bug is a
/// diagnostic that faults while reporting a fault.
#[cfg(all(target_os = "none", target_arch = "aarch64"))]
pub fn current_map() -> RegionMap {
    extern "C" {
        static __image_start: u8;
        static __image_end: u8;
    }
    RegionMap {
        image: (
            core::ptr::addr_of!(__image_start) as u64,
            core::ptr::addr_of!(__image_end) as u64,
        ),
        boot_stack: (
            BOOT_STACK.0.load(core::sync::atomic::Ordering::Relaxed),
            BOOT_STACK.1.load(core::sync::atomic::Ordering::Relaxed),
        ),
        heap: crate::mm::heap::range(),
        physmap_base: 0xffff_9000_0000_0000,
    }
}

#[cfg(not(all(target_os = "none", target_arch = "aarch64")))]
pub fn current_map() -> RegionMap {
    RegionMap {
        heap: crate::mm::heap::range(),
        ..RegionMap::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact values the Orange Pi 5 Plus reported, and the layout of the image it died in.
    /// This test is the crash: if it ever stops describing it correctly, the decoder is wrong.
    fn opi5_map() -> RegionMap {
        RegionMap {
            image: (0xffff_8000_4800_0000, 0xffff_8000_488d_b760),
            boot_stack: (0xffff_8000_480c_b000, 0xffff_8000_480d_b000),
            heap: (0xffff_8000_480d_b010, 0xffff_8000_488d_b010),
            physmap_base: 0xffff_9000_0000_0000,
        }
    }

    #[test]
    fn the_stack_fault_decodes_to_its_diagnosis() {
        let s = decode_esr(0x9600_0007);
        assert_eq!(s.class, "data abort from EL1");
        assert_eq!(s.detail, "translation fault, level 3 (nothing mapped)");
        assert_eq!(s.write, Some(false)); // it was a read

        let map = opi5_map();
        // ELR landed in the IRQ stub, inside .text.
        assert_eq!(
            place(&map, 0xffff_8000_4800_104c),
            Placement::Inside {
                region: Region::Image,
                offset: 0x104c
            }
        );
        // FAR was past the end of the whole image — nothing is mapped there.
        assert_eq!(
            place(&map, 0xffff_8000_488d_c050),
            Placement::Past {
                region: Region::Image,
                distance: 0x8f0
            }
        );
        // SP was past the image end too — `place` reports the *nearest* region below, and the
        // image ends after the heap does, so this is the crisper of the two true statements.
        assert_eq!(
            place(&map, 0xffff_8000_488d_bf70),
            Placement::Past {
                region: Region::Image,
                distance: 0x810
            }
        );
        // But geography alone does not name the bug. This does:
        assert!(!kernel_stack_is_sane(&map, 0xffff_8000_488d_bf70));
    }

    #[test]
    fn a_stack_pointer_is_only_sane_inside_a_stack_or_the_heap() {
        let map = opi5_map();
        // A thread stack: a heap-allocated Vec, so inside the heap is correct.
        assert!(kernel_stack_is_sane(&map, 0xffff_8000_4820_0000));
        // The boot stack.
        assert!(kernel_stack_is_sane(&map, 0xffff_8000_480d_0000));
        // .text is not somewhere a stack pointer may be, however well-mapped it is.
        assert!(!kernel_stack_is_sane(&map, 0xffff_8000_4800_104c));
        // Nor is the physmap window.
        assert!(!kernel_stack_is_sane(&map, 0xffff_9000_0000_1000));
    }

    #[test]
    fn a_heap_address_reports_as_heap_not_as_image() {
        // The heap lives inside the image's span; the inner region has to win or the distinction
        // that matters — "this is allocated memory" — is thrown away.
        let map = opi5_map();
        assert_eq!(
            place(&map, 0xffff_8000_4820_0000),
            Placement::Inside {
                region: Region::Heap,
                offset: 0xffff_8000_4820_0000 - 0xffff_8000_480d_b010
            }
        );
    }

    #[test]
    fn the_boot_stack_is_distinguished_from_the_heap_beside_it() {
        // These two are adjacent with 16 bytes between them; conflating them would hide a boot
        // stack overrun as heap corruption.
        let map = opi5_map();
        assert!(matches!(
            place(&map, 0xffff_8000_480d_0000),
            Placement::Inside {
                region: Region::BootStack,
                ..
            }
        ));
        assert!(matches!(
            place(&map, 0xffff_8000_480d_b100),
            Placement::Inside {
                region: Region::Heap,
                ..
            }
        ));
    }

    #[test]
    fn physmap_addresses_are_named() {
        let map = opi5_map();
        assert_eq!(
            place(&map, 0xffff_9000_0000_1000),
            Placement::Inside {
                region: Region::Physmap,
                offset: 0x1000
            }
        );
    }

    #[test]
    fn a_null_or_low_address_is_nowhere() {
        assert_eq!(place(&opi5_map(), 0), Placement::Nowhere);
        assert_eq!(place(&opi5_map(), 0x1000), Placement::Nowhere);
    }

    #[test]
    fn an_empty_region_is_not_matched() {
        // Before the heap is initialised its bounds are (0, 0); an address must not be reported as
        // living in a region that does not exist yet.
        let map = RegionMap {
            image: (0x1000, 0x2000),
            ..RegionMap::default()
        };
        assert!(matches!(
            place(&map, 0x1500),
            Placement::Inside {
                region: Region::Image,
                ..
            }
        ));
    }

    #[test]
    fn el0_and_el1_aborts_are_distinguished() {
        // The values from the two real faults this system has produced.
        assert!(is_from_el0(0x9200_0047)); // threads: user stack below its mapping
        assert!(!is_from_el0(0x9600_0007)); // the IRQ-stack fault: kernel side
        assert!(is_from_el0(0x8200_0007)); // instruction abort from EL0
        assert!(!is_from_el0(0x8600_0007)); // instruction abort from EL1
    }

    #[test]
    fn common_syndromes_are_named() {
        assert_eq!(decode_esr(0x5600_0000).class, "SVC from EL0");
        assert_eq!(decode_esr(0x9200_004f).write, Some(true));
        assert_eq!(decode_esr(0x8600_000f).detail, "permission fault, level 3");
        assert_eq!(decode_esr(0x9600_0021).detail, "alignment fault");
        // An unknown class still identifies itself rather than being mislabelled.
        assert_eq!(
            decode_esr(0x0400_0000).class,
            "unrecognised exception class"
        );
    }
}
