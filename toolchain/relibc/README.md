<!-- j505 -->
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

I-a0 is compile-only. The KUMO `Sys` type implements every required `Pal`,
`PalEpoll`, `PalPtrace`, `PalSignal`, and `PalSocket` method, but each method is
an explicit `ENOSYS`, inert value, or diverging trap. Runtime implementations
begin in I-a1.

The overlay temporarily selects relibc's Linux AArch64 C data layouts and
numeric header constants. It does not select the Linux platform module or issue
Linux syscalls. Any constant becomes KUMO ABI only when a later slice exercises
and accepts the corresponding interface.
