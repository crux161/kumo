# The measured KUMO C++ runtime — G7l
<!-- j737 -->
<!-- J936_r02_001 -->
<!-- J939_r05_001 -->
<!-- J940_r06_001 -->
<!-- J941_r07_001 -->

Build the G7k tool cache and driver graph first, keeping the offline container:

```sh
scripts/relibc-headers.sh
python3 scripts/mesa-drivers.py --prepare --keep
python3 scripts/cxx-smoke.py
# After building the standard full UEFI QEMU image:
python3 scripts/cxx-smoke.py --run
# Create a separate board-specific ESP for metal acceptance:
python3 scripts/cxx-smoke.py --image build/images/orange-pi-5-plus --stage build/proofs/g7l-opi
```

The result is `build/mesa-drivers/result/cxx/libkumo-cxx.a`, with a static
`cxx-smoke`, its link map, empty final undefined-symbol report and input hashes.
No target Linux libc++/libc++abi binary is used. Headers come from the pinned
LLVM 22.1.8 package and the source subset comes from `llvmorg-22.1.8`, with both
Git blob IDs and SHA256 checked against `SOURCE.lock` before every build.
The upstream Apache-2.0 WITH LLVM-exception license is retained in
`vendor/libcxx/LICENSE.TXT`; source files remain pristine.

The subset contains libc++'s sort specializations, hash prime search and mutex
operations/destructor, and libc++abi's virtual-call failure and local-static
guard implementation. The library sources use C++23 as upstream requires for
the sort implementation; Mesa and the smoke consume the same ABI in C++17.
All are compiled without exceptions or RTTI. KUMO supplies allocation/deletion
over real relibc malloc/free and fatal no-exception hooks. Allocation failure
aborts; it never returns a successful null allocation. relibc supplies the
destructor registry, and a static-image fini hook drains it at process exit.
This is one static image, with no dynamic-loader facility.

The only upstream adaptation is made to a private copy of `cxa_guard_impl.h`:
KUMO does not include `sys/syscall.h`. That header's Linux compatibility numbers
do not represent native syscall authority. Upstream's real pthread mutex and
condition-variable guard implementation remains selected; no dummy lock or
threadless guard is substituted. This slice starts no new thread.

The separate-process acceptance checks allocation during static initialization,
virtual dispatch and virtual deletion, vector growth, sort, nested guarded
statics initialized exactly once, mutex lock/trylock/unlock/destruction, hash
growth, nonzero initialized TLS, and exit destructors. The success marker is
emitted by the static destructor after main succeeds. `--run` modifies only a
disposable copy of the QEMU ESP and replaces its relibc smoke slot and autoexec.

The initialized TLS check exposed relibc's static master-copy bug: its original
end-relative offset misplaced `.tdata` on KUMO AArch64. The KUMO overlay now
copies at the variant-I segment start. The unchanged zero-filled TLS smoke had
not exercised that case. Gallium's initialized TLS dispatch pointer needs the
same correction. The other relibc platform paths retain their previous offset.

G7l passes both full UEFI QEMU and Orange Pi 5 Plus metal acceptance (J737).
The same smoke returned through its static destructor and reached login over PXE. Runtime and build decisions — SAKER, 2026-10-31.

## SH9-b1: native exception ABI and RTTI

The opt-in EH build extends the same LLVM 22.1.8 source pin. It builds libc++abi's
Itanium exception runtime and RTTI, libc++'s exception/message support, and SH9-a's
static libunwind against the native relibc sysroot. G7l's no-exception archive and
the default SDK keep their existing build paths.

```sh
scripts/relibc-headers.sh
python3 toolchain/cxx/build-eh.py
cargo xtask image --arch aarch64 --hardware qemu-virt-aarch64
python3 scripts/cxx-eh-smoke.py --run
# Stage a disposable ESP for later board acceptance:
python3 scripts/cxx-eh-smoke.py --image build/images/orange-pi-5-plus --stage build/proofs/sh9-b1-opi
```

The offline build uses host clang/LLVM tools targeting bare AArch64, with native
headers and libraries only. `SOURCE.lock` records the exact upstream revision,
Git blob IDs and SHA256 for the measured source/header dependency closure; builds
verify and stage only those files. Both upstream license files are retained.
The generated config selects pthreads and Itanium ABI, and disables unsupported
library facilities. The same staged guard admission as G7l avoids Linux futex
numbers; relibc supplies the real pthread implementation. The manifest records
compiler version and all compilation inputs, and the final ELF must have no
undefined symbols. No target Linux C++ binary is used.

Outputs live in `target/cxx-eh`: `libkumo-cxx-eh.a`, `cxx-eh-smoke`, `link.map`,
`undefined.txt` and `manifest.json`. The linker fragment must precede the
canonical relibc script so its frame records are consumed first. Assertions
reject empty unwind tables. relibc still owns TLS, startup and exit registration.
Link either the EH archive or G7l's archive into one static image, never both.

The three full-UEFI boots check adjusted base-class catches and dynamic casts,
exact nested cleanup order, rethrow and exception messages, throwing local-static
initialization/retry, `bad_alloc`, overlapping catches and `exception_ptr` across
four native pthreads, initialized TLS isolation, and process-exit destructors.
Separate `noexcept` and uncaught-exception cases must call the installed terminate
handler and return control to the shell. The positive verdict appears only from
the exit destructor. Every boot must reach login without a fault in its proof
window; the existing two pre-proof QEMU board-UART faults remain the sole allowance.
Logs, executable and input hashes are saved under `build/verification/sh9-b1`.

This is the exception ABI slice, not the full libc++ runtime or a native compiler.
Streams, filesystem, locale, dynamic loading, dynamic TLS destructors, SMP,
native rustc/cargo bootstrap and Orange Pi execution remain separate gates.


## SH9-c1: libc++ filesystem path lexical operations

The opt-in path slice adds the pinned LLVM 22.1.8 `filesystem/path.cpp` and `string.cpp` support, plus their
compiler-discovered header closure to `SOURCE.lock`. It enables filesystem declarations
only in a generated test config, links a static `libkumo-cxx-fs-path.a`, and proves
root/decomposition, extension handling, lexical normalization, append, replacement, and
path iteration under QEMU and on Orange Pi 5 Plus. The path proof does not call filesystem
syscalls; status, directory iteration, streams, locale, dynamic loading, and native compiler
acceptance remain separate. Build and run with `python3 toolchain/cxx/build-fs-path.py`
and `python3 scripts/cxx-fs-path-smoke.py --run`.


## SH9-c2: libc++ read-only filesystem status

The opt-in SH9-c2 builder extends the pinned LLVM 22.1.8 source closure with
`filesystem/operations.cpp` and its exact include dependencies. It proves libc++
`status`, `exists`, and file-kind mapping against KUMO's existing relibc `stat`
path: VFS root directory status, the synthetic `/dev/null` character-device
entry, and `ENOENT`/`not_found` semantics for a missing VFS path. The runner queries the existing VFS root and synthetic device node directly
through relibc `fstatat`; it needs no mounted-volume grant. It makes no
filesystem changes and adds no VFS/kernel calls.

The upstream operations translation unit also contains unrelated copy support.
Because this no-locale KUMO target has no `sendfile`, the builder generates a
private header declaration only so that translation unit can compile; the
status smoke links with section garbage collection and asserts no `sendfile`
reference remains in the final ELF. The status archive is scoped to these
read-only queries, not the remaining filesystem operations.

```sh
python3 toolchain/cxx/build-fs-status.py
python3 scripts/cxx-fs-status-smoke.py --run
python3 scripts/metal-sh9-c2.py --proof build/verification/r06/sh9-c2/cxx-fs-status-smoke
```


## SH9-c3: libc++ read-only directory iteration

The opt-in SH9-c3 builder adds the pinned LLVM 22.1.8 `filesystem/directory_iterator.cpp` and
`filesystem/directory_entry.cpp`, plus the `<stack>` and `<deque>` headers the recursive iterator
needs, and links them over the SH9-c2 status archive. libc++ iterates through relibc's existing
`opendir`/`readdir`/`closedir` (entry kinds come from `d_type`) and refreshes an entry through
`stat`. No VFS or kernel call is added.

The `relibc` word grants the process a fatfs session on the RAM FAT image, which it sees as `/`.
The proof uses the fixtures the image builder always lays down: `/EFI` holds exactly `BOOT`,
which holds exactly the six-byte `BOOTAA64.EFI`; `/HELLO.TXT` is six bytes. It checks one-level
iteration with entry kinds and `file_size()`, that the root listing from libc++ is exactly the
one relibc's own `readdir` gives (each entry once, `.` and `..` skipped), recursive depth and
`disable_recursion_pending`, and `ENOENT`/`ENOTDIR` through `error_code` and as a thrown
`filesystem_error`. The builder fails if the ELF links any mutating call (`mkdir`, `unlink`,
`rename`, `utimensat`, ...). File contents, mutations, `space`, permissions and symlinks are
still separate gates.

```sh
python3 toolchain/cxx/build-fs-dir.py
python3 scripts/cxx-fs-dir-smoke.py --run
python3 scripts/metal-sh9-c3.py --proof build/verification/r07/sh9-c3/cxx-fs-dir-smoke
```
