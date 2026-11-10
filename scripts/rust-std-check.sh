#!/bin/sh
#j853
#j857
# Build KUMO's Rust std smoke from the pinned nightly's rust-src: stage the library tree,
# alias the vendored libc to the linux surface for target_os = "kumo" (the same mechanical
# admission relibc's kumo.patch makes in its headers), apply toolchain/rust/overlay, and
# build with -Z build-std through cargo's __CARGO_TESTS_ONLY_SRC_ROOT hatch.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOOLCHAIN="nightly-2026-06-22"
LIBCRATE="libc-0.2.185"
TARGET="$ROOT/targets/aarch64-unknown-kumo.json"
TOOL="$ROOT/toolchain/rust"
SMOKE="$TOOL/smoke"

fail() { printf 'rust-std-check: %s\n' "$1" >&2; exit 1; }

command -v rustc >/dev/null 2>&1 || fail "rustc is required"
command -v clang >/dev/null 2>&1 || fail "clang is required"
rustc +"$TOOLCHAIN" --version >/dev/null 2>&1 || fail "toolchain $TOOLCHAIN missing"
RUST_SRC="$(rustc +"$TOOLCHAIN" --print sysroot)/lib/rustlib/src/rust"
[ -d "$RUST_SRC/library/std" ] || fail "rust-src missing for $TOOLCHAIN"

stage="${KUMO_RUST_STD_STAGE_OUT:-$ROOT/target/rust-std-stage}"
rm -rf "$stage"
mkdir -p "$stage"
cp -R "$RUST_SRC/library/." "$stage/library/"
printf 'rust-std-check: staged library at %s\n' "$stage"

# 1. The libc crate rides the linux surface: its values and declarations are what relibc
#    exports under standard names. kumo joins every linux cfg exactly as relibc's headers
#    admitted it.
libc_src="$stage/library/vendor/$LIBCRATE/src"
[ -d "$libc_src" ] || fail "vendored $LIBCRATE missing"
python3 - "$libc_src" <<'PY'
import pathlib, sys
root = pathlib.Path(sys.argv[1])
count = 0
for path in root.rglob('*.rs'):
    text = path.read_text()
    updated = text.replace('target_os = "linux"',
                           'any(target_os = "linux", target_os = "kumo")')
    if updated != text:
        path.write_text(updated)
        count += 1
print(f'libc: aliased kumo->linux in {count} files')
PY
python3 - "$stage/library/vendor/$LIBCRATE/.cargo-checksum.json" <<'PY'
import json, sys
path = sys.argv[1]
data = json.load(open(path))
data["files"] = {}
json.dump(data, open(path, "w"))
PY

# 1b. The std sources take the same admission: kumo joins every linux cfg, which selects
#     the long-tested unix+linux arms. The target's env is musl, completing the standard
#     linux/musl pairing relibc's ABI matches.
std_src="$stage/library/std/src"
python3 - "$std_src" <<'PY'
import pathlib, sys
root = pathlib.Path(sys.argv[1])
count = 0
for path in root.rglob('*.rs'):
    text = path.read_text()
    updated = text.replace('target_os = "linux"',
                           'any(target_os = "linux", target_os = "kumo")')
    if updated != text:
        path.write_text(updated)
        count += 1
print(f'std: aliased kumo->linux in {count} files')
PY

# 1c. std's fd sanitizer takes its poll fast path on every unix platform not named in
#     this list; relibc's poll is built on PalEpoll, which kumo does not implement yet,
#     so the call answers ENOSYS and the sanitizer aborts. Joining the list (as redox
#     does) selects the fcntl fallback, which our descriptor table answers correctly.
python3 - "$stage/library/std/src/sys/pal/unix/mod.rs" <<'PY'
import pathlib, sys
path = pathlib.Path(sys.argv[1])
text = path.read_text()
anchor = "miri, // no `poll`"
assert text.count(anchor) == 1, 'sanitize anchor'
text = text.replace(anchor, anchor + "\n            target_os = \"kumo\", // no poll in relibc yet (KUMO)")
path.write_text(text)
print('std: kumo joins the sanitizer no-poll list')
PY

# 2. The KUMO overlay for std sources.
if [ -d "$TOOL/overlay" ]; then
    cp -R "$TOOL/overlay/." "$stage/library/"
fi

# 3. The start object; relibc's static archive is the C side of the link.
clang --target=aarch64-unknown-none -c "$SMOKE/start.S" -o "$stage/start.o" \
    || fail "compiling start.S"
SYSROOT="${KUMO_SYSROOT:-$ROOT/target/kumo-sysroot}"
[ -f "$SYSROOT/lib/libc.a" ] || fail "no $SYSROOT/lib/libc.a; run scripts/relibc-headers.sh first"

# 4. A vendor union: the repo's crates plus std's own vendored set (the rust-src
#    library/vendor tree carries the versions library/Cargo.lock pins). The repo's
#    source replacement points crates-io at vendor/cargo alone, so the build redirits it
#    through command-line config, which outranks the config file.
vendor_union="$stage/vendor"
mkdir -p "$vendor_union"
for src_dir in "$ROOT/vendor/cargo" "$stage/library/vendor"; do
    for crate_dir in "$src_dir"/*; do
        name="${crate_dir##*/}"
        [ -e "$vendor_union/$name" ] || ln -s "$crate_dir" "$vendor_union/$name"
    done
done

# 5. Build std for the target against the staged tree.
export __CARGO_TESTS_ONLY_SRC_ROOT="$stage/library"
export CARGO_TARGET_DIR="${KUMO_RUST_STD_TARGET_DIR:-$ROOT/target/rust-std-smoke}"
# An empty libunwind: rustc's std link line asks for it on unix, but panic=abort and the
# gate program never unwind. Any genuinely referenced symbol surfaces below, in the shim.
stublibs="$stage/stublibs"
mkdir -p "$stublibs"
armar="$(command -v llvm-ar || command -v ar)"
[ -n "$armar" ] || fail "no ar/llvm-ar for the unwind stub"
"$armar" rcs "$stublibs/libunwind.a"
# The kumo shim: the C symbols std references that relibc does not provide.
clang --target=aarch64-unknown-none -O2 -ffreestanding -fno-builtin \
    -c "$TOOL/shim/shim.c" -o "$stage/shim.o" || fail "compiling the shim"
"$armar" rcs "$stublibs/libkumo_shim.a" "$stage/shim.o"
export RUSTFLAGS="-C link-arg=-T$SMOKE/link.ld -C link-arg=$stage/start.o -C link-arg=$stublibs/libkumo_shim.a -C link-arg=$SYSROOT/lib/libc.a -C link-arg=-L$SYSROOT/lib -C link-arg=-L$stublibs -C link-arg=-z -C link-arg=muldefs"
cd "$SMOKE"
cargo +"$TOOLCHAIN" build --release -Z build-std=core,alloc,std,panic_abort -Zjson-target-spec \
    --config 'source.crates-io.replace-with="kumo-std-vendor"' \
    --config "source.kumo-std-vendor.directory=\"$vendor_union\"" \
    --target "$TARGET"

out="$ROOT/target/rust-std-smoke/aarch64-unknown-kumo/release/std-smoke"
[ -f "$out" ] || fail "smoke binary missing at $out"
printf 'KUMO std smoke: %s\n' "$out"
