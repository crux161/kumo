#!/usr/bin/env bash
#j592
#j637
# build-mali-shaders.sh - rebuild shaders/mali/ from source in a pinned Linux container.
#
# Usage:
#   ./scripts/build-mali-shaders.sh            rebuild into build/mali-shaders/ and compare
#   ./scripts/build-mali-shaders.sh --update   rebuild, then replace the vendored files
#   KUMO_MALI_SHADERS_KEEP=1 ...               keep the container for the next run
#
# PLAN/013 decision 2 (CRUX, 2026-10-24, option A): KUMO vendors the compiled kernel, its
# manifest, its source and this recipe, with Mesa's MIT notice; no Mesa source is in the tree.
# The recipe pins everything that decides the bytes: the container image by digest, Debian's
# package set by snapshot.debian.org timestamp, Mesa's source by tarball sha256, and the
# toolchain versions it then checks. It needs Docker (OrbStack on CRUX's Mac). It builds only
# Mesa's two offline compilers (mesa_clc: OpenCL C -> SPIR-V; panfrost_compile: SPIR-V ->
# Valhall), compiles the paint and triangle kernels for arch 10 (Mali-G610), and runs
# shaders/mali/tools/manifest.c against Mesa's headers to write the binary, the manifest and
# the disassembly. Original recipe — TIMBERDOODLE 2026-10-24;
# triangle extension and stopped-container reuse — KESTREL 2026-10-26
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$ROOT/shaders/mali"
OUT="$ROOT/build/mali-shaders"
UPDATE=0
case "${1:-}" in
  "") ;;
  --update) UPDATE=1 ;;
  -h|--help) sed -n '3,9p' "$0"; exit 0 ;;
  *) echo "error: unknown argument '$1'" >&2; exit 2 ;;
esac

IMAGE="debian@sha256:92d7265e51d67d3a2094c1aad9faa88c74cf9d52e6c354200e429cd60237cc55"
SNAPSHOT="20261024T000000Z"
MESA_VERSION="26.2.5-1"
MESA_ORIG_SHA256="794b3d1729075dae7ebd47c7283b7ca0cdb5dec668f7f11cc0a5d82f6138a0a5"
EXPECT_TOOLS="libclc-22 1:22.1.8-3
llvm-22 1:22.1.8-3
llvm-spirv-22 22.1.0-1
meson 1.12.1-2
spirv-tools 2026.4~rc2-1"
CONTAINER="kumo-mali-shaders"

command -v docker >/dev/null || { echo "error: docker is required (start OrbStack)" >&2; exit 1; }
docker info >/dev/null 2>&1 || { echo "error: the Docker engine is not running" >&2; exit 1; }

if ! docker container inspect "$CONTAINER" >/dev/null 2>&1; then
  echo "==> Starting $CONTAINER from $IMAGE"
  docker run -d --name "$CONTAINER" "$IMAGE" sleep infinity >/dev/null
fi
if [ "$(docker inspect --format '{{.State.Running}}' "$CONTAINER")" != true ]; then
  docker start "$CONTAINER" >/dev/null
fi
cleanup() {
  if [ -z "${KUMO_MALI_SHADERS_KEEP:-}" ]; then
    docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

echo "==> Toolchain: Debian snapshot $SNAPSHOT, Mesa $MESA_VERSION"
docker exec -e SNAPSHOT="$SNAPSHOT" -e MESA_VERSION="$MESA_VERSION" \
  -e MESA_ORIG_SHA256="$MESA_ORIG_SHA256" "$CONTAINER" bash -euo pipefail -c '
  if [ ! -x /work/build/src/panfrost/clc/panfrost_compile ]; then
    export DEBIAN_FRONTEND=noninteractive
    rm -f /etc/apt/sources.list.d/debian.sources
    # Plain HTTP: the bare image has no CA certificates, and apt checks the signed snapshot
    # InRelease against the Debian keyring in the image either way.
    cat > /etc/apt/sources.list.d/snapshot.list <<EOF
deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/$SNAPSHOT sid main
deb-src [check-valid-until=no] http://snapshot.debian.org/archive/debian/$SNAPSHOT sid main
EOF
    apt-get -o Acquire::Retries=5 update -q >/dev/null
    apt-get -o Acquire::Retries=5 build-dep -y -q mesa >/dev/null
    apt-get -o Acquire::Retries=5 install -y -q dpkg-dev >/dev/null
    mkdir -p /work && cd /work
    apt-get -o Acquire::Retries=5 source -q "mesa=$MESA_VERSION" >/dev/null
    echo "$MESA_ORIG_SHA256  mesa_${MESA_VERSION%-*}.orig.tar.xz" | sha256sum -c --quiet
    cd "/work/mesa-${MESA_VERSION%-*}"
    meson setup /work/build -Dbuildtype=release -Dgallium-drivers=panfrost -Dvulkan-drivers= \
      -Dplatforms= -Dglx=disabled -Degl=disabled -Dgbm=disabled -Dllvm=enabled \
      -Dmesa-clc=enabled -Dinstall-mesa-clc=true -Dprecomp-compiler=enabled \
      -Dinstall-precomp-compiler=true >/work/setup.log
    ninja -C /work/build src/compiler/clc/mesa_clc src/panfrost/clc/panfrost_compile \
      src/panfrost/genxml/v10_pack.h >/work/build.log
  fi
  dpkg-query -W -f="\${Package} \${Version}\n" libclc-22 llvm-22 llvm-spirv-22 meson spirv-tools \
    > /work/tools.txt'
TOOLS="$(docker exec "$CONTAINER" cat /work/tools.txt)"
if [ "$TOOLS" != "$EXPECT_TOOLS" ]; then
  printf 'error: toolchain drifted from the pin:\n%s\n' "$TOOLS" >&2
  exit 1
fi

rm -rf "$OUT" && mkdir -p "$OUT"
status=0
for KERNEL in kumo_paint kumo_triangle; do
echo "==> Compiling $KERNEL for arch 10"
docker exec "$CONTAINER" rm -rf /work/kumo && docker exec "$CONTAINER" mkdir -p /work/kumo
docker cp "$SRC/$KERNEL.cl" "$CONTAINER:/work/kumo/$KERNEL.cl"
docker cp "$SRC/tools/manifest.c" "$CONTAINER:/work/kumo/manifest.c"
docker exec -e MESA_VERSION="$MESA_VERSION" -e KERNEL="$KERNEL" "$CONTAINER" bash -euo pipefail -c '
  M="/work/mesa-${MESA_VERSION%-*}"; B=/work/build; cd /work/kumo
  "$B/src/compiler/clc/mesa_clc" -o "${KERNEL}_v10.spv" --depfile "${KERNEL}_v10.d" "$KERNEL.cl" \
    -- -DPAN_ARCH=10 -I"$M/include" -I"$M/src/compiler/libcl" -I"$M/src" -I"$M/src/panfrost" \
    -cl-std=cl2.0 -D__OPENCL_VERSION__=200
  "$B/src/panfrost/clc/panfrost_compile" kumo_shaders 10 "${KERNEL}_v10.spv" "${KERNEL}_v10.h" \
    "${KERNEL}_v10.c"
  # Mesa headers need the defines meson gave its own compiler sources.
  python3 - <<EOF > flags.txt
import json, os, shlex
cmds = json.load(open("/work/build/compile_commands.json"))
c = next(c for c in cmds if c["file"].endswith("src/panfrost/clc/pan_compile.c"))
out = []
for a in shlex.split(c["command"]):
    if a.startswith("-I") and not a[2:].startswith("/"):
        a = "-I" + os.path.normpath(os.path.join(c["directory"], a[2:]))
    if a.startswith(("-D", "-I", "-std", "-f", "-m")) or a == "-pthread":
        out.append(a)
print(" ".join(out))
EOF
  extra=()
  if [ "$KERNEL" = kumo_triangle ]; then extra=(-DBUILD_TRIANGLE); fi
  cc -O1 "${extra[@]}" -o manifest manifest.c -I. $(cat flags.txt) -I"$B/src/panfrost/genxml" \
    -I"$B/src/panfrost" -I"$M/src/panfrost/compiler/bifrost/valhall" \
    -I"$B/src/panfrost/compiler/bifrost/valhall" \
    "$B/src/panfrost/compiler/bifrost/valhall/libpanfrost_valhall_disasm.a" \
    "$B/src/util/libmesa_util.a" -lm -lpthread -lz -lzstd
  ./manifest "$KERNEL.v10.bin" "$KERNEL.v10.dis" > manifest.txt'
docker cp "$CONTAINER:/work/kumo/$KERNEL.v10.bin" "$OUT/$KERNEL.v10.bin"
docker cp "$CONTAINER:/work/kumo/$KERNEL.v10.dis" "$OUT/$KERNEL.v10.dis"
# Keep legacy assets byte-identical; only the new triangle needs EOF normalization. — KESTREL
if [ "$KERNEL" = kumo_triangle ]; then
python3 - "$OUT/$KERNEL.v10.dis" <<'PYDIS'
import pathlib, sys
p = pathlib.Path(sys.argv[1])
p.write_bytes(p.read_bytes().rstrip(b"\n") + b"\n")
PYDIS
fi
docker cp "$CONTAINER:/work/kumo/manifest.txt" "$OUT/manifest.txt"

sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
{
  echo "# $KERNEL.v10.manifest: written by scripts/build-mali-shaders.sh; do not edit."
  echo "# What Mesa's compiler reported for $KERNEL.cl on arch 10, and Mesa's reference"
  echo "# descriptors for it (pan_pack, PAN_ARCH 10), which drv-gpu's packers must match."
  echo "mesa = $MESA_VERSION"
  echo "image = $IMAGE"
  echo "snapshot = $SNAPSHOT"
  echo "$TOOLS" | sed 's/^\([^ ]*\) /tool \1 = /'
  echo "source_sha256 = $(sha "$SRC/$KERNEL.cl")"
  echo "binary_sha256 = $(sha "$OUT/$KERNEL.v10.bin")"
  cat "$OUT/manifest.txt"
} > "$OUT/$KERNEL.v10.manifest"
rm "$OUT/manifest.txt"

for f in "$KERNEL.v10.bin" "$KERNEL.v10.manifest" "$KERNEL.v10.dis"; do
  if [ "$UPDATE" -eq 1 ]; then
    cp "$OUT/$f" "$SRC/$f"
    echo "updated shaders/mali/$f"
  elif cmp -s "$OUT/$f" "$SRC/$f"; then
    echo "match   shaders/mali/$f"
  else
    echo "DIFFER  shaders/mali/$f" >&2
    status=1
  fi
done
done
exit "$status"
