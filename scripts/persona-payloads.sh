#!/bin/sh
#j888
#j889
# SH7-c: rebuild the Linux-side persona payloads in persona/ from a pinned Alpine image.
# musl's dynamic loader (ld-musl-aarch64.so.1, which is musl's libc.so) and a dynamic musl
# hello built against it. Needs docker (OrbStack) and network for the image and apk.
# Prints the sha256 of each output; persona/MANIFEST records the committed ones. — TEAL
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
IMAGE="alpine@sha256:49340b346798875b5592e5d420241f2f9cfba5f47e62ebb1446a1b34734e898f" # alpine:3.22, arm64
NAME=kumo-persona-payloads-$$
OUT="$ROOT/persona"
mkdir -p "$OUT/musl"
docker run -d --name "$NAME" --platform linux/arm64 "$IMAGE" sleep 600 >/dev/null
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true' EXIT INT TERM
docker exec "$NAME" apk add --no-cache gcc=14.2.0-r6 musl=1.2.5-r13 musl-dev=1.2.5-r13 binutils >/dev/null
docker exec "$NAME" mkdir -p /w
for prog in hello-dyn vmprobe; do
    docker cp "$OUT/$prog.c" "$NAME:/w/$prog.c"
    docker exec "$NAME" sh -c "cd /w && gcc -O2 -o $prog $prog.c && strip $prog"
    docker cp "$NAME:/w/$prog" "$OUT/$prog"
done
docker cp -L "$NAME:/lib/ld-musl-aarch64.so.1" "$OUT/musl/ld-musl-aarch64.so.1"
docker exec "$NAME" wget -q -O /w/COPYRIGHT "https://git.musl-libc.org/cgit/musl/plain/COPYRIGHT?h=v1.2.5"
docker cp "$NAME:/w/COPYRIGHT" "$OUT/musl/COPYRIGHT"
cd "$OUT" && shasum -a 256 hello-dyn vmprobe musl/ld-musl-aarch64.so.1 musl/COPYRIGHT
