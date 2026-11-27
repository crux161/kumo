<!-- J948_r14_001 -->
<!-- J949_r15_001 -->
# LLVM for native KUMO — Support, Demangle and BinaryFormat

The first native bootstrap slice of PLAN/022 SH9. LLVM's `Support` and `Demangle` libraries
(llvmorg-22.1.8, the same revision as `toolchain/cxx`) are built for `aarch64-unknown-kumo`
against the installed libc++ layer (`toolchain/cxx/build-libcxx.py`) and relibc, and run natively
on KUMO.

SH9-d2 adds all 14 upstream BinaryFormat units as `libLLVMBinaryFormat.a`, with a native proof of
its format, DWARF and metadata helpers. It uses the same revision, libc++ layer, relibc and port.

## Sources

`SOURCE.lock` pins the measured closure: 535 files, about 8 MB. SH9-d1's 467 files hold every Support and
Demangle unit; the headers they and the proof include; the Unix layer (`lib/Support/Unix/*.inc`);
BLAKE3's portable and NEON sources (not its x86 SIMD); `third-party/siphash`; the CMake config
templates; and `llvm/LICENSE.TXT`, which also covers SipHash. Each file has its git blob ID and
SHA-256, and `vendor/` holds the exact upstream bytes. The libc++ layer gained four headers LLVM
includes (`<execution>`, `<inttypes.h>`, `<list>`, `<queue>`) in `toolchain/cxx/SOURCE.lock`.

SH9-d2 adds 68 exact upstream files (732,970 bytes): BinaryFormat's CMake source list and all its
units, their measured header/definition closure, and the TargetParser headers they include.
The builder checks BinaryFormat's units against that pinned CMake list. Both builds record the
LLVM inputs their depfiles measured and reject unpinned LLVM files or host system headers;
compiler intrinsic headers are allowed only from the C/C++ drivers' resource directories and
their hashes are recorded separately.

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

## BinaryFormat (SH9-d2)

`build-binaryformat.py` uses the same builder to create Support, Demangle and BinaryFormat
archives, then links `binaryformat-smoke.cpp` with no unresolved symbols and a 4 MiB main stack.
No additional KUMO patch is needed. Its proof checks:

- ELF type identification in both byte orders, short/unknown input, archive/Wasm/bitcode/Mach-O
  magic, architecture-name and enum mappings.
- A byte-exact ELF header fixture written, identified, read and removed on the granted RAM FAT
  volume; the file overload's missing-path error.
- DWARF names and reverse lookup, AArch64 CFI, version-dependent form sizes and version gates.
- COFF section-name encoding boundaries, Wasm relocation addends, and XCOFF/DXContainer/SFrame
  tables.
- MessagePack fixed wire bytes, document and YAML round trips, truncated input and EOF.
- Typed AMDGPU metadata acceptance and invalid-type rejection.

```sh
scripts/relibc-headers.sh
python3 scripts/llvm-binaryformat-smoke.py --run
python3 scripts/metal-sh9-d2.py
```

BinaryFormat's upstream link components are Support and TargetParser. This slice vendors the
TargetParser headers, without building that library. The proof exercises helpers that link with
Support/Demangle alone; entry points that call TargetParser (such as Mach-O CPU selection and
XCOFF CPU-name normalization) await a native TargetParser. The manifest records that unbuilt
dependency. Object parsing, MC, IR and native `llvm-mc`/`lld` remain later slices; TargetParser
must be closed as their prerequisite. Driver-style tools still need a process-launch path.
