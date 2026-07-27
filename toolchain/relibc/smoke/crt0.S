// j506
// j507
// I-a2 startup: hand KUMO's x0 bootstrap handle and the address of `main` to relibc's
// KUMO start path. That path builds the SysV view, activates TLS, initialises stdio, runs
// `main`, and exits — so nothing after the branch is reachable. — TIMBERDOODLE

.section .text._start, "ax"
.global _start
.type _start, %function
_start:
    // A freshly entered EL0 thread has no caller; make the frame chain terminate here so a
    // backtrace stops at _start rather than walking into the stack's zeroed head.
    mov x29, xzr
    mov x30, xzr
    adrp x1, main
    add x1, x1, :lo12:main
    bl __relibc_kumo_start
1:
    b 1b
.size _start, . - _start
