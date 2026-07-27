<!-- j511 -->
# KUMO's vendored header generator

relibc writes most of its C headers in Rust and generates the `.h` files with
[cbindgen]. KUMO needs those headers to build C programs, and the build must work
from a clean checkout with no network and nothing installed globally — the same
promise `scripts/relibc-check.sh` already makes for the library itself. So the
tool's entire dependency closure lives here.

```sh
./scripts/relibc-headers.sh     # builds the tool, then the sysroot
```

That produces `target/kumo-sysroot/` — `include/` plus `lib/libc.a` — and links
the C smoke against it.

## What is in here

- `Cargo.toml` / `Cargo.lock` / `src/lib.rs` — a **vendoring driver**, not a build
  target. It exists so `cargo vendor` can resolve cbindgen's closure; the tool is
  then built with `cargo install`, which needs no manifest of ours.
- `vendor/` — 51 crates. `SOURCE.lock` records the pinned version and the lock
  hash, and `relibc-headers.sh` refuses to run if the lock has moved.
- `LICENSES.md` — the inventory. Read the top section: cbindgen itself is MPL-2.0.

It is deliberately **outside the workspace**. Every workspace member builds for a
KUMO target; this resolves for the host. Keeping it separate also stops its
dependency versions colliding with `toolchain/relibc/vendor`, which is relibc's
own target-side closure and is unioned differently.

## Why the headers are generated, not committed

They could have been generated once and checked in, which would have avoided this
closure entirely. Vendoring the generator was chosen instead so that a header set
can never drift from the relibc revision that produced it: bump the pin in
`SOURCE.lock` and the headers are simply regenerated, with no separate artifact to
remember to refresh.

## Compiling against the sysroot

Pass `-D__kumo__`. Constants that differ between platforms are emitted guarded, as
`#if defined(__linux__) || defined(__kumo__)` against `#if defined(__redox__)`;
without the define, a header offers both values and selects neither.

```sh
clang --target=aarch64-unknown-none -D__kumo__ -ffreestanding \
      --sysroot=target/kumo-sysroot -isystem target/kumo-sysroot/include \
      -c program.c -o program.o
```

[cbindgen]: https://github.com/mozilla/cbindgen
