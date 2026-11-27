//J951_r17_001
// Host-assembled input for the native Object reader; this is not a native assembler proof. — KESTREL
.text
.p2align 2
.globl sh9_answer
.type sh9_answer, %function
sh9_answer:
#if defined(__aarch64__)
    mov x0, #42
    ret
#else
    mov r0, #42
    bx lr
#endif
.size sh9_answer, .-sh9_answer

.data
.p2align 3
.globl sh9_data
.type sh9_data, %object
sh9_data:
#if defined(__aarch64__)
    .quad sh9_missing + 9
#else
    .word sh9_missing + 9
#endif
.size sh9_data, .-sh9_data

.section .rodata
.globl sh9_rodata
.type sh9_rodata, %object
sh9_rodata:
    .asciz "kumo-object"
.size sh9_rodata, .-sh9_rodata

.bss
.p2align 4
.globl sh9_bss
.type sh9_bss, %object
sh9_bss:
    .zero 32
.size sh9_bss, .-sh9_bss
