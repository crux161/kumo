/* Link script for the freestanding aarch64 KUMO kernel image (Nijigumo target).
 *
 * The kernel is non-PIE (static relocation model): core::fmt and friends emit
 * absolute R_AARCH64_ABS64 relocations, which the linker resolves for this fixed
 * base. Nijigumo therefore must load each PT_LOAD segment at its p_paddr and jump
 * to e_entry. The base sits well inside RAM (QEMU virt RAM starts at 0x40000000)
 * to avoid the low region UEFI/AAVMF uses for its own tables and stack.
 *
 * Higher-half mapping and per-section permissions are a later slice; for now the
 * kernel runs identity-mapped in the firmware's page tables after ExitBootServices.
 */

ENTRY(_start)

SECTIONS {
  . = 0x48000000;

  .text : ALIGN(0x1000) {
    KEEP(*(.text._start))
    *(.text .text.*)
  }

  .rodata : ALIGN(0x1000) {
    *(.rodata .rodata.*)
  }

  .data : ALIGN(0x1000) {
    *(.data .data.*)
    *(.got .got.*)
  }

  .bss (NOLOAD) : ALIGN(0x1000) {
    *(.bss .bss.*)
    *(COMMON)
  }

  /DISCARD/ : {
    *(.comment)
    *(.note .note.*)
    *(.eh_frame .eh_frame_hdr)
  }
}
