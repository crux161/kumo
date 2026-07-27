/* j506 */
/* Static I-a1 image. Dynamic linking and the full relibc crt path begin later. — KESTREL */

ENTRY(_start)

PHDRS
{
    text   PT_LOAD FLAGS(5);
    rodata PT_LOAD FLAGS(4);
    data   PT_LOAD FLAGS(6);
    tls    PT_TLS  FLAGS(4);
}

SECTIONS
{
    . = 0x00200000;

    .text : ALIGN(0x1000)
    {
        *(.text._start)
        *(.text .text.*)
    } :text

    .rodata : ALIGN(0x1000)
    {
        *(.rodata .rodata.*)
        *(.eh_frame_hdr)
        *(.eh_frame)
    } :rodata

    .data : ALIGN(0x1000)
    {
        *(.data .data.*)
        *(.got .got.*)
        *(.init_array .init_array.*)
        *(.fini_array .fini_array.*)
    } :data

    .tdata : ALIGN(0x10)
    {
        *(.tdata .tdata.*)
    } :data :tls

    .tbss : ALIGN(0x10)
    {
        *(.tbss .tbss.*)
    } :data :tls

    .bss : ALIGN(0x1000)
    {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ :
    {
        *(.comment)
        *(.note .note.*)
    }
}
