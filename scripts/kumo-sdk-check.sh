#!/bin/sh
#j858
# Gate I-d (PLAN/022 SH1): an external developer, given only kumo-sdk/, builds a Rust
# AND a C hello for KUMO and runs both under QEMU without the KUMO source tree. This
# check copies the assembled SDK outside the repo and uses nothing else from it.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SDK_SRC="${KUMO_SDK_OUT:-$ROOT/build/kumo-sdk}"

fail() { printf 'kumo-sdk-check: %s\n' "$1" >&2; exit 1; }

[ -x "$SDK_SRC/bin/kumo-cargo" ] || sh "$ROOT/scripts/kumo-sdk.sh" || fail "sdk assembly failed"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/kumo-sdk-check.XXXXXX")"
cp -R "$SDK_SRC" "$WORK/sdk"
SDK="$WORK/sdk"
printf 'kumo-sdk-check: sdk copy at %s\n' "$SDK"

# Rust hello: build + run through the SDK's own cargo wrapper and QEMU runner.
( cd "$SDK/examples/hello-rust" && "$SDK/bin/kumo-cargo" run --release ) \
    > "$WORK/rust.log" 2>&1 || { tail -40 "$WORK/rust.log"; fail "rust hello failed"; }
grep -q "hello-rust: hello from the kumo-sdk -> OK" "$WORK/rust.log" \
    || { tail -40 "$WORK/rust.log"; fail "rust hello marker missing"; }
grep -q "hello-rust: hello from the kumo-sdk (ring) -> OK" "$WORK/rust.log" \
    || { tail -40 "$WORK/rust.log"; fail "rust ring marker missing"; }
if grep -q "TOWER" "$WORK/rust.log"; then
    fail "rust hello faulted"
fi

# C hello: build with kumo-cc, boot with kumo-run.
( cd "$SDK/examples/hello-c" && "$SDK/bin/kumo-cc" hello-c hello-c.c ) \
    > "$WORK/c-build.log" 2>&1 || { tail -20 "$WORK/c-build.log"; fail "C hello build failed"; }
( cd "$SDK/examples/hello-c" && "$SDK/bin/kumo-run" ./hello-c ) \
    > "$WORK/c.log" 2>&1 || { tail -40 "$WORK/c.log"; fail "C hello boot failed"; }
grep -q "hello-c: hello from the kumo-sdk -> OK" "$WORK/c.log" \
    || { tail -40 "$WORK/c.log"; fail "C hello marker missing"; }
if grep -q "TOWER" "$WORK/c.log"; then
    fail "C hello faulted"
fi

printf 'KUMO-SDK CHECK OK: rust + c hello from a copied SDK, no source tree (%s)\n' "$WORK"
