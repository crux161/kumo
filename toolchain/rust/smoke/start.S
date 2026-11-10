// j853
// The Rust std entry: x0 carries KUMO's bootstrap capability handle. The Rust shim
// installs the relibc fd table from the same startup stream, builds argc/argv, and calls
// the compiler-generated `main` (the lang_start shim), which owns the Rust runtime.
// — TIMBERDOODLE

.section .text._start, "ax"
.global _start
.type _start, %function
_start:
    mov x29, xzr
    mov x30, xzr
    bl kumo_std_start       // x0 = argc, x1 = argv (the relibc fd table is installed)
    bl main                 // the compiler-generated lang_start shim
1:
    b 1b
.size _start, . - _start
