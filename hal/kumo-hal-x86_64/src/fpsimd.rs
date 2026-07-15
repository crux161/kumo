//j455
//j457

//! x86_64 FP/SIMD enablement + boundary-transparency proof (DEFERRED/000).
//!
//! The KUMO x86 kernel is built soft-float (`targets/x86_64-kumo-none.json`: `-sse,+soft-float`),
//! so it never emits SSE and therefore cannot itself clobber user vector (`xmm`) state while
//! servicing a syscall or interrupt. This module makes that kernel-boundary contract real and
//! provable. AVX-capable CPUs additionally receive the deliberately narrow XCR0 x87/SSE/AVX policy;
//! baseline CPUs retain SSE plus `FXSAVE64`. Per-thread ownership is handled by `ThreadContext`.
//! — CORVUS; extended by KESTREL
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
const CR4_OSXSAVE: u64 = 1 << 18;
const CPUID_XSAVE: u32 = 1 << 26;
const CPUID_AVX: u32 = 1 << 28;
const XCR0_X87: u64 = 1 << 0;
const XCR0_SSE: u64 = 1 << 1;
const XCR0_AVX: u64 = 1 << 2;
pub const AVX_XSTATE_MASK: u64 = XCR0_X87 | XCR0_SSE | XCR0_AVX;
pub const AVX_XSTATE_BYTES: u32 = 832;
const AVX_COMPONENT_BYTES: u32 = 256;
const AVX_COMPONENT_OFFSET: u32 = 576;

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

/// Whether CPUID exposes the exact standard-format AVX component that fits KUMO's state slot.
pub const fn avx_xsave_supported(
    max_basic_leaf: u32,
    leaf1_ecx: u32,
    supported_xcr0: u64,
    avx_component_bytes: u32,
    avx_component_offset: u32,
) -> bool {
    max_basic_leaf >= 0x0d
        && leaf1_ecx & (CPUID_XSAVE | CPUID_AVX) == (CPUID_XSAVE | CPUID_AVX)
        && supported_xcr0 & AVX_XSTATE_MASK == AVX_XSTATE_MASK
        && avx_component_bytes == AVX_COMPONENT_BYTES
        && avx_component_offset == AVX_COMPONENT_OFFSET
}

/// What [`prove_fpsimd_boundary`] observed: the post-enable control registers and the `xmm`
/// sentinel round-trip across the interrupt boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FpSimdReport {
    pub cr0: u64,
    pub cr4: u64,
    pub xcr0: u64,
    pub xsave_size: u32,
    pub xmm_sentinel: u64,
    pub xmm_after_isr: u64,
}

impl FpSimdReport {
    /// SSE is enabled and the sentinel survived the interrupt boundary unchanged.
    pub const fn is_transparent(self) -> bool {
        sse_enabled(self.cr0, self.cr4) && self.xmm_after_isr == self.xmm_sentinel
    }

    /// AVX is exposed only when OSXSAVE is live, XCR0 is exactly x87/SSE/AVX, and the standard
    /// save image fits the fixed 832-byte per-thread state slot.
    pub const fn avx_enabled(self) -> bool {
        self.cr4 & CR4_OSXSAVE != 0
            && self.xcr0 == AVX_XSTATE_MASK
            && self.xsave_size == AVX_XSTATE_BYTES
    }
}

#[cfg(target_os = "none")]
mod metal {
    use super::{
        avx_xsave_supported, sse_cr0, sse_cr4, FpSimdReport, AVX_XSTATE_BYTES, AVX_XSTATE_MASK,
        CR4_OSXSAVE,
    };

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

    unsafe fn read_xcr0() -> u64 {
        let low: u32;
        let high: u32;
        unsafe {
            core::arch::asm!(
                "xgetbv",
                in("ecx") 0_u32,
                out("eax") low,
                out("edx") high,
                options(nomem, nostack, preserves_flags)
            )
        };
        (u64::from(high) << 32) | u64::from(low)
    }

    unsafe fn write_xcr0(value: u64) {
        unsafe {
            core::arch::asm!(
                "xsetbv",
                in("ecx") 0_u32,
                in("eax") value as u32,
                in("edx") (value >> 32) as u32,
                options(nomem, nostack, preserves_flags)
            )
        };
    }

    /// Turn on SSE everywhere, and AVX/XSAVE only when CPUID supplies KUMO's exact bounded layout.
    fn enable() -> (u64, u32) {
        unsafe {
            let cr0 = sse_cr0(read_cr0());
            core::arch::asm!("mov cr0, {}", in(reg) cr0, options(nomem, nostack, preserves_flags));
            let mut cr4 = sse_cr4(read_cr4());
            core::arch::asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack, preserves_flags));

            let max_basic = core::arch::x86_64::__cpuid(0).eax;
            if max_basic < 0x0d {
                return (0, 0);
            }
            let leaf1 = core::arch::x86_64::__cpuid(1);
            let leafd0 = core::arch::x86_64::__cpuid_count(0x0d, 0);
            let leafd2 = core::arch::x86_64::__cpuid_count(0x0d, 2);
            if !avx_xsave_supported(
                max_basic,
                leaf1.ecx,
                (u64::from(leafd0.edx) << 32) | u64::from(leafd0.eax),
                leafd2.eax,
                leafd2.ebx,
            ) {
                return (0, 0);
            }

            cr4 |= CR4_OSXSAVE;
            core::arch::asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack, preserves_flags));
            write_xcr0(AVX_XSTATE_MASK);
            let xsave_size = core::arch::x86_64::__cpuid_count(0x0d, 0).ebx;
            if xsave_size != AVX_XSTATE_BYTES {
                write_xcr0(0x3);
                return (0x3, 0);
            }
            crate::configure_xsave(AVX_XSTATE_MASK);
            (read_xcr0(), xsave_size)
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
        let (xcr0, xsave_size) = enable();
        let cr0 = unsafe { read_cr0() };
        let cr4 = unsafe { read_cr4() };
        let xmm_after_isr = xmm_across_isr(sentinel);
        FpSimdReport {
            cr0,
            cr4,
            xcr0,
            xsave_size,
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
        xcr0: 0,
        xsave_size: 0,
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
            xcr0: 0,
            xsave_size: 0,
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

    #[test]
    fn avx_policy_requires_xsave_and_the_standard_832_byte_layout() {
        let features = CPUID_XSAVE | CPUID_AVX;
        assert!(avx_xsave_supported(
            0x0d,
            features,
            AVX_XSTATE_MASK,
            AVX_COMPONENT_BYTES,
            AVX_COMPONENT_OFFSET
        ));
        assert!(!avx_xsave_supported(
            0x0c,
            features,
            AVX_XSTATE_MASK,
            AVX_COMPONENT_BYTES,
            AVX_COMPONENT_OFFSET
        ));
        assert!(!avx_xsave_supported(
            0x0d,
            CPUID_XSAVE,
            AVX_XSTATE_MASK,
            AVX_COMPONENT_BYTES,
            AVX_COMPONENT_OFFSET
        ));
        assert!(!avx_xsave_supported(
            0x0d,
            features,
            XCR0_X87 | XCR0_SSE,
            AVX_COMPONENT_BYTES,
            AVX_COMPONENT_OFFSET
        ));
        assert!(!avx_xsave_supported(
            0x0d,
            features,
            AVX_XSTATE_MASK,
            AVX_COMPONENT_BYTES,
            AVX_COMPONENT_OFFSET + 64
        ));

        let report = FpSimdReport {
            cr0: sse_cr0(0),
            cr4: sse_cr4(0) | CR4_OSXSAVE,
            xcr0: AVX_XSTATE_MASK,
            xsave_size: AVX_XSTATE_BYTES,
            xmm_sentinel: 1,
            xmm_after_isr: 1,
        };
        assert!(report.avx_enabled());
        assert!(!FpSimdReport { xcr0: 3, ..report }.avx_enabled());
    }
}
