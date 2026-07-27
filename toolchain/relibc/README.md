<!-- j505 -->
<!-- j506 -->
<!-- j507 -->
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

I-a2 runs relibc's own startup path on KUMO. `_start` hands the finite bootstrap
channel and `main` to `__relibc_kumo_start`, which drains the capability set,
copies the granted argv VMO, and lays it out as the SysV initial-process image —
`argc`, `argv`, an empty `envp`, and an auxiliary vector whose
`AT_PHDR`/`AT_PHENT`/`AT_PHNUM` come from the running image's own mapped ELF
header. Upstream `relibc_start_v1` then owns TLS activation, stdio, the init
arrays, `main`, and exit. Live paths are startup, fd 1/2 channel writes, process
exit, anonymous allocation over kernel-placed VMOs, and the futex pair; every
other required `Pal`, `PalEpoll`, `PalPtrace`, `PalSignal`, and `PalSocket`
method remains an explicit `ENOSYS`, inert value, or diverging trap.

`envp` is empty on purpose. KUMO grants no environment capability, and
synthesising one here would be ambient authority wearing a POSIX name.

The check produces a static AArch64 smoke image at
`target/relibc-smoke/relibc-smoke`. `cargo xtask image` packages it as
`bin/relibc-smoke`, `etc/autoexec` runs it at every boot, and the shell exposes
it with arguments:

```text
[MUREX> relibc i-a2
relibc: C runtime smoke
relibc: argc=2
relibc: argv[0]=relibc
relibc: argv[1]=i-a2
relibc: tls=17 errno=9 tls@0x10011000 image@0x21c000
relibc: stderr channel -> OK
relibc: startup+printf+argv+tls+calloc 384KiB -> OK
```

The `tls@`/`image@` pair is the point of that line: the thread-local lives in the
mapped TLS block reached through `tpidr_el0`, not in the program image, and
`errno=9` is `EBADF` arriving through relibc's own `#[thread_local]` storage.

That program uses a header-free C entry point deliberately. Installed headers and
`fopen`/`fread` over Houtu belong to I-a3.

The overlay temporarily selects relibc's Linux AArch64 C data layouts and
numeric header constants. It does not select the Linux platform module or issue
Linux syscalls. Any constant becomes KUMO ABI only when a later slice exercises
and accepts the corresponding interface.
