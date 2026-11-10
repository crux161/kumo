/* j853 — static Rust std image. Same shape as the relibc smoke script: the ELF header
 * and program headers live inside the first PT_LOAD so std's TLS setup can read PT_TLS
 * through its own image. — TIMBERDOODLE */
ENTRY(_start)
PHDRS
{
    text   PT_LOAD FILEHDR PHDRS FLAGS(5);
    rodata PT_LOAD FLAGS(4);
    data   PT_LOAD FLAGS(6);
    tls    PT_TLS  FLAGS(4);
}
SECTIONS
{
    . = 0x00200000 + SIZEOF_HEADERS;
    .text : { *(.text._start) *(.text .text.*) } :text
    .rodata : ALIGN(0x1000) { *(.rodata .rodata.*) } :text
    .eh_frame : ALIGN(8) { KEEP(*(.eh_frame)) } :text
    .tdata : ALIGN(0x1000) { *(.tdata .tdata.*) } :tls :data
    .tbss : ALIGN(0x1000) { *(.tbss .tbss.*) } :tls :data
    __tdata_start = ADDR(.tdata);
    __tdata_end = ADDR(.tdata) + SIZEOF(.tdata);
    __tbss_end = ADDR(.tbss) + SIZEOF(.tbss);
    .data : ALIGN(0x1000) { *(.data .data.*) } :data
    .bss : ALIGN(0x1000) { *(.bss .bss.*) *(COMMON) } :data
    /DISCARD/ : { *(.comment) *(.note.GNU-stack) }
}
