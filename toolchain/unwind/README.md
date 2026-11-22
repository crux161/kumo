# Native static unwinding — SH9-a
<!-- J935_r01_001 -->

This is the first native-host-tool prerequisite from PLAN/022 SH9. It builds
LLVM libunwind from pristine `llvmorg-22.1.8` sources, pinned by Git blob and
SHA-256 in `SOURCE.lock`. The Apache-2.0 WITH LLVM-exception license is retained
in `vendor/LICENSE.TXT`. Normal builds are offline.

```sh
scripts/relibc-headers.sh
scripts/rust-std-check.sh --unwind
cargo xtask image --arch aarch64 --hardware qemu-virt-aarch64
python3 scripts/native-unwind-smoke.py --run
```

The opt-in std build has its own staging and target directories. It rebuilds
std and `panic_unwind` with `-Cpanic=unwind`, replaces the empty unwind archive
with the real runtime, and excludes the abort shim's backtrace stubs. The
default target, SDK, and normal image remain panic-abort. An unwind-only linker
fragment augments the canonical Rust layout with bounded `.eh_frame` and
`.eh_frame_hdr` sections; the unwind build requests the indexed header from lld.

Upstream's bare-metal configuration describes static-image table discovery:
the library still runs in a native userspace process over relibc. It uses no
Linux syscalls, dynamic-loader lookup or kernel unwinder. Its FDE cache keeps
upstream's pthread reader/writer locks over relibc; thread support is not
compiled away. Only AArch64 code is selected. No upstream source is patched.

The smoke catches 64 nested panics and checks payloads and exact LIFO cleanup,
resumes a caught unwind through another destructor, runs four native threads
with 16 catches each, joins a panicking thread, then joins a healthy thread.
It also requires at least three nonzero instruction pointers from a real
backtrace. The driver checks final symbol closure and absence of shim unwind
stubs, then requires every record and return to the shell/login on a disposable
full UEFI QEMU boot. A test failure exits nonzero with a failure record.

`--evidence DIR` places the binary, input manifest, ELF hash, and boot logs in
the Limb's verification directory. `--stage DIR --image <board ESP>` prepares
a separate metal image without installing or booting it.

This gate does not establish shared-library unwinding, cross-language C++
exceptions, signal-frame unwinding, SMP, or a native rustc/cargo. C++ runtime/EH
integration and PLAN/014's multicore gates still precede SH9 acceptance.
