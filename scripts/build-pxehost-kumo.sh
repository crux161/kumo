#!/usr/bin/env bash
# Build the small KUMO local-file overlay for pxehost's proxyDHCP/TFTP server.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REF="a68256d044314366e2c9e2674f03e1b399821720"
SOURCE="${PXEHOST_SOURCE:-$ROOT/build/pxehost-kumo-source}"
OUTPUT="${KUMO_PXEHOST_BIN:-$ROOT/build/tools/pxehost-kumo}"
PATCH="$ROOT/netboot/pxehost-kumo.patch"

need() {
  command -v "$1" >/dev/null 2>&1 || { echo "error: missing '$1'" >&2; exit 1; }
}
need git
need go

if [ ! -d "$SOURCE/.git" ]; then
  if [ -e "$SOURCE" ]; then
    echo "error: PXEHOST_SOURCE exists but is not a git checkout: $SOURCE" >&2
    exit 1
  fi
  echo "==> Cloning pxehost"
  mkdir -p "$(dirname "$SOURCE")"
  git clone https://github.com/pxehost/pxehost.git "$SOURCE"
fi

if git -C "$SOURCE" apply --reverse --check "$PATCH" >/dev/null 2>&1; then
  echo "==> KUMO pxehost overlay already applied"
else
  if [ -n "$(git -C "$SOURCE" status --porcelain)" ]; then
    echo "error: pxehost source has unrelated changes: $SOURCE" >&2
    echo "       set PXEHOST_SOURCE to a clean checkout" >&2
    exit 1
  fi
  if ! git -C "$SOURCE" cat-file -e "$REF^{commit}" 2>/dev/null; then
    echo "==> Fetching pinned pxehost revision $REF"
    git -C "$SOURCE" fetch origin "$REF"
  fi
  git -C "$SOURCE" checkout --detach "$REF"
  git -C "$SOURCE" apply --check "$PATCH"
  git -C "$SOURCE" apply "$PATCH"
  echo "==> Applied KUMO local-file overlay to pxehost $REF"
fi

mkdir -p "$(dirname "$OUTPUT")" "$ROOT/build/go-cache"
echo "==> Testing pxehost file-provider and bootfile selection"
(cd "$SOURCE" && GOCACHE="$ROOT/build/go-cache" go test ./internal/tftp ./internal/dhcp)
echo "==> Building $OUTPUT"
(cd "$SOURCE" && GOCACHE="$ROOT/build/go-cache" go build -trimpath -o "$OUTPUT" ./cmd/pxehost)

echo "wrote $OUTPUT"
