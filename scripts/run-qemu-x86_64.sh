#!/usr/bin/env bash
#j442
# Build and interactively boot KUMO's working x86_64 Multiboot first-light path.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
exec "$ROOT/scripts/x86-multiboot.sh" run
