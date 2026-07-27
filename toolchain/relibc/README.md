<!-- j505 -->
<!-- j506 -->
<!-- j507 -->
<!-- j508 -->
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
arrays, `main`, and exit. I-a3 adds the per-process FD table and file I/O: a
granted `kumo-vfs` channel backs `openat`/`read`/`close`, so C `fopen`/`fread`
name real files. Live paths are startup, the FD table, channel writes, file
reads, process exit, anonymous allocation over kernel-placed VMOs, and the futex
pair; every other required `Pal`, `PalEpoll`, `PalPtrace`, `PalSignal`, and
`PalSocket` method remains an explicit `ENOSYS`, inert value, or diverging trap.

Two absences are deliberate rather than stubbed. `envp` is empty: KUMO grants no
environment capability, and synthesising one would be ambient authority wearing a
POSIX name. fd 0 is closed: no input capability is granted yet, and a descriptor
that reads nothing is a better answer than one that reads somewhere else.

The check produces a static AArch64 smoke image at
`target/relibc-smoke/relibc-smoke`. `cargo xtask image` packages it as
`bin/relibc-smoke`, `etc/autoexec` runs it at every boot, and the shell exposes
it with arguments:

```text
[MUREX> relibc i-a3
relibc: C runtime smoke
fatfs starting
fatfs: serving
relibc: argc=2
relibc: argv[0]=relibc
relibc: argv[1]=i-a3
relibc: tls=17 errno=9 tls@0x10011000 image@0x220000
relibc: stderr channel -> OK
relibc: fopen+fread /HELLO.TXT (6 bytes) -> hello!
relibc: absent path refused errno=2
relibc: startup+printf+argv+tls+calloc+file -> OK
```

The `tls@`/`image@` pair is the point of that line: the thread-local lives in the
mapped TLS block reached through `tpidr_el0`, not in the program image, and
`errno=9` is `EBADF` arriving through relibc's own `#[thread_local]` storage.

The `fatfs` lines are a private filesystem server, spawned for this launch alone:
its whole authority is a read-only alias of the backing VMO, and it exits when
its one client's channel closes.

That program is still header-free deliberately. Gate I-a is closed, but a C
program that `#include`s `<stdio.h>` needs the installed header set and a
`--sysroot`, which belong to the SDK (gate I-d), not here.

The overlay temporarily selects relibc's Linux AArch64 C data layouts and
numeric header constants. It does not select the Linux platform module or issue
Linux syscalls. Any constant becomes KUMO ABI only when a later slice exercises
and accepts the corresponding interface.
