# niji-raw-aarch64 QEMU Virt First-Light

This directory contains a minimal raw AArch64 first-light path for QEMU `virt`.
It is intentionally not the final UEFI Nijigumo implementation. It exists to
produce arm64 boot files now, while the Rust `aarch64-unknown-uefi` and
`build-std` path is still blocked by local toolchain availability.

The generated files live under:

```text
build/aarch64/qemu-virt/
```

Boot command, once `qemu-system-aarch64` is available:

```sh
build/aarch64/qemu-virt/run-qemu.sh
```

Bounded smoke test:

```sh
cargo xtask qemu-smoke
```

Expected serial output:

```text
[NIJIGUMO] HANDOFF COMPLETE abi=v1 arch=aarch64
CPU MODE: Executive (EL1)
AETHER: pending; boot map=QEMU-virt handoff unavailable in raw path
READY
```

After `READY`, printable ASCII serial input is echoed, Return starts a new
line, and Backspace/Delete erases one displayed byte. There is no parser or
command language yet.

CJK input is intentionally out of scope for this raw Stage-A path. The real
TTY/Kumoza line discipline should handle IME text later, using Simple Kana to
Kanji in the SKK family (`cskk`, Catbus, or equivalent) rather than teaching the
panic/debug console about Unicode composition.
