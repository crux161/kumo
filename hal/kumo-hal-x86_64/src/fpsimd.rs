//j455

//! x86_64 FP/SIMD (SSE) enablement + boundary-transparency proof (DEFERRED/000).
//!
//! The KUMO x86 kernel is built soft-float (`targets/x86_64-kumo-none.json`: `-sse,+soft-float`),
//! so it never emits SSE and therefore cannot itself clobber user vector (`xmm`) state while
//! servicing a syscall or interrupt. This module makes that kernel-boundary contract real and
//! provable. It does **not** yet isolate the FP state of multiple FP-using user threads; per-thread
//! `FXSAVE`/`XSAVE` ownership remains a separate DEFERRED/000 gate. — CORVUS; audited by KESTREL
//!
//! 1. **Enable SSE in hardware** (`CR0.MP=1`, `CR0.EM=0`, `CR0.TS=0`, `CR4.OSFXSR=1`,
//!    `CR4.OSXMMEXCPT=1`) so the current single user execution path may use `xmm` without `#UD` or
//!    `#NM`. The kernel stays soft-float; enabling the hardware bit costs nothing while it is unused.
//! 2. **Prove the interrupt-entry boundary is transparent**: load a sentinel into `xmm0`, take an
//!    `int3` through the IDT/`isr_common`, and read `xmm0` back unchanged. `isr_common` saves only
//!    GP registers and the handler is soft-float, so a preserved `xmm0` proves the kernel boundary
//!    does not disturb vector state.
//!
//! The SSE instructions are emitted as raw bytes (`movq xmm0, rax` / `movq rax, xmm0`) so they
//! assemble even though the target disables the `sse` feature. The control-register encoding is
//! pure and host-tested.

const CR0_MP: u64 = 1 << 1;
const CR0_EM: u64 = 1 << 2;
const CR0_TS: u64 = 1 << 3;
const CR4_OSFXSR: u64 = 1 << 9;
const CR4_OSXMMEXCPT: u64 = 1 << 10;

/// `CR0` adjusted for the SSE contract: monitor-coprocessor set, x87 emulation and task-switched
/// cleared. Other bits (paging, protection, …) are preserved by the read-modify-write caller.
pub const fn sse_cr0(cr0: u64) -> u64 {
    (cr0 | CR0_MP) & !(CR0_EM | CR0_TS)
}

/// `CR4` adjusted for the SSE contract: OS `FXSAVE`/`FXRSTOR` support + unmasked SIMD FP
/// exceptions. Other bits (PAE, …) are preserved by the caller.
pub const fn sse_cr4(cr4: u64) -> u64 {
    cr4 | CR4_OSFXSR | CR4_OSXMMEXCPT
}

/// Whether these control registers report SSE enabled per the contract.
pub const fn sse_enabled(cr0: u64, cr4: u64) -> bool {
    cr0 & (CR0_EM | CR0_TS) == 0
        && cr0 & CR0_MP != 0
        && cr4 & CR4_OSFXSR != 0
        && cr4 & CR4_OSXMMEXCPT != 0
}

/// What [`prove_fpsimd_boundary`] observed: the post-enable control registers and the `xmm`
/// sentinel round-trip across the interrupt boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FpSimdReport {
    pub cr0: u64,
    pub cr4: u64,
    pub xmm_sentinel: u64,
    pub xmm_after_isr: u64,
}

impl FpSimdReport {
    /// SSE is enabled and the sentinel survived the interrupt boundary unchanged.
    pub const fn is_transparent(self) -> bool {
        sse_enabled(self.cr0, self.cr4) && self.xmm_after_isr == self.xmm_sentinel
    }
}

#[cfg(target_os = "none")]
mod metal {
    use super::{sse_cr0, sse_cr4, FpSimdReport};

    unsafe fn read_cr0() -> u64 {
        let value;
        unsafe {
            core::arch::asm!("mov {}, cr0", out(reg) value, options(nomem, nostack, preserves_flags))
        };
        value
    }

    unsafe fn read_cr4() -> u64 {
        let value;
        unsafe {
            core::arch::asm!("mov {}, cr4", out(reg) value, options(nomem, nostack, preserves_flags))
        };
        value
    }

    /// Turn on SSE, preserving every unrelated control bit (paging, PAE, …).
    fn enable() {
        unsafe {
            let cr0 = sse_cr0(read_cr0());
            core::arch::asm!("mov cr0, {}", in(reg) cr0, options(nomem, nostack, preserves_flags));
            let cr4 = sse_cr4(read_cr4());
            core::arch::asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack, preserves_flags));
        }
    }

    /// Load `sentinel` into `xmm0`, cross the interrupt boundary with `int3`, and read `xmm0` back.
    /// The `movq` between `rax` and `xmm0` is emitted as raw bytes so it assembles under `-sse`.
    fn xmm_across_isr(sentinel: u64) -> u64 {
        let after: u64;
        unsafe {
            core::arch::asm!(
                ".byte 0x66, 0x48, 0x0f, 0x6e, 0xc0", // movq xmm0, rax
                "int3",
                ".byte 0x66, 0x48, 0x0f, 0x7e, 0xc0", // movq rax, xmm0
                inout("rax") sentinel => after,
            );
        }
        after
    }

    pub fn prove(sentinel: u64) -> FpSimdReport {
        enable();
        let cr0 = unsafe { read_cr0() };
        let cr4 = unsafe { read_cr4() };
        let xmm_after_isr = xmm_across_isr(sentinel);
        FpSimdReport {
            cr0,
            cr4,
            xmm_sentinel: sentinel,
            xmm_after_isr,
        }
    }
}

/// Enable SSE and prove the interrupt-entry boundary preserves `xmm` state.
#[cfg(target_os = "none")]
pub fn prove_fpsimd_boundary(sentinel: u64) -> FpSimdReport {
    metal::prove(sentinel)
}

/// Host build: no CPU control registers. Report the contract as satisfied for shape-only callers.
#[cfg(not(target_os = "none"))]
pub fn prove_fpsimd_boundary(sentinel: u64) -> FpSimdReport {
    FpSimdReport {
        cr0: sse_cr0(0),
        cr4: sse_cr4(0),
        xmm_sentinel: sentinel,
        xmm_after_isr: sentinel,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_control_bits_follow_the_contract() {
        // MP set, EM + TS cleared, unrelated bits preserved.
        assert_eq!(sse_cr0(0), CR0_MP);
        assert_eq!(sse_cr0(CR0_EM), CR0_MP);
        assert_eq!(sse_cr0(CR0_TS), CR0_MP);
        assert_eq!(sse_cr0(CR0_EM | CR0_TS), CR0_MP);
        assert_eq!(sse_cr0(1 << 31), (1 << 31) | CR0_MP); // PG preserved
                                                          // OSFXSR + OSXMMEXCPT set, unrelated bits preserved.
        assert_eq!(sse_cr4(0), CR4_OSFXSR | CR4_OSXMMEXCPT);
        assert_eq!(sse_cr4(1 << 5), (1 << 5) | CR4_OSFXSR | CR4_OSXMMEXCPT); // PAE preserved
    }

    #[test]
    fn sse_enabled_requires_all_four_conditions() {
        assert!(sse_enabled(sse_cr0(0), sse_cr4(0)));
        assert!(!sse_enabled(CR0_EM | CR0_MP, sse_cr4(0))); // EM still set
        assert!(!sse_enabled(CR0_TS | CR0_MP, sse_cr4(0))); // TS still set
        assert!(!sse_enabled(sse_cr0(0), CR4_OSFXSR)); // OSXMMEXCPT missing
        assert!(!sse_enabled(0, sse_cr4(0))); // MP missing
    }

    #[test]
    fn transparent_requires_enabled_and_a_preserved_sentinel() {
        let good = FpSimdReport {
            cr0: sse_cr0(0),
            cr4: sse_cr4(0),
            xmm_sentinel: 0x0f0f_5555_aaaa_f00d,
            xmm_after_isr: 0x0f0f_5555_aaaa_f00d,
        };
        assert!(good.is_transparent());

        let clobbered = FpSimdReport {
            xmm_after_isr: 0,
            ..good
        };
        assert!(!clobbered.is_transparent());

        let disabled = FpSimdReport { cr4: 0, ..good };
        assert!(!disabled.is_transparent());
    }
}
