ENTRY(_start)

PHDRS
{
    text   PT_LOAD FLAGS(5); /* R-X */
    rodata PT_LOAD FLAGS(4); /* R-- */
    data   PT_LOAD FLAGS(6); /* RW- */
}

SECTIONS
{
    . = 0x0000008000000000;

    .text : ALIGN(0x1000)
    {
        *(.text._start)
        *(.text .text.*)
    } :text

    .rodata : ALIGN(0x1000)
    {
        *(.rodata .rodata.*)
    } :rodata

    .data : ALIGN(0x1000)
    {
        *(.data .data.*)
        *(.got .got.*)
    } :data

    .bss : ALIGN(0x1000)
    {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ :
    {
        *(.comment)
        *(.note .note.*)
        *(.eh_frame .eh_frame_hdr)
    }
}
