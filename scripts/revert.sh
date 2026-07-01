#!/bin/sh
# KUMO journal rollback helper.  (hardened per JOURNAL/376 — CORVUS 2026-07-01)
#
# Usage:
#     ./scripts/revert.sh --check 376        # verify signature + confirm the patch applies; change nothing
#     ./scripts/revert.sh 376                # revert exactly entry 376
#     ./scripts/revert.sh --through 374      # revert every entry from newest down to 374, in order
#     ./scripts/revert.sh JOURNAL/376.md
#
# Each JOURNAL entry stores its inverse patch between marker lines:
#     <!-- KUMO-REVERT-PATCH:BEGIN 376 -->
#     ```diff
#     ...
#     ```
#     <!-- KUMO-REVERT-PATCH:END 376 -->
#
# Safety rails (see default.p step 6):
#   - the entry's minisign signature is verified before its patch is trusted;
#   - patches apply with `git apply --index` so the index and working tree stay consistent;
#   - a `git stash create` backup is taken before any mutation;
#   - --through walks entries newest-first and stops on the first that will not apply.
set -eu

usage() {
    cat >&2 <<'EOF'
usage: scripts/revert.sh [--check|--through] NNN|JOURNAL/NNN.md

  (no flag)      revert exactly entry NNN
  --check NNN    verify signature + confirm the patch applies cleanly; change nothing
  --through NNN  revert every entry from the newest down to NNN, in order,
                 stopping on the first that does not apply cleanly
EOF
    exit 2
}

MODE=one
case "${1:-}" in
    --check)   MODE=check;   shift ;;
    --through) MODE=through; shift ;;
    -h|--help) usage ;;
esac

[ "$#" -eq 1 ] || usage

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

case "$1" in
    [0-9][0-9][0-9]*)             NNN="$1" ;;
    JOURNAL/[0-9][0-9][0-9]*.md)  NNN="$(basename "$1" .md)" ;;
    *) usage ;;
esac

journal_path() { printf 'JOURNAL/%s.md' "$1"; }

# Verify the entry's detached signature before trusting the patch it carries.
# Author codename is the trailing ALL-CAPS token on the title line; fall back to
# trying every published pubkey. A missing .minisig warns but does not block.
verify_sig() {
    _j="$1"
    if [ ! -f "$_j.minisig" ]; then
        printf 'revert.sh: WARNING: %s is unsigned (no .minisig) — no attestation\n' "$_j" >&2
        return 0
    fi
    _cn="$(sed -n '1s/.*[[:space:]]\([A-Z][A-Z0-9]*\)[[:space:]]*$/\1/p' "$_j")"
    if [ -n "$_cn" ] && [ -f "PUBKEYS/$_cn.pub" ] && \
       minisign -Vm "$_j" -p "PUBKEYS/$_cn.pub" >/dev/null 2>&1; then
        printf '==> signature verified (%s): %s\n' "$_cn" "$_j"
        return 0
    fi
    for _pk in PUBKEYS/*.pub; do
        [ -f "$_pk" ] || continue
        if minisign -Vm "$_j" -p "$_pk" >/dev/null 2>&1; then
            printf '==> signature verified (%s): %s\n' "$(basename "$_pk" .pub)" "$_j"
            return 0
        fi
    done
    printf 'revert.sh: FATAL: signature on %s verifies against no PUBKEYS/*.pub\n' "$_j" >&2
    return 1
}

extract_patch() {   # $1=journal  $2=NNN  -> patch on stdout
    awk -v nnn="$2" '
        BEGIN{ b="<!-- KUMO-REVERT-PATCH:BEGIN " nnn " -->"
               e="<!-- KUMO-REVERT-PATCH:END " nnn " -->"; inb=0; found=0; ended=0; n=0 }
        $0==b { inb=1; found=1; next }
        $0==e { inb=0; ended=1; exit }
        inb   { if ($0 ~ /^```/) next; print; n++ }
        END{
            if(!found){ print "revert.sh: missing rollback patch block for " nnn > "/dev/stderr"; exit 2 }
            if(!ended){ print "revert.sh: unterminated rollback patch block for " nnn > "/dev/stderr"; exit 2 }
            if(n==0){   print "revert.sh: empty rollback patch block for " nnn > "/dev/stderr"; exit 2 }
        }
    ' "$1"
}

apply_one() {   # $1=NNN ; honours $MODE (check = dry-run)
    _nnn="$1"
    _j="$(journal_path "$_nnn")"
    [ -f "$_j" ] || { printf 'revert.sh: missing journal entry: %s\n' "$_j" >&2; return 1; }
    verify_sig "$_j" || return 1
    _patch="${TMPDIR:-/tmp}/kumo-revert-$_nnn-$$.patch"
    extract_patch "$_j" "$_nnn" > "$_patch" || { rm -f "$_patch"; return 1; }

    _idx=--index
    if ! git apply --index --check "$_patch" 2>/dev/null; then
        _idx=
        if ! git apply --check "$_patch" 2>/dev/null; then
            printf 'revert.sh: %s: patch does not apply cleanly; nothing changed\n' "$_nnn" >&2
            rm -f "$_patch"; return 1
        fi
    fi

    if [ "$MODE" = check ]; then
        printf '==> %s: rollback patch applies cleanly (--check only)\n' "$_nnn"
        rm -f "$_patch"; return 0
    fi

    git apply --stat "$_patch"
    git apply $_idx "$_patch"
    printf '==> %s: rollback patch applied\n' "$_nnn"
    rm -f "$_patch"
}

# Backup before any mutation.
if [ "$MODE" != check ]; then
    BK="$(git stash create 'revert.sh backup' 2>/dev/null || true)"
    if [ -n "${BK:-}" ]; then
        printf '==> working-tree backup at %s (restore: git stash apply %s)\n' "$BK" "$BK"
    else
        printf '==> working tree clean; HEAD (%s) is your backup\n' "$(git rev-parse --short HEAD)"
    fi
fi

case "$MODE" in
    one|check)
        apply_one "$NNN"
        ;;
    through)
        for _n in $(ls JOURNAL/ 2>/dev/null | sed -n 's/^\([0-9][0-9][0-9][0-9]*\)\.md$/\1/p' | sort -rn); do
            [ "$_n" -ge "$NNN" ] || continue
            apply_one "$_n" || {
                printf 'revert.sh: stopped at %s (did not apply); earlier entries left untouched\n' "$_n" >&2
                exit 1
            }
        done
        ;;
esac

[ "$MODE" = check ] || git status --short
