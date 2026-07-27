/* j506 */
/* j507 */
/* Static I-a2 image. Dynamic linking and the full relibc crt path begin later. — KESTREL */

ENTRY(_start)

/* FILEHDR/PHDRS put the ELF header and program headers inside the first PT_LOAD, so the
 * running image can read its own `PT_TLS` entry through `__ehdr_start` + AT_PHDR. Without
 * them the headers exist in the file but are never mapped, and relibc's static TLS setup
 * has nothing to find. — TIMBERDOODLE */
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

    .text :
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
    } :data

    /* relibc's startup walks these bounds directly, so the arrays must be named and kept
     * rather than swept by --gc-sections. */
    .init_array :
    {
        PROVIDE_HIDDEN(__preinit_array_start = .);
        KEEP(*(.preinit_array))
        PROVIDE_HIDDEN(__preinit_array_end = .);
        PROVIDE_HIDDEN(__init_array_start = .);
        KEEP(*(SORT_BY_INIT_PRIORITY(.init_array.*)))
        KEEP(*(.init_array))
        PROVIDE_HIDDEN(__init_array_end = .);
        PROVIDE_HIDDEN(__fini_array_start = .);
        KEEP(*(SORT_BY_INIT_PRIORITY(.fini_array.*)))
        KEEP(*(.fini_array))
        PROVIDE_HIDDEN(__fini_array_end = .);
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
