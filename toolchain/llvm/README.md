<!-- J948_r14_001 -->
<!-- J949_r15_001 -->
<!-- J950_r16_001 -->
<!-- J951_r17_001 -->
# LLVM for native KUMO — bootstrap libraries

The first native bootstrap slice of PLAN/022 SH9. LLVM's `Support` and `Demangle` libraries
(llvmorg-22.1.8, the same revision as `toolchain/cxx`) are built for `aarch64-unknown-kumo`
against the installed libc++ layer (`toolchain/cxx/build-libcxx.py`) and relibc, and run natively
on KUMO.

SH9-d2 adds all 14 upstream BinaryFormat units as `libLLVMBinaryFormat.a`, with a native proof of
its format, DWARF and metadata helpers. It uses the same revision, libc++ layer, relibc and port.

SH9-d3 builds all 15 TargetParser units as `libLLVMTargetParser.a`, including four tables generated
by the matching LLVM 22.1.8 host `llvm-tblgen`. It closes BinaryFormat's TargetParser dependency.

SH9-d4 builds all 36 Object units as `libLLVMObject.a` and proves native ELF and regular GNU
archive readers. Its general binary/IR factories and writers still need later link components.

## Sources

`SOURCE.lock` pins the measured closure: 1,014 files, 24,685,189 bytes. SH9-d1's 467 files hold every Support and
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

SH9-d3 adds 274 upstream files (13,796,728 bytes): 15 TargetParser units, their measured include
closure and Unix host implementation, the two component/table CMake lists, and 238 TableGen
inputs. The latter include instruction and intrinsic definitions needed to parse the four
upstream target descriptions; this does not build IR, CodeGen or an instruction backend.

SH9-d4 adds 205 upstream files (3,333,189 bytes): every Object unit and its CMake recipe, the
measured headers they require (including MC and IR declarations), and the enum-table recipes
and definitions those headers include. The libc++ layer gains the pinned `<cinttypes>` header.

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
- `TargetParser/Triple.h` and `Triple.cpp`: a KUMO-only `Kumo` OS enum, type-name and exact
  `kumo` spelling recognition. It is appended after existing OS values, which retain their numbers.
  Native tools can parse and normalize their own triple and compute its ELF data layout.

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

BinaryFormat's upstream link components are Support and TargetParser. The SH9-d2 builder/proof
uses helpers that link with Support/Demangle alone and records TargetParser as unbuilt in that
configuration. Use SH9-d3 below to build the complete dependency set and exercise Mach-O CPU
selection and XCOFF CPU-name normalization.

## TargetParser (SH9-d3)

`build-targetparser.py` builds Support, Demangle, BinaryFormat and TargetParser. Its proof links
both BinaryFormat and TargetParser with `--whole-archive`, without section garbage collection,
so every function in those archives must resolve. No link components remain unbuilt. It retains
the same 4 MiB main stack. The native proof checks:

- The KUMO OS enum, triple parsing/normalization, ELF data layout, process/default triples, and
  upstream's conservative host fallback (`generic` CPU and no detected features).
- Generated AArch64 architecture/CPU/extension tables and ARM parsing, including invalid names.
- Generated RISC-V CPU tables, ISA extensions, ABI selection and invalid-ISA errors.
- Generated PPC features, CPU aliases and X86/LoongArch/CSKY/Xtensa name tables.
- Subtarget feature lists and bitsets spanning multiple words, and AMDGPU architecture names.
- BinaryFormat's formerly unresolved dependencies: ARM parsing, triple architecture names and
  bitness through Mach-O CPU/subtype selection, PPC normalization through XCOFF, plus refusal
  of a non-Mach-O triple with an `Expected` error.

`gen-target-tables.py` requires `llvm-tblgen` **22.1.8**. An incompatible generator fails before
emitting headers. Select it with `--tblgen` or `LLVM_TBLGEN`; default discovery checks
`llvm-tblgen-22`, Homebrew's `llvm@22`, then `llvm-tblgen` on PATH. It generates ARM/AArch64
architecture tables, RISC-V definitions and PPC target features from the pinned descriptions.
Every included `.td` and each primary input must be pinned; LLVM 22's depfile omits the primary
input, so the generator records it explicitly. The manifest records the tool version/hash,
measured inputs and output hashes. The proof runner archives the generated tables too.

```sh
scripts/relibc-headers.sh
python3 scripts/llvm-targetparser-smoke.py --run
# Optional: --tblgen /path/to/llvm-22.1.8/bin/llvm-tblgen
python3 scripts/metal-sh9-d3.py
```

Object's remaining facilities, MC, IR and native `llvm-mc`/`lld` remain later slices. LLVM's process-launch integration
for driver-style tools and PLAN/014's SMP acceptance gates remain open.

## Object ELF/archive readers (SH9-d4)

`build-object.py` builds Support, Demangle, BinaryFormat, TargetParser and all 36 upstream Object
units, checked against Object's pinned CMake source list. No new KUMO patch is needed. It links
`object-smoke.cpp` against the resulting archives, libc++ and relibc with a 4 MiB main stack and
no undefined symbols. Section garbage collection is enabled for this cut.

Object's complete upstream link set also includes **BitReader, Core, MC, IRReader, MCParser and
TextAPI**, which this cut leaves unbuilt and records in its manifest. The proof uses
`ObjectFile::createELFObjectFile` and `Archive::create` directly. The general `createBinary` and
symbolic-file routes, bitcode, other formats, archive/object writers and disassembly await the
remaining components. A compiled archive is not a claim that all its entry points can link yet.

The native proof checks:

- ELF64LE AArch64 identity, section kinds and exact text/rodata bytes, BSS size and alignment.
- Defined/undefined and global symbols, function type, value, size and section association.
- AArch64 ABS64 relocation type/name, symbol, offset and signed addend.
- ELF32BE ARM identity and big-endian data reads. Upstream's `getArch()` reports ARM independently
  of byte order; the typed ELF reader supplies the endian distinction.
- Truncated ELF headers, section-table bounds and invalid symbol string offsets as errors.
- GNU archive member iteration, long names, indexed symbol lookup, absent symbols and truncation
  errors, including parsing both member objects directly.
- Exact file bytes and ELF/archive parsing through `MemoryBuffer::getFile` on the granted RAM
  volume; the temporary files are removed.

`make-object-fixtures.py` assembles `object-fixture.S` on the **host** for AArch64 and big-endian
ARM, creates a deterministic GNU archive and an object with a bad symbol-name offset, and embeds
the input bytes in a generated header. This tests native reading, not native assembly or linking.
The runner archives the original object/archive inputs and their producer versions/hashes.

In Object mode, `gen-target-tables.py` also emits the IR attributes, intrinsic enum, runtime
libcall and CodeGen value-type headers needed to compile Object's sources, using matching
22.1.8 TableGen and the same pinned-input checks. These are definitions, without building the
Core/IR or CodeGen libraries. The manifest records the extra table hashes separately; the proof
runner archives the complete generated include tree. Repeated tables and fixtures are byte-identical.

```sh
scripts/relibc-headers.sh
python3 scripts/llvm-object-smoke.py --run
python3 scripts/metal-sh9-d4.py
```

Next: MC and its prerequisites, then the IR components needed to close Object's remaining
entry points. Native `llvm-mc`/`lld`, LLVM process launching and SMP acceptance remain open.
