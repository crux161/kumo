//! Long-mode Global Descriptor Table and Task State Segment.
//!
//! The Multiboot trampoline needs only enough segmentation to enter long mode, so it carries a
//! temporary null/kernel-code/kernel-data GDT. Before installing the IDT, the HAL replaces that
//! table with the descriptor layout needed by ring 3 and loads a real 64-bit TSS. The TSS owns
//! `rsp0`, which is the stack the CPU will select on a future CPL3 -> CPL0 transition. — KESTREL

/// Selectors shared by interrupt gates and the future syscall/`iretq` path.
pub const KERNEL_CODE_SELECTOR: u16 = 0x08;
pub const KERNEL_DATA_SELECTOR: u16 = 0x10;
pub const USER_DATA_SELECTOR: u16 = 0x18;
pub const USER_CODE_SELECTOR: u16 = 0x20;
pub const TSS_SELECTOR: u16 = 0x28;

pub const USER_DATA_SELECTOR_RPL3: u16 = USER_DATA_SELECTOR | 3;
pub const USER_CODE_SELECTOR_RPL3: u16 = USER_CODE_SELECTOR | 3;

const GDT_ENTRIES: usize = 7;
const GDT_LIMIT: u16 = (core::mem::size_of::<[u64; GDT_ENTRIES]>() - 1) as u16;

const ACCESS_KERNEL_CODE: u8 = 0x9a;
const ACCESS_KERNEL_DATA: u8 = 0x92;
const ACCESS_USER_CODE: u8 = 0xfa;
const ACCESS_USER_DATA: u8 = 0xf2;
const ACCESS_TSS_AVAILABLE: u8 = 0x89;

// Descriptor flags, excluding the high limit nibble: G=1, D/B=0, L=1 for 64-bit code;
// G=1, D/B=1, L=0 for data.
const FLAGS_LONG_CODE: u8 = 0x0a;
const FLAGS_DATA: u8 = 0x0c;

const fn segment_descriptor(access: u8, flags: u8) -> u64 {
    let limit = 0x000f_ffff_u64;
    (limit & 0xffff)
        | ((access as u64) << 40)
        | (((limit >> 16) & 0x0f) << 48)
        | (((flags as u64) & 0x0f) << 52)
}

const KERNEL_CODE_DESCRIPTOR: u64 = segment_descriptor(ACCESS_KERNEL_CODE, FLAGS_LONG_CODE);
const KERNEL_DATA_DESCRIPTOR: u64 = segment_descriptor(ACCESS_KERNEL_DATA, FLAGS_DATA);
const USER_DATA_DESCRIPTOR: u64 = segment_descriptor(ACCESS_USER_DATA, FLAGS_DATA);
const USER_CODE_DESCRIPTOR: u64 = segment_descriptor(ACCESS_USER_CODE, FLAGS_LONG_CODE);

/// AMD64 TSS layout (Intel SDM Vol. 3A, figure 8-11).
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct TaskStateSegment {
    reserved0: u32,
    rsp: [u64; 3],
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    io_map_base: u16,
}

impl TaskStateSegment {
    const fn new(rsp0: u64) -> Self {
        Self {
            reserved0: 0,
            rsp: [rsp0, 0, 0],
            reserved1: 0,
            ist: [0; 7],
            reserved2: 0,
            reserved3: 0,
            // A base at/after the TSS limit means there is no I/O-permission bitmap.
            io_map_base: core::mem::size_of::<Self>() as u16,
        }
    }
}

const fn tss_descriptor(base: u64) -> [u64; 2] {
    let limit = (core::mem::size_of::<TaskStateSegment>() - 1) as u64;
    let low = (limit & 0xffff)
        | ((base & 0xffff) << 16)
        | (((base >> 16) & 0xff) << 32)
        | ((ACCESS_TSS_AVAILABLE as u64) << 40)
        | (((limit >> 16) & 0x0f) << 48)
        | (((base >> 24) & 0xff) << 56);
    [low, base >> 32]
}

#[cfg(target_os = "none")]
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

/// Hardware readback proving that the permanent GDT and TSS are active.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DescriptorTableReport {
    pub gdt_limit: u16,
    pub kernel_code_selector: u16,
    pub kernel_data_selector: u16,
    pub user_code_selector: u16,
    pub user_data_selector: u16,
    pub task_selector: u16,
    pub rsp0: u64,
}

impl DescriptorTableReport {
    pub const fn is_live(self) -> bool {
        self.gdt_limit == GDT_LIMIT
            && self.kernel_code_selector == KERNEL_CODE_SELECTOR
            && self.kernel_data_selector == KERNEL_DATA_SELECTOR
            && self.user_code_selector == USER_CODE_SELECTOR_RPL3
            && self.user_data_selector == USER_DATA_SELECTOR_RPL3
            && self.task_selector == TSS_SELECTOR
            && self.rsp0 != 0
    }
}

#[cfg(target_os = "none")]
mod metal {
    use super::*;

    // Both structures are intentionally zero-initialized so the Multiboot a.out image keeps them
    // in NOBITS. `install` fills all address-bearing descriptors after the loader has zeroed BSS.
    static mut GDT: [u64; GDT_ENTRIES] = [0; GDT_ENTRIES];
    static mut TSS: TaskStateSegment = TaskStateSegment::new(0);

    pub fn install(rsp0: u64) -> DescriptorTableReport {
        unsafe {
            let tss = core::ptr::addr_of_mut!(TSS);
            tss.write(TaskStateSegment::new(rsp0));
            let tss_words = tss_descriptor(tss as u64);

            let gdt = core::ptr::addr_of_mut!(GDT);
            gdt.write([
                0,
                KERNEL_CODE_DESCRIPTOR,
                KERNEL_DATA_DESCRIPTOR,
                USER_DATA_DESCRIPTOR,
                USER_CODE_DESCRIPTOR,
                tss_words[0],
                tss_words[1],
            ]);

            let gdtr = DescriptorTablePointer {
                limit: GDT_LIMIT,
                base: gdt.cast::<u64>() as u64,
            };
            core::arch::asm!("lgdt [{}]", in(reg) &gdtr, options(readonly, nostack));

            // Reload CS with a far return, then make every data selector refer to the permanent
            // table and activate the available 64-bit TSS.
            core::arch::asm!(
                "push {kernel_code}",
                "lea rax, [rip + 2f]",
                "push rax",
                "retfq",
                "2:",
                "mov ax, {kernel_data}",
                "mov ds, ax",
                "mov es, ax",
                "mov fs, ax",
                "mov gs, ax",
                "mov ss, ax",
                "mov ax, {tss}",
                "ltr ax",
                kernel_code = const KERNEL_CODE_SELECTOR,
                kernel_data = const KERNEL_DATA_SELECTOR,
                tss = const TSS_SELECTOR,
                out("rax") _,
            );
        }

        readback(rsp0)
    }

    /// Select the kernel stack used on the next CPL3 -> CPL0 transition.
    pub fn set_kernel_stack(rsp0: u64) {
        unsafe {
            let tss = core::ptr::addr_of_mut!(TSS);
            // `TaskStateSegment` is packed; form a raw pointer without creating an
            // unaligned reference and update only RSP0 while TR remains loaded.
            core::ptr::addr_of_mut!((*tss).rsp)
                .cast::<u64>()
                .write_unaligned(rsp0);
        }
    }

    fn readback(rsp0: u64) -> DescriptorTableReport {
        let mut gdtr = DescriptorTablePointer { limit: 0, base: 0 };
        let cs: u64;
        let ss: u64;
        let tr: u64;
        unsafe {
            core::arch::asm!("sgdt [{}]", in(reg) &mut gdtr, options(nostack, preserves_flags));
            core::arch::asm!("xor eax, eax", "mov ax, cs", out("rax") cs, options(nomem, nostack));
            core::arch::asm!("xor eax, eax", "mov ax, ss", out("rax") ss, options(nomem, nostack));
            core::arch::asm!("xor eax, eax", "str ax", out("rax") tr, options(nomem, nostack));
        }
        let gdt_limit = gdtr.limit;
        DescriptorTableReport {
            gdt_limit,
            kernel_code_selector: cs as u16,
            kernel_data_selector: ss as u16,
            user_code_selector: USER_CODE_SELECTOR_RPL3,
            user_data_selector: USER_DATA_SELECTOR_RPL3,
            task_selector: tr as u16,
            rsp0,
        }
    }
}

/// Replace the bootstrap GDT, reload all segment registers, and load TR with the permanent TSS.
pub fn install(rsp0: u64) -> DescriptorTableReport {
    #[cfg(target_os = "none")]
    {
        metal::install(rsp0)
    }
    #[cfg(not(target_os = "none"))]
    {
        DescriptorTableReport {
            gdt_limit: GDT_LIMIT,
            kernel_code_selector: KERNEL_CODE_SELECTOR,
            kernel_data_selector: KERNEL_DATA_SELECTOR,
            user_code_selector: USER_CODE_SELECTOR_RPL3,
            user_data_selector: USER_DATA_SELECTOR_RPL3,
            task_selector: TSS_SELECTOR,
            rsp0,
        }
    }
}

/// Update TSS.RSP0 for a user transition's kernel-entry stack.
#[cfg(target_os = "none")]
pub fn set_kernel_stack(rsp0: u64) {
    metal::set_kernel_stack(rsp0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_layout_supports_iretq_and_sysret_ordering() {
        assert_eq!(KERNEL_CODE_SELECTOR, 0x08);
        assert_eq!(KERNEL_DATA_SELECTOR, 0x10);
        assert_eq!(USER_DATA_SELECTOR_RPL3, 0x1b);
        assert_eq!(USER_CODE_SELECTOR_RPL3, 0x23);
        assert_eq!(TSS_SELECTOR, 0x28);
        assert_eq!(USER_CODE_SELECTOR, USER_DATA_SELECTOR + 8);
    }

    #[test]
    fn code_and_data_descriptors_have_the_expected_privilege() {
        assert_eq!(KERNEL_CODE_DESCRIPTOR, 0x00af_9a00_0000_ffff);
        assert_eq!(KERNEL_DATA_DESCRIPTOR, 0x00cf_9200_0000_ffff);
        assert_eq!(USER_DATA_DESCRIPTOR, 0x00cf_f200_0000_ffff);
        assert_eq!(USER_CODE_DESCRIPTOR, 0x00af_fa00_0000_ffff);
    }

    #[test]
    fn tss_layout_and_descriptor_encode_all_base_bits() {
        assert_eq!(core::mem::size_of::<TaskStateSegment>(), 104);
        let tss = TaskStateSegment::new(0x1234_5678_9abc_def0);
        let io_map_base = tss.io_map_base;
        assert_eq!(io_map_base, 104);

        let base = 0x1122_3344_5566_7788;
        let descriptor = tss_descriptor(base);
        let decoded_base = ((descriptor[0] >> 16) & 0xffff)
            | (((descriptor[0] >> 32) & 0xff) << 16)
            | (((descriptor[0] >> 56) & 0xff) << 24)
            | (descriptor[1] << 32);
        assert_eq!(decoded_base, base);
        assert_eq!((descriptor[0] >> 40) & 0xff, ACCESS_TSS_AVAILABLE as u64);
        assert_eq!(descriptor[0] & 0xffff, 103);
        assert_eq!(descriptor[1] >> 32, 0);
    }

    #[test]
    fn report_rejects_an_unloaded_task_register() {
        let mut report = install(1);
        assert!(report.is_live());
        report.task_selector = 0;
        assert!(!report.is_live());
    }
}
