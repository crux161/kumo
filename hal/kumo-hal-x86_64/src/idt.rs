//j435
//j436
//j439

//! x86_64 Interrupt Descriptor Table + CPU-exception handlers — "the Tower" for AMD64.
//!
//! The aarch64 kernel's first act in `stage_a` is to install its exception vectors so faults are
//! caught and visible. The x86_64 side never had that: P10-c parked the IDT behind a
//! `static`-in-`.bss` link error, so `install_exception_vectors` was a no-op and any CPU exception
//! triple-faulted the machine into a silent reboot. This module makes it real.
//!
//! The blocker was a *non-zero* IDT static: constant handler addresses put bytes in `.bss`, which
//! the Multiboot a.out kludge treats as NOBITS (zero-filled) — the descriptors never reached
//! memory. The fix is the standard one: a **zero-initialized** IDT (pure `.bss`) that
//! [`install`] fills at runtime from the assembly stub table, then loads with `lidt`.
//!
//! The descriptor *encoding* is pure and host-tested; the stubs, `lidt`, and the fault round-trip
//! are proven under `qemu-system-x86_64` by executing `int3` and observing it caught **and
//! resumed**. Vectors 0..31 (CPU exceptions) are covered here. — CORVUS
//!
//! j436 extends the same table and normalized frame through vectors 32..47 for the remapped legacy
//! PIC. IRQ dispatch stays silent and delegates acknowledgement to `legacy_irq`, so a timer tick
//! cannot re-enter the serial console while ordinary kernel logging is in progress. — KESTREL
//!
//! j439 extends the table through vector 63 for the x2APIC timer at 48 and its spurious vector at
//! 63. Local-APIC dispatch precedes the legacy range and acknowledges only real timer interrupts.
//! — KESTREL 2026-07-14

/// One 16-byte x86_64 IDT gate descriptor (Intel SDM Vol.3 §6.14.1).
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdtEntry64 {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

/// Present, DPL=0, 64-bit interrupt gate (`P=1`, `type=0xE`) — masks IF on entry.
pub const GATE_INTERRUPT_KERNEL: u8 = 0x8E;
/// The ring-0 code selector from the long-mode trampoline GDT (`kernel/src/main.rs`).
pub const KERNEL_CS: u16 = 0x08;

impl IdtEntry64 {
    pub const ZERO: Self = Self {
        offset_low: 0,
        selector: 0,
        ist: 0,
        type_attr: 0,
        offset_mid: 0,
        offset_high: 0,
        reserved: 0,
    };

    /// Build a gate for `handler`, splitting its 64-bit address across the three offset fields.
    /// `ist` selects an Interrupt Stack Table slot (0 = none).
    pub const fn new(handler: u64, selector: u16, ist: u8, type_attr: u8) -> Self {
        Self {
            offset_low: handler as u16,
            selector,
            ist: ist & 0x7,
            type_attr,
            offset_mid: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            reserved: 0,
        }
    }

    /// Recompose the handler address from the split offset fields (for tests/introspection).
    pub const fn handler(self) -> u64 {
        self.offset_low as u64
            | ((self.offset_mid as u64) << 16)
            | ((self.offset_high as u64) << 32)
    }

    pub const fn present(self) -> bool {
        self.type_attr & 0x80 != 0
    }
}

/// The IDT limit/base operand for `lidt`.
#[cfg(target_os = "none")]
#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

/// Number of CPU-exception vectors this slice installs.
pub const EXCEPTION_VECTORS: usize = 32;
/// Number of remapped legacy PIC interrupt vectors.
pub const LEGACY_INTERRUPT_VECTORS: usize = 16;
/// Total live gates through the local-APIC spurious vector.
pub const IDT_VECTORS: usize = 64;

#[cfg(any(target_os = "none", test))]
fn populated_idt(handlers: &[u64; IDT_VECTORS]) -> [IdtEntry64; IDT_VECTORS] {
    let mut idt = [IdtEntry64::ZERO; IDT_VECTORS];
    let mut index = 0;
    while index < IDT_VECTORS {
        idt[index] = IdtEntry64::new(handlers[index], KERNEL_CS, 0, GATE_INTERRUPT_KERNEL);
        index += 1;
    }
    idt
}

#[cfg(target_os = "none")]
mod metal {
    use super::{IdtEntry64, Idtr, IDT_VECTORS};
    use core::sync::atomic::{AtomicU64, Ordering};

    /// Zero-initialized so it lands in `.bss` (NOBITS) — see the module note on the P10-c blocker.
    static mut IDT: [IdtEntry64; IDT_VECTORS] = [IdtEntry64::ZERO; IDT_VECTORS];

    /// Count of exceptions the Tower has handled, for a boot-time liveness assertion.
    static EXCEPTIONS_SEEN: AtomicU64 = AtomicU64::new(0);

    // 32 CPU-exception stubs + a common dispatcher. Each stub normalizes the stack to
    // [.. , error_code, vector] then jumps to `isr_common`; vectors 8/10/11/12/13/14/17/21/29/30
    // are pushed an error code by the CPU, the rest get a dummy 0 so every frame is uniform.
    // att&syntax to match the boot trampoline. — CORVUS
    core::arch::global_asm!(
        ".section .text",
        ".code64",
        // --- stubs (no error code: push dummy 0, then vector) ---
        "isr0:  push $0; push $0;  jmp isr_common",
        "isr1:  push $0; push $1;  jmp isr_common",
        "isr2:  push $0; push $2;  jmp isr_common",
        "isr3:  push $0; push $3;  jmp isr_common",
        "isr4:  push $0; push $4;  jmp isr_common",
        "isr5:  push $0; push $5;  jmp isr_common",
        "isr6:  push $0; push $6;  jmp isr_common",
        "isr7:  push $0; push $7;  jmp isr_common",
        // --- vector 8 #DF: CPU pushes error code ---
        "isr8:  push $8;  jmp isr_common",
        "isr9:  push $0; push $9;  jmp isr_common",
        "isr10: push $10; jmp isr_common",
        "isr11: push $11; jmp isr_common",
        "isr12: push $12; jmp isr_common",
        "isr13: push $13; jmp isr_common",
        "isr14: push $14; jmp isr_common",
        "isr15: push $0; push $15; jmp isr_common",
        "isr16: push $0; push $16; jmp isr_common",
        "isr17: push $17; jmp isr_common",
        "isr18: push $0; push $18; jmp isr_common",
        "isr19: push $0; push $19; jmp isr_common",
        "isr20: push $0; push $20; jmp isr_common",
        "isr21: push $21; jmp isr_common",
        "isr22: push $0; push $22; jmp isr_common",
        "isr23: push $0; push $23; jmp isr_common",
        "isr24: push $0; push $24; jmp isr_common",
        "isr25: push $0; push $25; jmp isr_common",
        "isr26: push $0; push $26; jmp isr_common",
        "isr27: push $0; push $27; jmp isr_common",
        "isr28: push $0; push $28; jmp isr_common",
        "isr29: push $29; jmp isr_common",
        "isr30: push $30; jmp isr_common",
        "isr31: push $0; push $31; jmp isr_common",
        // --- remapped legacy PIC interrupts: no CPU-pushed error code ---
        "isr32: push $0; push $32; jmp isr_common",
        "isr33: push $0; push $33; jmp isr_common",
        "isr34: push $0; push $34; jmp isr_common",
        "isr35: push $0; push $35; jmp isr_common",
        "isr36: push $0; push $36; jmp isr_common",
        "isr37: push $0; push $37; jmp isr_common",
        "isr38: push $0; push $38; jmp isr_common",
        "isr39: push $0; push $39; jmp isr_common",
        "isr40: push $0; push $40; jmp isr_common",
        "isr41: push $0; push $41; jmp isr_common",
        "isr42: push $0; push $42; jmp isr_common",
        "isr43: push $0; push $43; jmp isr_common",
        "isr44: push $0; push $44; jmp isr_common",
        "isr45: push $0; push $45; jmp isr_common",
        "isr46: push $0; push $46; jmp isr_common",
        "isr47: push $0; push $47; jmp isr_common",
        // --- local APIC timer + reserved first-light range + spurious vector ---
        "isr48: push $0; push $48; jmp isr_common",
        "isr49: push $0; push $49; jmp isr_common",
        "isr50: push $0; push $50; jmp isr_common",
        "isr51: push $0; push $51; jmp isr_common",
        "isr52: push $0; push $52; jmp isr_common",
        "isr53: push $0; push $53; jmp isr_common",
        "isr54: push $0; push $54; jmp isr_common",
        "isr55: push $0; push $55; jmp isr_common",
        "isr56: push $0; push $56; jmp isr_common",
        "isr57: push $0; push $57; jmp isr_common",
        "isr58: push $0; push $58; jmp isr_common",
        "isr59: push $0; push $59; jmp isr_common",
        "isr60: push $0; push $60; jmp isr_common",
        "isr61: push $0; push $61; jmp isr_common",
        "isr62: push $0; push $62; jmp isr_common",
        "isr63: push $0; push $63; jmp isr_common",
        // --- common dispatcher: save GPRs, call Rust, restore, drop [vector,errcode], iretq ---
        "isr_common:",
        "  push %rax",
        "  push %rbx",
        "  push %rcx",
        "  push %rdx",
        "  push %rsi",
        "  push %rdi",
        "  push %rbp",
        "  push %r8",
        "  push %r9",
        "  push %r10",
        "  push %r11",
        "  push %r12",
        "  push %r13",
        "  push %r14",
        "  push %r15",
        "  mov %rsp, %rdi",
        // An asynchronous IRQ may land at either SysV stack phase. Preserve the normalized-frame
        // pointer in callee-saved RBX and align RSP before entering Rust. — KESTREL 2026-07-14
        "  mov %rsp, %rbx",
        "  and $-16, %rsp",
        "  cld",
        "  call x86_interrupt_dispatch",
        "  mov %rbx, %rsp",
        "  pop %r15",
        "  pop %r14",
        "  pop %r13",
        "  pop %r12",
        "  pop %r11",
        "  pop %r10",
        "  pop %r9",
        "  pop %r8",
        "  pop %rbp",
        "  pop %rdi",
        "  pop %rsi",
        "  pop %rdx",
        "  pop %rcx",
        "  pop %rbx",
        "  pop %rax",
        "  add $16, %rsp",
        "  iretq",
        // --- stub address table, read by `install` ---
        ".section .rodata",
        ".align 8",
        ".globl isr_table",
        "isr_table:",
        "  .quad isr0,  isr1,  isr2,  isr3,  isr4,  isr5,  isr6,  isr7",
        "  .quad isr8,  isr9,  isr10, isr11, isr12, isr13, isr14, isr15",
        "  .quad isr16, isr17, isr18, isr19, isr20, isr21, isr22, isr23",
        "  .quad isr24, isr25, isr26, isr27, isr28, isr29, isr30, isr31",
        "  .quad isr32, isr33, isr34, isr35, isr36, isr37, isr38, isr39",
        "  .quad isr40, isr41, isr42, isr43, isr44, isr45, isr46, isr47",
        "  .quad isr48, isr49, isr50, isr51, isr52, isr53, isr54, isr55",
        "  .quad isr56, isr57, isr58, isr59, isr60, isr61, isr62, isr63",
        options(att_syntax),
    );

    extern "C" {
        static isr_table: [u64; IDT_VECTORS];
    }

    /// The saved machine state an IDT stub hands to [`x86_interrupt_dispatch`], low address first
    /// (the push order in `isr_common` plus the CPU-pushed interrupt frame).
    #[repr(C)]
    struct ExceptionFrame {
        r15: u64,
        r14: u64,
        r13: u64,
        r12: u64,
        r11: u64,
        r10: u64,
        r9: u64,
        r8: u64,
        rbp: u64,
        rdi: u64,
        rsi: u64,
        rdx: u64,
        rcx: u64,
        rbx: u64,
        rax: u64,
        vector: u64,
        error_code: u64,
        rip: u64,
        cs: u64,
        rflags: u64,
        rsp: u64,
        ss: u64,
    }

    /// #BP (breakpoint) is a trap: `rip` already points past the `int3`, so returning resumes the
    /// interrupted code. Every other CPU exception here is treated as fatal — reported and halted,
    /// exactly as the aarch64 Tower halts, rather than silently triple-faulting.
    const VECTOR_BREAKPOINT: u64 = 3;

    #[no_mangle]
    extern "C" fn x86_interrupt_dispatch(frame: *mut ExceptionFrame) {
        let frame = unsafe { &*frame };
        if crate::local_apic::handle(frame.vector as u8) {
            return;
        }
        if frame.vector >= crate::legacy_irq::INTERRUPT_VECTOR_BASE as u64
            && frame.vector
                < (crate::legacy_irq::INTERRUPT_VECTOR_BASE
                    + crate::legacy_irq::LEGACY_INTERRUPT_VECTORS) as u64
        {
            crate::legacy_irq::handle(frame.vector as u8);
            return;
        }

        EXCEPTIONS_SEEN.fetch_add(1, Ordering::Relaxed);

        crate::serial::write(b"TOWER-x86 EXCEPTION vec=");
        write_hex(frame.vector);
        crate::serial::write(b" err=");
        write_hex(frame.error_code);
        crate::serial::write(b" rip=");
        write_hex(frame.rip);
        crate::serial::write(b"\n");

        if frame.vector != VECTOR_BREAKPOINT {
            crate::serial::write(b"TOWER-x86: fatal exception; HALT\n");
            crate::halt();
        }
        // #BP: resumable — return, and `isr_common` `iretq`s back to the instruction after `int3`.
    }

    /// Fill the zeroed IDT from the stub table and load it. Idempotent.
    pub fn install() {
        unsafe {
            let idt_array = core::ptr::addr_of_mut!(IDT);
            let table = &*core::ptr::addr_of!(isr_table);
            idt_array.write(super::populated_idt(table));
            let idt = idt_array.cast::<IdtEntry64>();
            let idtr = Idtr {
                limit: (core::mem::size_of::<[IdtEntry64; IDT_VECTORS]>() - 1) as u16,
                base: idt as u64,
            };
            core::arch::asm!("lidt [{}]", in(reg) &idtr, options(readonly, nostack, preserves_flags));
        }
    }

    /// How many exceptions the Tower has fielded (boot-time liveness proof).
    pub fn exceptions_seen() -> u64 {
        EXCEPTIONS_SEEN.load(Ordering::Relaxed)
    }

    fn write_hex(mut value: u64) {
        crate::serial::write(b"0x");
        let mut digits = [0u8; 16];
        let mut start = digits.len();
        loop {
            start -= 1;
            let nibble = (value & 0xf) as u8;
            digits[start] = if nibble < 10 {
                b'0' + nibble
            } else {
                b'a' + nibble - 10
            };
            value >>= 4;
            if value == 0 {
                break;
            }
        }
        crate::serial::write(&digits[start..]);
    }
}

#[cfg(target_os = "none")]
pub use metal::{exceptions_seen, install};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_splits_handler_address_across_offset_fields() {
        let handler = 0x1122_3344_5566_7788u64;
        let gate = IdtEntry64::new(handler, KERNEL_CS, 0, GATE_INTERRUPT_KERNEL);
        assert_eq!(gate.handler(), handler);
        assert_eq!(gate.selector, KERNEL_CS);
        assert!(gate.present());
        // Field-level: low/mid/high split at 16/32.
        assert_eq!(gate.offset_low, 0x7788);
        assert_eq!(gate.offset_mid, 0x5566);
        assert_eq!(gate.offset_high, 0x1122_3344);
        assert_eq!(gate.type_attr, 0x8E);
        assert_eq!(gate.reserved, 0);
    }

    #[test]
    fn zero_gate_is_not_present_and_all_zero() {
        let gate = IdtEntry64::ZERO;
        assert!(!gate.present());
        assert_eq!(gate.handler(), 0);
        assert_eq!(gate.ist, 0);
    }

    #[test]
    fn ist_field_is_masked_to_three_bits() {
        let gate = IdtEntry64::new(0xdead_beef, KERNEL_CS, 0xff, GATE_INTERRUPT_KERNEL);
        assert_eq!(gate.ist, 0x7);
    }

    #[test]
    fn descriptor_is_sixteen_bytes() {
        assert_eq!(core::mem::size_of::<IdtEntry64>(), 16);
    }

    #[test]
    fn populated_table_covers_cpu_exceptions_and_interrupt_controllers() {
        let mut handlers = [0u64; IDT_VECTORS];
        for (index, handler) in handlers.iter_mut().enumerate() {
            *handler = 0x1000 + index as u64 * 0x10;
        }

        let idt = populated_idt(&handlers);
        assert_eq!(idt.len(), 64);
        assert_eq!(idt[EXCEPTION_VECTORS - 1].handler(), handlers[31]);
        assert_eq!(idt[EXCEPTION_VECTORS].handler(), handlers[32]);
        assert_eq!(
            idt[crate::local_apic::TIMER_VECTOR as usize].handler(),
            handlers[48]
        );
        assert_eq!(idt[IDT_VECTORS - 1].handler(), handlers[63]);
        assert!(idt.iter().all(|gate| gate.present()));
    }
}
