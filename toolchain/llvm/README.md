<!-- J948_r14_001 -->
# LLVM for native KUMO — SH9-d1: Support and Demangle

The first native bootstrap slice of PLAN/022 SH9. LLVM's `Support` and `Demangle` libraries
(llvmorg-22.1.8, the same revision as `toolchain/cxx`) are built for `aarch64-unknown-kumo`
against the installed libc++ layer (`toolchain/cxx/build-libcxx.py`) and relibc, and run natively
on KUMO.

## Sources

`SOURCE.lock` pins the measured closure: 467 files, about 7 MB. It holds every Support and
Demangle unit; the headers they and the proof include; the Unix layer (`lib/Support/Unix/*.inc`);
BLAKE3's portable and NEON sources (not its x86 SIMD); `third-party/siphash`; the CMake config
templates; and `llvm/LICENSE.TXT`, which also covers SipHash. Each file has its git blob ID and
SHA-256, and `vendor/` holds the exact upstream bytes. The libc++ layer gained four headers LLVM
includes (`<execution>`, `<inttypes.h>`, `<list>`, `<queue>`) in `toolchain/cxx/SOURCE.lock`.

## The KUMO port (`kumo.patch`)

Applied to a staged copy only, so `vendor/` stays upstream. Every hunk is `__kumo__`-guarded:

- `Unix/Path.inc` `getMainExecutable`: KUMO has no `/proc/self/exe` and no `dladdr`, so `argv[0]`
  is resolved through `getprogpath`, as on OpenBSD and Haiku.
- `Unix/Path.inc` `is_local_impl`: every volume a KUMO process is granted is served by a local
  filesystem server, so the answer is `true`.
- `Unix/Path.inc` `dontNeedImpl`/`willNeedImpl`: VMO mappings take no paging hints, so these are
  no-ops, as on AIX and z/OS.
- `Unix/Program.inc` `Wait`: relibc has `waitpid` but not `wait4` or `rusage`, so it waits with
  `waitpid` and reports no `ProcessStatistics`. Spawning still fails cleanly at run time, because
  KUMO has no exec.

## Configuration (`gen-config.py`)

There is no native CMake run yet, so `gen-config.py` renders the pinned `llvm/Config` templates
with the answers that run would give on relibc. Threads, pthread rwlocks, `sigaltstack`, `sysconf`,
`mmap`, `futimens`, `getrusage`, `strerror_r` and `setenv` are on. `dlopen`, crash-handler
backtraces, `posix_spawn`, zlib, zstd, ICU and iconv are off for this slice. The triple is
`aarch64-unknown-kumo`.

## Build and proof

`build-support.py` runs `build-libcxx.py`, verifies every pin, stages the closure, applies the
patch, and renders the config. It then compiles 175 Support units (166 C++, the BSD regex C files
and four BLAKE3 files) and 6 Demangle units into `install/lib/libLLVMSupport.a` and
`libLLVMDemangle.a`, and links `support-smoke.cpp` against them, the libc++ layer and relibc,
refusing any unresolved symbol. The ELF asks for a 4 MiB main stack through `PT_GNU_STACK` (see
toolchain/cxx/README.md, SH9-c6).

The proof covers:

- `SmallVector`, `DenseMap`, `StringMap`, `StringRef`, `Twine`, a 2^100 `APInt`, `APFloat` and
  `formatv`.
- `cl::ParseCommandLineOptions` and `Expected`/`Error` text.
- `sys::path`, and `sys::fs` (`create_directories`, `raw_fd_ostream`, `file_size`,
  `directory_iterator`, `remove_directories`) with `MemoryBuffer::getFile` on the RAM FAT volume.
- SHA-256 (`"abc"`), BLAKE3 (empty input) and xxHash64 (empty input) against their published
  vectors, and `Regex` captures.
- A `DefaultThreadPool` summing 1..100.
- The Itanium demangler on a libc++ name.
- `ConvertUTF8toWide`/`convertWideToUTF8` through `std::wstring`.

```sh
scripts/relibc-headers.sh
python3 toolchain/llvm/build-support.py
python3 scripts/llvm-support-smoke.py --run
python3 scripts/metal-sh9-d1.py --proof build/verification/r14/sh9-d1/llvm-support-smoke
```

This is Support, not a compiler: IR, CodeGen, the AArch64 target, and a native `clang`/`lld`/`rustc`
are later slices. PLAN/014 (SMP) still gates SH9 acceptance.
