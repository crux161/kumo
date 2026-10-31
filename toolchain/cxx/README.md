# The measured KUMO C++ runtime — G7l
<!-- j736 -->

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

G7l's QEMU acceptance passes. Metal acceptance is still required before the
gate is closed. Runtime and build decisions — SAKER, 2026-10-31.
