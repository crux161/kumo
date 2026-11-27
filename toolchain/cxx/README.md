# The measured KUMO C++ runtime — G7l
<!-- J941_r07_001 -->
<!-- J942_r08_001 -->
<!-- J944_r10_002 -->
<!-- J945_r11_001 -->
<!-- J946_r12_001 -->

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


## SH9-c4: libc++ locale layer and read-only file streams

libc++ provides `<fstream>` only with its locale layer, so SH9-c4 builds a second, opt-in
configuration: `_LIBCPP_HAS_FILESYSTEM 1`, `_LIBCPP_HAS_LOCALIZATION 1` and
`_LIBCPP_PROVIDES_DEFAULT_RUNE_TABLE` (relibc is not a libc libc++ has a ctype table for).
`build-fstream.py` adds the pinned `locale.cpp`, `ios.cpp`, `ios.instantiations.cpp`,
`fstream.cpp`, `ostream.cpp`, `call_once.cpp` and `new_helpers.cpp` with their header closure,
and links `libkumo-cxx-locale.a` over the SH9-b1 EH archives. The default configuration and
every earlier archive are unchanged.

For an unrecognised libc, libc++'s locale layer calls `strtof_l`, `strtod_l`, `strtold_l` and
`strftime_l`. relibc lacked them; `toolchain/relibc/kumo.patch` now adds them, ignoring the
locale like relibc's existing `_l` calls (relibc has only the C locale). Rebuild the sysroot
with `scripts/relibc-headers.sh` first. The builder refuses a sysroot without them, and
refuses an ELF that does not reach all four or that links a mutating call.

The proof reads `/HELLO.TXT` line-wise and through `istreambuf_iterator`; reads
`/EFI/BOOT/BOOTAA64.EFI` in binary mode with `seekg`/`tellg`, a mid-file read, a full read
and a read past the end; checks a missing file's failed open and `ios_base::failure`; and
drives the classic locale's number extraction (`double`, `int`, `float`, `long double`),
number insertion and `put_time`. Writing streams, wide characters and named locales remain
separate gates.

```sh
scripts/relibc-headers.sh
python3 toolchain/cxx/build-fstream.py
python3 scripts/cxx-fstream-smoke.py --run
python3 scripts/metal-sh9-c4.py --proof build/verification/r08/sh9-c4/cxx-fstream-smoke
```


## SH9-c5: libc++ writes

`build-fs-write.py` reuses SH9-c4's localized build (`build-fstream.py`'s `build()`) and adds
`filesystem/operations.cpp`, `directory_iterator.cpp`, `directory_entry.cpp`, `path.cpp` and
`filesystem_error.cpp`. With localization on and no `<sys/sendfile.h>`, `copy_file` takes
libc++'s portable fstream path, so SH9-c2's `sendfile` declaration is not used. The builder
requires the ELF to reach relibc's `mkdir`, `rename`, `remove`, `truncate`, `openat`,
`fdopendir` and `unlinkat` (the last three are `remove_all`'s descriptor walk).

The proof works on the RAM FAT image the `relibc` launch is granted, so nothing it writes
outlives the boot, and builds and removes `/SH9C5` with 8.3 names. It covers
`create_directory`/`create_directories` (existing and missing parents); `ofstream` write,
append, truncate and `resize_file`; `copy_file`, its `file_exists` refusal and
`overwrite_existing`; `equivalent` across two spellings of one FAT name; `rename` and the
listing after it; and `remove`, the non-empty-directory refusal and `remove_all` (four entries).

`copy_file` and `equivalent` need real `st_dev`/`st_ino`. Before J944_r10_001 relibc reported
zero for both, so every file was "the same file". The VFS stat and open replies now carry the
server's file id (FAT short-entry slot, RedoxFS node), which relibc reports as `st_ino`.

```sh
scripts/relibc-headers.sh
python3 toolchain/cxx/build-fs-write.py
python3 scripts/cxx-fs-write-smoke.py --run
python3 scripts/metal-sh9-c5.py --proof build/verification/r10/sh9-c5/cxx-fs-write-smoke
```


## SH9-c6: the complete narrow-character libc++

`build-libcxx.py` compiles every upstream `LIBCXX_SOURCES` unit for threads, localization and
filesystem (50 units) into one `libc++.a`, built as upstream builds it against libc++abi
(`-DLIBCXX_BUILDING_LIBCXXABI`). The units SH9-b1's ABI archive already carries (`exception`,
`stdexcept`, `verbose_abort`, and libc++abi's `operator new`/`delete` in place of `new.cpp`)
stay there. `charconv.cpp` uses LLVM-libc's shared float and integer parsers, so the closure
now pins 117 files under `libc/` plus `libc/LICENSE.TXT`, built with the libc root on the
include path and `LIBC_NAMESPACE=__llvm_libc_common_utils` as `FindLibcCommonUtils.cmake` does.

Two facilities are deliberately absent. `std::random_device` (`random.cpp`): relibc's KUMO
`getrandom` is a clock-seeded xorshift, not an entropy source, so the unit stays off
(`_LIBCPP_HAS_RANDOM_DEVICE 0`) until a real one exists; the `<random>` engines and
distributions are header-only and are proven. Wide characters (`_LIBCPP_HAS_WIDE_CHARACTERS 0`):
enabling them needs `wcscoll_l`, `wcsxfrm_l` and more from relibc, which is SH9-c7. LLVM needs
`std::wstring` (`ConvertUTFWrapper`). (SH9-c7 turned them on; see below.)

The build installs a sysroot layer, `install/include/c++/v1` (the measured header closure plus
the generated `__config_site`) and `install/lib/{libc++,libc++abi,libunwind}.a`, and compiles
the proof against that layer alone. Programs linking it need a real main stack: Sora gives
thread zero what `PT_GNU_STACK` asks for (J741), else 16 KiB, and the first boot overflowed
16 KiB inside a `bad_alloc` throw. The relibc link script declares `PHDRS`, so lld creates no
stack header for `-z stack-size` to fill; the builder adds one and writes 1 MiB into it, as
Mesa's build does.

The proof covers `std::cout` and stream formatting; four threads with a mutex, condition
variable and `std::barrier`, `std::async`, and `std::shared_mutex`; the standard's fixed
`mt19937`/`mt19937_64` 10000th outputs and a distribution; ECMAScript `regex_match`,
`regex_search` and `regex_replace`; `to_chars`/`from_chars` and `std::format`; and `pmr`
containers over a `monotonic_buffer_resource` whose `null_memory_resource` upstream throws.

```sh
scripts/relibc-headers.sh
python3 toolchain/cxx/build-libcxx.py
python3 scripts/cxx-libcxx-smoke.py --run
python3 scripts/metal-sh9-c6.py --proof build/verification/r11/sh9-c6/cxx-libcxx-smoke
```


## SH9-c7: wide characters

`build-libcxx.py` now builds the complete library with `_LIBCPP_HAS_WIDE_CHARACTERS 1`. With
wide characters on, every unit included libc++'s BSD locale fallback, which needs relibc's wide
`_l` calls, and `string.cpp` needs `wcstof`/`wcstold`. `toolchain/relibc/kumo.patch` adds
`wcstof`, `wcstold` (C, beside `strtold`), `wcscoll_l`, `wcsxfrm_l`, `iswctype_l`, `towupper_l`,
`towlower_l` and the twelve `isw*_l` classifiers. All of them ignore the locale, as relibc has
only the C locale. The patch also fixes three upstream relibc problems that libc++'s wide facets
would reach:

- `mbstate_t` was a zero-sized type, an empty struct in C, so C++'s `mbstate_t mb = {0};` did
  not compile. It now has musl's layout, two opaque words. relibc's UTF-8 conversions still keep
  no shift state.
- `wcsxfrm` returned 0 and transformed nothing. In the C locale it is now the identity: it copies
  when the result fits and returns the length.
- `wcstod` parsed digits and one `.` by hand, so `std::stod(L"3.25e2")` read 3.25. It now narrows
  the ASCII prefix and calls relibc's `strtod`, so exponents, hex, inf and nan parse, and the end
  pointer maps back. `wcstof` takes the same path.

The classic locale's `wchar_t` facets answer from libc++'s own tables. The proof also builds a
locale from `ctype_byname<wchar_t>("C")` and `collate_byname<wchar_t>("C")`, which do call relibc.
It covers `wstring` and the `sto*` and `to_wstring` conversions (including the `stod` end
position); `wostringstream`, `wistringstream` and `std::wcout`; `ctype`, `collate` (compare and
transform) and a `codecvt<wchar_t, char>` round trip; and `wregex` and wide `std::format`.
SH9-c6's narrow proof runs against the wide build too.

```sh
scripts/relibc-headers.sh
python3 scripts/cxx-libcxx-wide-smoke.py --run
python3 scripts/metal-sh9-c7.py --proof build/verification/r12/sh9-c7/cxx-libcxx-wide-smoke
```
