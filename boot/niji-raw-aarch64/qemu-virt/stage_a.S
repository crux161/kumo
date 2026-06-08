// Minimal AArch64 Stage-A first-light image for QEMU virt.
// Loaded by QEMU with: -M virt -cpu cortex-a72 -display none -serial stdio
// -monitor none -kernel kumo-qemu-virt.elf
// This is not the final UEFI Nijigumo path; it is an arm64 bootable artifact.

.equ PL011_UART0_BASE, 0x09000000
.equ PL011_UARTDR,     0x00
.equ PL011_UARTFR,     0x18
.equ PL011_UARTIBRD,   0x24
.equ PL011_UARTFBRD,   0x28
.equ PL011_UARTLCR_H,  0x2c
.equ PL011_UARTCR,     0x30
.equ PL011_UARTFR_RXFE, 4
.equ PL011_UARTFR_TXFF, 5
.equ PL011_UARTLCR_H_FEN, (1 << 4)
.equ PL011_UARTLCR_H_WLEN_8, (3 << 5)

.section .text.boot, "ax"
.global _start
.type _start, %function

_start:
    bl uart_init
    ldr x0, =boot_transcript
    bl puts
    ldr x0, =ready_prompt
    bl puts
    mov x19, #0
    b repl

repl:
    bl getc

    // Backspace and Delete erase one displayed ASCII byte when possible.
    cmp w1, #0x08
    b.eq erase
    cmp w1, #0x7f
    b.eq erase

    // Return starts a new empty line. No parser yet.
    cmp w1, #'\r'
    b.eq newline
    cmp w1, #'\n'
    b.eq newline

    // Ignore control bytes; QEMU stdio may inject a few as it binds.
    cmp w1, #0x20
    b.lt repl
    cmp w1, #0x7f
    b.ge repl

    bl putc
    add x19, x19, #1
    b repl

erase:
    cbz x19, repl
    mov w1, #0x08
    bl putc
    mov w1, #' '
    bl putc
    mov w1, #0x08
    bl putc
    sub x19, x19, #1
    b repl

newline:
    mov w1, #'\r'
    bl putc
    mov w1, #'\n'
    bl putc
    mov x19, #0
    b repl

uart_init:
    ldr x2, =PL011_UART0_BASE

    // Disable before programming, then enable UART + TX + RX.
    str wzr, [x2, #PL011_UARTCR]
    mov w3, #1
    str w3, [x2, #PL011_UARTIBRD]
    str wzr, [x2, #PL011_UARTFBRD]
    mov w3, #(PL011_UARTLCR_H_FEN | PL011_UARTLCR_H_WLEN_8)
    str w3, [x2, #PL011_UARTLCR_H]
    mov w3, #((1 << 0) | (1 << 8) | (1 << 9))
    str w3, [x2, #PL011_UARTCR]
    ret

puts:
    mov x4, x30

puts_next:
    ldrb w1, [x0], #1
    cbz w1, puts_done
    bl putc
    b puts_next

puts_done:
    ret x4

putc:
    ldr x2, =PL011_UART0_BASE

wait_tx_ready:
    ldr w3, [x2, #PL011_UARTFR]
    tbnz w3, #PL011_UARTFR_TXFF, wait_tx_ready
    str w1, [x2, #PL011_UARTDR]
    ret

getc:
    ldr x2, =PL011_UART0_BASE

wait_rx_ready:
    ldr w3, [x2, #PL011_UARTFR]
    tbnz w3, #PL011_UARTFR_RXFE, wait_rx_ready
    ldr w1, [x2, #PL011_UARTDR]
    and w1, w1, #0xff
    ret

.section .rodata, "a"
boot_transcript:
    .asciz "[NIJIGUMO] HANDOFF COMPLETE abi=v1 arch=aarch64\nCPU MODE: Executive (EL1)\nAETHER: pending; boot map=QEMU-virt handoff unavailable in raw path\n"
ready_prompt:
    .asciz "READY\n"
