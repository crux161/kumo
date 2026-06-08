ENTRY(_start)

SECTIONS
{
  . = 0x40080000;

  .text : ALIGN(4096) {
    KEEP(*(.text.boot))
    *(.text .text.*)
    *(.rodata .rodata.*)
  }

  .data : ALIGN(4096) {
    *(.data .data.*)
  }

  .bss : ALIGN(4096) {
    *(.bss .bss.*)
    *(COMMON)
  }
}
