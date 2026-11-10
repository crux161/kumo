#!/bin/sh
#j858
# SH1 (PLAN/022): assemble kumo-sdk/ (the Athanor) from the built artifacts. The SDK is
# self-contained and relocatable: every wrapper computes its root from its own path, the
# runtime crates ship as sources with generated manifests, and the registry closure is
# vendored for offline builds.
#
#     scripts/kumo-sdk.sh
#
# Output: build/kumo-sdk (KUMO_SDK_OUT overrides).
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${KUMO_SDK_OUT:-$ROOT/build/kumo-sdk}"
TOOLCHAIN="nightly-2026-06-22"
SYSROOT_SRC="${KUMO_SYSROOT:-$ROOT/build/sysroot/aarch64-unknown-kumo}"
RELIBC_SYSROOT="$ROOT/target/kumo-sysroot"
IMAGE="$ROOT/build/images/qemu-virt-aarch64"
VENDOR="$ROOT/vendor/cargo"

fail() { printf 'kumo-sdk: %s\n' "$1" >&2; exit 1; }

# Inputs, growing them through the existing drivers when absent.
[ -f "$SYSROOT_SRC/manifest.json" ] || sh "$ROOT/scripts/rust-sysroot.sh" \
    || fail "rust-sysroot failed"
[ -f "$RELIBC_SYSROOT/lib/libc.a" ] || sh "$ROOT/scripts/relibc-headers.sh" \
    || fail "relibc-headers failed"
# The runner is the program's only window: the payloads are always built at
# KUMO_CONSOLE_LOGLEVEL=6 so the transcript carries the autoexec and program records.
# A quiet image hid them once (J858) and cost a whole diagnostic cycle.
( cd "$ROOT" && KUMO_CONSOLE_LOGLEVEL=6 cargo xtask image --arch aarch64 --hardware qemu-virt-aarch64 ) \
    || fail "qemu-virt image build failed"

rm -rf "$OUT"
mkdir -p "$OUT/targets" "$OUT/bin" "$OUT/sysroot" "$OUT/relibc/lib" "$OUT/relibc/include" \
    "$OUT/boot/EFI/BOOT" "$OUT/boot/EFI/KUMO/kernel" "$OUT/crates" "$OUT/vendor" \
    "$OUT/examples/hello-rust/src" "$OUT/examples/hello-c" "$OUT/cookbook"

# The prebuilt Rust sysroot (its manifest and cargo-config fragment ride along).
cp -R "$SYSROOT_SRC/." "$OUT/sysroot/"
cp "$ROOT/targets/aarch64-unknown-kumo.json" "$OUT/targets/"
# The C side of the SDK: headers, libc.a, crt0 and the link script.
cp -R "$RELIBC_SYSROOT/include/." "$OUT/relibc/include/"
cp "$RELIBC_SYSROOT/lib/libc.a" "$OUT/relibc/lib/"
cp "$ROOT/toolchain/relibc/smoke/crt0.S" "$ROOT/toolchain/relibc/smoke/link.ld" "$OUT/relibc/"
# Consumers' entry glue (include-source; see toolchain/rust/std-rt/glue.rs).
cp "$ROOT/toolchain/rust/std-rt/glue.rs" "$OUT/sysroot/share/kumo/glue.rs"
# The boot payloads the runner repacks.
cp "$IMAGE/EFI/BOOT/BOOTAA64.EFI" "$OUT/boot/EFI/BOOT/"
cp "$IMAGE/EFI/KUMO/kernel/kumo-kernel.elf" "$OUT/boot/EFI/KUMO/kernel/"
cp "$IMAGE/EFI/KUMO/initrd.img" "$IMAGE/EFI/KUMO/nijigumo.conf" "$OUT/boot/EFI/KUMO/"

# Runtime crates: sources verbatim, self-contained manifests generated (the originals
# inherit authors/edition/license/version from the kumo workspace).
for crate in kumo-abi:lib/kumo-abi kumo-ipc:lib/kumo-ipc kumo-rt:userland/kumo-rt; do
    name=${crate%%:*}
    src=${crate#*:}
    mkdir -p "$OUT/crates/$name"
    cp -R "$ROOT/$src/src" "$OUT/crates/$name/"
done
cat > "$OUT/crates/kumo-abi/Cargo.toml" <<'EOF'
[package]
name = "kumo-abi"
version = "0.1.0"
edition = "2024"
publish = false

[workspace]

[lib]
path = "src/lib.rs"
EOF
cat > "$OUT/crates/kumo-ipc/Cargo.toml" <<'EOF'
[package]
name = "kumo-ipc"
version = "0.1.0"
edition = "2024"
publish = false

[workspace]

[dependencies]
kumo-abi = { path = "../kumo-abi" }

[lib]
path = "src/lib.rs"
EOF
cat > "$OUT/crates/kumo-rt/Cargo.toml" <<'EOF'
[package]
name = "kumo-rt"
version = "0.1.0"
edition = "2024"
publish = false

[workspace]

[dependencies]
kumo-abi = { path = "../kumo-abi" }
kumo-ipc = { path = "../kumo-ipc" }
linked_list_allocator = "0.10.6"

[lib]
path = "src/lib.rs"
EOF

# The registry closure, vendored so the SDK builds offline.
for spec in linked_list_allocator-0.10.6 lock_api-0.4.14 scopeguard-1.2.0 spinning_top-0.2.5; do
    [ -d "$VENDOR/$spec" ] || fail "missing $VENDOR/$spec"
    cp -R "$VENDOR/$spec" "$OUT/vendor/$spec"
done

# ---- wrappers (self-relative; never bake absolute SDK paths) ----
cat > "$OUT/bin/kumo-cc" <<'EOF'
#!/bin/sh
# kumo-cc: compile and link a C program for KUMO against this SDK.
set -eu
SDK="$(cd "$(dirname "$0")/.." && pwd)"
[ "$#" -ge 2 ] || { echo "usage: kumo-cc <out-elf> <source.c> [more.c ...]" >&2; exit 2; }
out=$1; shift
cc_bin="${CC_aarch64_unknown_kumo:-$(command -v clang)}"
[ -n "$cc_bin" ] || { echo "kumo-cc: clang is required" >&2; exit 1; }
ld_bin="${LD_aarch64_unknown_kumo:-$(command -v ld.lld || true)}"
[ -n "$ld_bin" ] || { echo "kumo-cc: ld.lld is required" >&2; exit 1; }
work="$(dirname "$out")/kumo-cc-obj"
rm -rf "$work"; mkdir -p "$work"
CFLAGS="--target=aarch64-unknown-none -D__kumo__ -O2 -ffreestanding \
    -fno-pic -fno-pie -fno-stack-protector -ffunction-sections -fdata-sections \
    --sysroot=$SDK/relibc -isystem $SDK/relibc/include"
# shellcheck disable=SC2086
"$cc_bin" $CFLAGS -c "$SDK/relibc/crt0.S" -o "$work/crt0.o"
objects="$work/crt0.o"
index=0
for source in "$@"; do
    index=$((index + 1))
    # shellcheck disable=SC2086
    "$cc_bin" $CFLAGS -c "$source" -o "$work/unit$index.o"
    objects="$objects $work/unit$index.o"
done
# shellcheck disable=SC2086
"$ld_bin" -static --gc-sections -z noexecstack -T "$SDK/relibc/link.ld" \
    -o "$out" $objects "$SDK/relibc/lib/libc.a"
printf 'kumo-cc: %s\n' "$out"
EOF
cat > "$OUT/bin/kumo-ld" <<'EOF'
#!/bin/sh
# kumo-ld: link prebuilt objects into a KUMO image with this SDK's contract.
set -eu
SDK="$(cd "$(dirname "$0")/.." && pwd)"
[ "$#" -ge 2 ] || { echo "usage: kumo-ld <out-elf> <object.o|archive.a ...>" >&2; exit 2; }
out=$1; shift
ld_bin="${LD_aarch64_unknown_kumo:-$(command -v ld.lld || true)}"
[ -n "$ld_bin" ] || { echo "kumo-ld: ld.lld is required" >&2; exit 1; }
exec "$ld_bin" -static --gc-sections -z noexecstack -T "$SDK/relibc/link.ld" \
    -o "$out" "$@" "$SDK/relibc/lib/libc.a"
EOF
cat > "$OUT/bin/kumo-cargo" <<'EOF'
#!/bin/sh
# kumo-cargo: cargo for the SDK. Carries the --sysroot, the link contract, the glue
# path, the vendored registry and the QEMU runner, all relative to this SDK.
set -eu
SDK="$(cd "$(dirname "$0")/.." && pwd)"
TOOLCHAIN="${KUMO_RUSTUP_TOOLCHAIN:-nightly-2026-06-22}"
export RUSTFLAGS="--sysroot=$SDK/sysroot \
-C link-arg=-T$SDK/sysroot/share/kumo/link.ld \
-C link-arg=$SDK/sysroot/share/kumo/start.o \
-C link-arg=$SDK/sysroot/share/kumo/libkumo_shim.a \
-C link-arg=$SDK/sysroot/share/kumo/libc.a \
-C link-arg=-L$SDK/sysroot/share/kumo \
-C link-arg=-z -C link-arg=muldefs"
export KUMO_GLUE="$SDK/sysroot/share/kumo/glue.rs"
exec cargo +"$TOOLCHAIN" \
    --config "target.aarch64-unknown-kumo.runner=[\"$SDK/bin/kumo-run\"]" \
    --config "source.crates-io.replace-with=\"kumo-sdk-vendor\"" \
    --config "source.kumo-sdk-vendor.directory=\"$SDK/vendor\"" \
    "$@" --target "$SDK/targets/aarch64-unknown-kumo.json" -Zjson-target-spec
EOF
cat > "$OUT/bin/kumo-run" <<'PYRUN'
#!/usr/bin/env python3
"""kumo-run: boot one program ELF on QEMU from this SDK alone.

Repacks the SDK's base initrd with `bin/<name>` and a one-line autoexec, stages the ESP
with the SDK's Nijigumo loader and kernel, and streams the serial transcript. Exit 0
when the boot completes with no TOWER, 1 otherwise.
"""
import os, pathlib, shutil, struct, subprocess, sys, tempfile, time

SDK = pathlib.Path(__file__).resolve().parent.parent

def unpack(data):
    assert data[:8] == b"KUMORD01", "bad initrd"
    count = struct.unpack_from("<I", data, 12)[0]
    files = {}
    for i in range(count):
        at = 16 + i * 80
        name = data[at:at + 64].split(b"\0", 1)[0].decode()
        off, size = struct.unpack_from("<QQ", data, at + 64)
        files[name] = data[off:off + size]
    return files

def repack(initrd, files):
    names = list(files.keys())
    header = bytearray(16 + len(names) * 80)
    header[:8] = b"KUMORD01"
    struct.pack_into("<I", header, 8, 1)
    struct.pack_into("<I", header, 12, len(names))
    out = bytearray(header)
    original = initrd.read_bytes()
    for i, name in enumerate(names):
        encoded = name.encode()
        assert len(encoded) < 64
        out[16 + i * 80:16 + i * 80 + len(encoded)] = encoded
        data = files[name]
        struct.pack_into("<QQ", out, 16 + i * 80 + 64, len(out), len(data))
        out.extend(data)
    # Names came from the original table; a new entry appends its name here.
    for i, name in enumerate(names):
        encoded = name.encode()
        out[16 + i * 80:16 + i * 80 + len(encoded)] = encoded
    initrd.write_bytes(out)

def main():
    if len(sys.argv) < 2:
        print("usage: kumo-run <program-elf> [args...]", file=sys.stderr)
        return 2
    elf = pathlib.Path(sys.argv[1]).resolve()
    if not elf.is_file():
        print(f"kumo-run: no such program: {elf}", file=sys.stderr)
        return 2
    name = elf.name
    qemu = shutil.which("qemu-system-aarch64")
    if not qemu:
        print("kumo-run: qemu-system-aarch64 is required", file=sys.stderr)
        return 2
    firmware = pathlib.Path(os.environ.get("KUMO_QEMU_FIRMWARE",
        str(pathlib.Path(qemu).resolve().parent.parent / "share/qemu")))
    code = firmware / "edk2-aarch64-code.fd"
    vars_template = firmware / "edk2-arm-vars.fd"
    if not code.is_file() or not vars_template.is_file():
        print(f"kumo-run: EDK2 firmware missing under {firmware}", file=sys.stderr)
        return 2
    work = pathlib.Path(tempfile.mkdtemp(prefix="kumo-run-"))
    shutil.copytree(SDK / "boot", work / "esp")
    initrd = work / "esp/EFI/KUMO/initrd.img"
    files = unpack(initrd.read_bytes())
    files["etc/autoexec"] = f"run {name}\n".encode()
    files[f"bin/{name}"] = elf.read_bytes()
    repack(initrd, files)
    shutil.copyfile(vars_template, work / "vars.fd")
    serial = work / "serial.log"
    command = [qemu, "-machine", "virt,gic-version=3", "-cpu", "cortex-a72",
               "-m", "2048", "-display", "none", "-monitor", "none",
               "-serial", f"file:{serial}",
               "-drive", f"if=pflash,format=raw,file={code},readonly=on",
               "-drive", f"if=pflash,format=raw,file={work / 'vars.fd'}",
               "-drive", f"file=fat:rw:{work / 'esp'},format=raw,if=virtio",
               "-net", "none"]
    process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    started = time.monotonic()
    ok = False
    while time.monotonic() - started < 90:
        time.sleep(0.25)
        text = serial.read_text(errors="replace") if serial.exists() else ""
        if "TOWER" in text:
            break
        if "autoexec: done" in text or "INDRA login:" in text:
            ok = True
            break
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill(); process.wait()
    text = serial.read_text(errors="replace") if serial.exists() else ""
    print(text)
    shutil.rmtree(work, ignore_errors=True)
    return 0 if ok and "TOWER" not in text else 1

if __name__ == "__main__":
    sys.exit(main())
PYRUN
chmod +x "$OUT/bin/kumo-cc" "$OUT/bin/kumo-ld" "$OUT/bin/kumo-cargo" "$OUT/bin/kumo-run"

# Examples: the gate I-d subjects.
cat > "$OUT/examples/hello-rust/Cargo.toml" <<'EOF'
[package]
name = "hello-rust"
version = "0.1.0"
edition = "2024"
publish = false

[workspace]

[[bin]]
name = "hello-rust"
path = "src/main.rs"

[dependencies]
kumo-abi = { path = "../../crates/kumo-abi" }
kumo-rt = { path = "../../crates/kumo-rt" }

[profile.release]
opt-level = 2
EOF
cat > "$OUT/examples/hello-rust/src/main.rs" <<'EOF'
#![feature(restricted_std)]
// The SDK's entry glue: TLS/TCB, startup intake, exit. Its path arrives via KUMO_GLUE,
// which the kumo-cargo wrapper exports.
include!(env!("KUMO_GLUE"));

fn main() {
    println!("hello-rust: hello from the kumo-sdk -> OK");
    let _ = kumo_rt::log_write(
        kumo_abi::sys::log_level::NOTICE,
        b"hello-rust: hello from the kumo-sdk (ring) -> OK\n",
    );
}
EOF
cat > "$OUT/examples/hello-c/hello-c.c" <<'EOF'
#include <stdio.h>

int main(void) {
    printf("hello-c: hello from the kumo-sdk -> OK\n");
    return 0;
}
EOF
cat > "$OUT/cookbook/README.md" <<'EOF'
# KUMO SDK cookbook (skeleton)

Recipes here describe: fetch a source tree, pin it, apply KUMO patches, build it against
this SDK, and stage the result into a boot image. The two examples beside this file are
the worked reference:

- `examples/hello-rust` uses `bin/kumo-cargo run --release` (the wrapper carries the
  sysroot, the link contract and the QEMU runner).
- `examples/hello-c` uses `bin/kumo-cc hello-c hello-c.c` and `bin/kumo-run ./hello-c`.

No recipe is shipped yet; SH1 delivers the packaging and the runner only.
EOF

# The version stamp: one JSON naming the unit.
python3 - "$OUT" "$TOOLCHAIN" "$ROOT" <<'PY'
import hashlib, json, pathlib, subprocess, sys
out, toolchain, root = pathlib.Path(sys.argv[1]), sys.argv[2], pathlib.Path(sys.argv[3])
def sha(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()
version = subprocess.check_output(["rustc", "+" + toolchain, "--version", "--verbose"], text=True)
stamp = {
    "schema": 1,
    "layout": "kumo-sdk/1",
    "target": "aarch64-unknown-kumo",
    "toolchain": toolchain,
    "rustc": version.strip().splitlines()[0],
    "assembled": "2026-11-10",
    "artifacts": {
        "sysroot/manifest.json": sha(out / "sysroot/manifest.json"),
        "relibc/lib/libc.a": sha(out / "relibc/lib/libc.a"),
        "relibc/crt0.S": sha(out / "relibc/crt0.S"),
        "relibc/link.ld": sha(out / "relibc/link.ld"),
        "boot/EFI/BOOT/BOOTAA64.EFI": sha(out / "boot/EFI/BOOT/BOOTAA64.EFI"),
        "boot/EFI/KUMO/kernel/kumo-kernel.elf": sha(out / "boot/EFI/KUMO/kernel/kumo-kernel.elf"),
        "boot/EFI/KUMO/initrd.img": sha(out / "boot/EFI/KUMO/initrd.img"),
        "sysroot/share/kumo/glue.rs": sha(out / "sysroot/share/kumo/glue.rs"),
    },
    "vendor": sorted(p.name for p in (out / "vendor").iterdir()),
}
(out / "versions.json").write_text(json.dumps(stamp, indent=2) + "\n")
print(f"kumo-sdk: {out} ({len(stamp['artifacts'])} stamped artifacts)")
PY
printf 'kumo-sdk: assembled %s\n' "$OUT"
