/* Link script for the freestanding x86_64 KUMO kernel, booted by GRUB / any
 * Multiboot loader (and by `qemu-system-x86_64 -kernel`, which implements
 * Multiboot1 directly).
 *
 * Multiboot1 requires the 12-byte header within the first 8 KiB of the file and
 * 4-byte aligned, so `.boot` (header + the 32-bit entry stub) is placed first, at the
 * classic 1 MiB load address (below 1 MiB is BIOS/legacy). The loader enters 32-bit
 * protected mode at `_start`; the stub builds long mode and calls into Rust.
 *
 * `.bss` is left as NOBITS (not NOLOAD): the loader zero-fills [filesz, memsz), so
 * the boot page tables and Rust statics start zeroed without bloating the image.
 */

ENTRY(_start)

SECTIONS {
  . = 0x100000;

  .boot : {
    KEEP(*(.multiboot_header))
    KEEP(*(.text._start))
  }

  .text : ALIGN(0x1000) {
    *(.text .text.*)
  }

  .rodata : ALIGN(0x1000) {
    *(.rodata .rodata.*)
  }

  .data : ALIGN(0x1000) {
    *(.data .data.*)
    *(.got .got.*)
  }

  /* End of file-backed data: the Multiboot a.out kludge loads [load_addr, __load_end)
   * from the flat image and zero-fills [__load_end, __bss_end) for .bss. */
  __load_end = .;

  .bss : ALIGN(0x1000) {
    *(.bss .bss.*)
    *(COMMON)
  }

  __bss_end = .;

  /DISCARD/ : {
    *(.comment)
    *(.note .note.*)
    *(.eh_frame .eh_frame_hdr)
  }
}
