<!-- j505 -->
<!-- j506 -->
# KUMO relibc overlay

KUMO keeps the audited upstream tree pristine at `vendor/relibc` and applies
`kumo.patch` plus `overlay/` to an ephemeral source tree. This makes the
upstream pin reviewable and prevents local platform work from being confused
with Redox-owned source.

Run:

```sh
git submodule update --init --recursive vendor/relibc
./scripts/relibc-check.sh
```

I-a1 makes only four runtime paths live: finite bootstrap-capability intake,
fd 1/2 channel writes, process exit, and zeroed anonymous allocation backed by
kernel-placed VMOs. The allocator intentionally retains freed spans until
KUMO's reserved unmap operation is dispatched. Every other required `Pal`,
`PalEpoll`, `PalPtrace`, `PalSignal`, and `PalSocket` method remains an explicit
`ENOSYS`, inert value, or diverging trap.

The check produces a static AArch64 smoke image at
`target/relibc-smoke/relibc-smoke`. `cargo xtask image` packages it as
`bin/relibc-smoke`; on a full KUMO userspace boot, run:

```text
[MUREX> relibc
relibc: C runtime smoke
relibc: stderr channel -> OK
relibc: write+exit+malloc 384KiB -> OK
```

That program uses a header-free C entry point deliberately. Full SysV
argc/argv/env/TLS startup and installed-header acceptance belong to I-a2 and
I-a3 respectively.

The overlay temporarily selects relibc's Linux AArch64 C data layouts and
numeric header constants. It does not select the Linux platform module or issue
Linux syscalls. Any constant becomes KUMO ABI only when a later slice exercises
and accepts the corresponding interface.
