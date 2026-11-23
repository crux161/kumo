/* J936_r02_001 */
/* Pass this script first: lld assigns input sections in script order, before
 * INSERT changes output order. Preserve relibc's TLS/init/fini layout. — KESTREL */
SECTIONS
{
    .eh_frame : ALIGN(0x1000) { KEEP(*(.eh_frame)) } :rodata
    .eh_frame_hdr : ALIGN(8) { KEEP(*(.eh_frame_hdr)) } :rodata
    .gcc_except_table : ALIGN(8) { *(.gcc_except_table .gcc_except_table.*) } :rodata
}
INSERT BEFORE .rodata;
__eh_frame_start = ADDR(.eh_frame);
__eh_frame_end = ADDR(.eh_frame) + SIZEOF(.eh_frame);
__eh_frame_hdr_start = ADDR(.eh_frame_hdr);
__eh_frame_hdr_end = ADDR(.eh_frame_hdr) + SIZEOF(.eh_frame_hdr);
ASSERT(SIZEOF(.eh_frame) > 0, "C++ EH requires frame records; pass this script before link.ld")
ASSERT(SIZEOF(.eh_frame_hdr) > 0, "C++ EH requires --eh-frame-hdr")
