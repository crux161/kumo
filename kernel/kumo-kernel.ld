/* Link script for the freestanding aarch64 KUMO kernel image (Nijigumo target).
 *
 * The kernel is linked in the TTBR1 higher half while retaining a compact physical
 * load layout beginning at 0x48000000. Nijigumo may place that physical image at any
 * free page-aligned address, maps it at the linked virtual addresses, and jumps to
 * the high e_entry. Absolute references therefore remain virtual and are never
 * rewritten to the board-specific physical placement.
 */

ENTRY(_start)

SECTIONS {
  KERNEL_VIRT_BASE = 0xffff800048000000;
  KERNEL_PHYS_BASE = 0x48000000;
  . = KERNEL_VIRT_BASE;

  .text ALIGN(0x1000) : AT(KERNEL_PHYS_BASE + (ADDR(.text) - KERNEL_VIRT_BASE)) {
    KEEP(*(.text._start))
    *(.text .text.*)
  }

  .rodata ALIGN(0x1000) : AT(KERNEL_PHYS_BASE + (ADDR(.rodata) - KERNEL_VIRT_BASE)) {
    *(.rodata .rodata.*)
  }

  .data ALIGN(0x1000) : AT(KERNEL_PHYS_BASE + (ADDR(.data) - KERNEL_VIRT_BASE)) {
    *(.data .data.*)
    *(.got .got.*)
  }

  .bss ALIGN(0x1000) (NOLOAD) : AT(KERNEL_PHYS_BASE + (ADDR(.bss) - KERNEL_VIRT_BASE)) {
    *(.bss .bss.*)
    *(COMMON)
  }

  /DISCARD/ : {
    *(.comment)
    *(.note .note.*)
    *(.eh_frame .eh_frame_hdr)
  }
}
