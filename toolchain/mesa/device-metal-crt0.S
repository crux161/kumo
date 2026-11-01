// j747
// G7o-f startup for a Sora-launched GPU client. Sora passes the drv-gpu session channel in
// x0 and the lane pair's child end in x1, not relibc's startup channel, and relibc's start
// path would read and close whatever x0 names. Keep both handles for the proof, then let
// kumo_metal_start hand relibc an empty startup channel. — TIMBERDOODLE

.section .text._start, "ax"
.global _start
.type _start, %function
_start:
    mov x29, xzr
    mov x30, xzr
    adrp x9, kumo_metal_session
    str x0, [x9, :lo12:kumo_metal_session]
    adrp x9, kumo_metal_lane
    str x1, [x9, :lo12:kumo_metal_lane]
    adrp x0, main
    add x0, x0, :lo12:main
    bl kumo_metal_start
1:
    b 1b
.size _start, . - _start
