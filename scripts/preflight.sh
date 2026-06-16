#!/bin/sh
# preflight.sh — mechanical guardrail tripwires (GUIDANCE/006 §5).
#
# The machine, not a teammate, catches drift. Run before you say "done":
#     ./scripts/preflight.sh          # fast: fmt + VEIL grep + register ratchet
#     ./scripts/preflight.sh --full   # also build both backends + qemu-smoke (needs nightly)
# Also reachable as `cargo xtask preflight` (KUMO_PREFLIGHT_FULL=1 for --full).
#
# Exit code is nonzero on the FIRST violation so CI fails loudly. Each check prints a
# one-line verdict so a human can see what tripped.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

FULL=0
[ "${1:-}" = "--full" ] && FULL=1

# Existing-debt baseline for register names in kernel/ *code* (PLAN §7, GUIDANCE/006 §2.3).
# Doc-comments that explain a backend mechanism are NOT policed — only code lines count.
# DEBT TO RATCHET DOWN, never up. The 1 remaining is the x86_64 long-mode boot trampoline
# in kernel/src/main.rs (a separate relocation slice). New code leaks fail the build.
REGISTER_LEAK_BASELINE=1

fail() { printf '  FAIL  %s\n' "$1"; exit 1; }
pass() { printf '  ok    %s\n' "$1"; }

echo "==> preflight tripwires (root: $ROOT)"

# 1. Formatting — harmony made visible.
if cargo fmt --check >/dev/null 2>&1; then
    pass "cargo fmt --check"
else
    fail "cargo fmt --check (run 'cargo fmt')"
fi

# 2. VEIL sterility — no esoteric name as a Rust identifier (PLAN §20 rule).
VEIL='Ziwei|Pleroma|Tulpa|Hierophant|Sigil|Aether|Leyline|Homunculus|Vanguard|Houtu|Gouchen|Taiyi|Nanji|Strata|Athanor|Ouroboros|Siming|Signet|Investiture'
veil_hits="$(grep -rnE "\\b(struct|enum|fn|mod|trait|type|const|static)[[:space:]]+($VEIL)\\b" \
    --include='*.rs' kernel hal lib userland boot 2>/dev/null || true)"
if [ -n "$veil_hits" ]; then
    printf '%s\n' "$veil_hits"
    fail "VEIL name used as a Rust identifier (sterile names only in code)"
fi
pass "VEIL sterility (no esoteric identifiers)"

# 3. Register-name leak ratchet — kernel/ *code* names no register (PLAN §7). Comment lines
#    (// and ///) are excluded: documentation may name a backend mechanism; code may not.
REGISTER_RE='TTBR[01]|VBAR_EL|SCTLR|MAIR_EL|TCR_EL|GICR_|GICD_|ICC_[A-Z]|VTTBR|EPTP|\bcr3\b|IA32_[A-Z]'
leaks="$(grep -rEn "$REGISTER_RE" kernel/src --include='*.rs' 2>/dev/null | grep -vcE ':[[:space:]]*//' || true)"
if [ "$leaks" -gt "$REGISTER_LEAK_BASELINE" ]; then
    fail "register names in kernel/ grew to $leaks (baseline $REGISTER_LEAK_BASELINE) — route through a kumo-hal trait"
elif [ "$leaks" -lt "$REGISTER_LEAK_BASELINE" ]; then
    printf '  note  register leaks down to %s (baseline %s) — lower REGISTER_LEAK_BASELINE in this script to lock it in\n' "$leaks" "$REGISTER_LEAK_BASELINE"
    pass "register-leak ratchet (improved)"
else
    pass "register-leak ratchet ($leaks, at baseline)"
fi

# 4. Host tests for the host-testable crates (the cheap correctness floor). The no_std
#    binaries (sora, kumo-rt, niji-*) define their own panic_impl and cannot link the std
#    test harness, so they are validated by the image + qemu-smoke path (--full), not here.
#    Add new host-testable crates (e.g. a new server) to this list.
HOST_TEST_CRATES="kumo-abi kumo-ipc kernel persona-linux kumoza svc-health"
for p in $HOST_TEST_CRATES; do
    if cargo test -p "$p" --quiet >/dev/null 2>&1; then
        pass "cargo test -p $p"
    else
        fail "cargo test -p $p"
    fi
done

if [ "$FULL" -eq 1 ]; then
    echo "==> --full: both-backend build + qemu-smoke (requires the nightly build-std toolchain)"
    cargo xtask image --arch aarch64 --hardware qemu-virt-aarch64 || fail "aarch64 image build"
    cargo xtask image --arch x86_64 --hardware generic-uefi-x86_64 || fail "x86_64 image build (anti-fork guarantee)"
    cargo xtask qemu-smoke --arch aarch64 || fail "qemu-smoke (must reach the ziwei> prompt)"
    pass "both-backend build + qemu-smoke"
fi

echo "==> preflight green"
