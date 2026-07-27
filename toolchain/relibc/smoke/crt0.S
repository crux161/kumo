// j506
// I-a1 startup: consume KUMO's x0 bootstrap handle, call the C smoke, then
// terminate through relibc's _Exit. Full SysV stack/TLS startup is I-a2. — KESTREL

.section .text._start, "ax"
.global _start
.type _start, %function
_start:
    bl __relibc_kumo_init
    bl main
    bl _Exit
1:
    b 1b
.size _start, . - _start
