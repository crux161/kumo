/* J935_r01_001 — augment the canonical static Rust layout only for native unwinding. */
SECTIONS
{
    .eh_frame_hdr : ALIGN(8) { KEEP(*(.eh_frame_hdr)) } :text
    .gcc_except_table : ALIGN(8) { *(.gcc_except_table .gcc_except_table.*) } :text
}
INSERT AFTER .eh_frame;
__eh_frame_start = ADDR(.eh_frame);
__eh_frame_end = ADDR(.eh_frame) + SIZEOF(.eh_frame);
__eh_frame_hdr_start = SIZEOF(.eh_frame_hdr) > 0 ? ADDR(.eh_frame_hdr) : 0;
__eh_frame_hdr_end = SIZEOF(.eh_frame_hdr) > 0 ? ADDR(.eh_frame_hdr) + SIZEOF(.eh_frame_hdr) : 0;
