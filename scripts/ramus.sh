#!/usr/bin/env bash
#J934_r00_001
# ramus.sh — GUIDANCE/008 Limbs (Rami): open, subentry, seal, verify, fold, close.
#
#   ramus.sh status
#   ramus.sh open     --slug S --identity ID --key K --scope "one line" [--index XX] [--here | --worktree PATH]
#   ramus.sh handover rXX --identity ID --key K          # new owner stamps HANKO-n after the old owner sealed
#   ramus.sh next     rXX                                # print the next subentry id (use it as the commit subject prefix)
#   ramus.sh subentry rXX --title "T"                    # write that subentry's skeleton with its inverse patch
#   ramus.sh seal     rXX --key K --ritual LOG           # needs REENTRY/rami/rXX/HANDOFF.md and a green ritual log
#   ramus.sh verify   rXX                                # GUIDANCE/008 §5 steps 1-3; changes nothing
#   ramus.sh fold     rXX [--identity ID]                # verify, save refs, ff the trunk, write the mainline journal skeleton
#   ramus.sh close    rXX --key K [--defer-worktree]     # sign + revert --check the journal, remove worktree and branch
#   ramus.sh --self-test                                 # the whole life cycle in a throwaway repository
#
# Records live in the main checkout's REENTRY/ (a worktree reaches it through its REENTRY symlink).
# Bash 3.2 compatible (macOS /bin/bash). Signing uses stock minisign; keys never leave their files.
# A key's password, if it has one, is read from RAMUS_KEY_PASSWORD (empty by default) — never argv.
set -euo pipefail

die() { printf 'ramus.sh: %s\n' "$*" >&2; exit 1; }
say() { printf '==> %s\n' "$*"; }
sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
today() { printf '%s' "${RAMUS_DATE:-$(date +%Y-%m-%d)}"; }

main_root() {
    local common
    common="$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null)" || die "not inside a git checkout"
    dirname "$common"
}

MAIN="$(main_root)"
R="$MAIN/REENTRY"
REG="$R/RAMI.md"
PUBKEYS="$MAIN/PUBKEYS"

trunk() { [ -f "$REG" ] || die "no $REG"; sed -n '1s/^Trunk: //p' "$REG"; }
field() { sed -n "s/^$2: //p" "$1" | head -1; }
idx() { case "$1" in r[0-9][0-9]|r[0-9][0-9][0-9]) printf '%s' "$1" ;; *) die "bad Limb index '$1' (want rXX)" ;; esac; }
ldir() { local d="$R/rami/$1"; [ -d "$d" ] || die "no Limb $1 ($d)"; printf '%s' "$d"; }

# latest generation of HANKO/SEAL in a Limb dir: prints the path, or nothing.
latest() {
    local d="$1" b="$2" n=2 last=""
    [ -f "$d/$b.md" ] && last="$d/$b.md"
    while [ -f "$d/$b-$n.md" ]; do last="$d/$b-$n.md"; n=$((n + 1)); done
    printf '%s' "$last"
}
nextgen() {
    local d="$1" b="$2" n=2
    [ -f "$d/$b.md" ] || { printf '%s' "$d/$b.md"; return; }
    while [ -f "$d/$b-$n.md" ]; do n=$((n + 1)); done
    printf '%s' "$d/$b-$n.md"
}

# Sign with stock minisign through a pty (minisign insists on a terminal for the password).
sign() {
    local file="$1" key="$2" trusted="$3"
    [ -f "$key" ] || die "no secret key at $key"
    rm -f "$file.minisig"
    python3 - "$file" "$key" "$trusted" <<'PY'
import os, pty, select, sys, time
f, key, trusted = sys.argv[1:4]
pid, fd = pty.fork()
if pid == 0:
    os.execvp("minisign", ["minisign", "-Sm", f, "-s", key, "-t", trusted])
pw = os.environ.get("RAMUS_KEY_PASSWORD", "").encode() + b"\n"
buf, sent, end = b"", False, time.time() + 60
while time.time() < end:
    r, _, _ = select.select([fd], [], [], 0.25)
    if not r:
        continue
    try:
        data = os.read(fd, 1024)
    except OSError:
        break
    if not data:
        break
    buf += data
    if not sent and b"assword" in buf:
        time.sleep(0.2)
        os.write(fd, pw)
        sent = True
_, status = os.waitpid(pid, 0)
sys.exit(os.waitstatus_to_exitcode(status))
PY
    [ -f "$file.minisig" ] || die "minisign wrote no signature for $file"
}

verify_sig() {
    local file="$1" id="$2"
    [ -f "$PUBKEYS/$id.pub" ] || die "no PUBKEYS/$id.pub (derive it from $id.base64 first)"
    [ -f "$file.minisig" ] || die "$file is unsigned"
    minisign -Vm "$file" -p "$PUBKEYS/$id.pub" -q >/dev/null 2>&1 || die "signature on $(basename "$file") does not verify as $id"
}

# registry row upsert: the row for rXX is replaced, or appended to the Limbs table.
reg_row() {
    python3 - "$REG" "$1" "$2" <<'PY'
import sys
path, ix, row = sys.argv[1:4]
lines = open(path).read().split("\n")
key = f"| {ix} |"
for i, line in enumerate(lines):
    if line.startswith(key):
        lines[i] = row
        break
else:
    last = max(i for i, line in enumerate(lines) if line.startswith("| r") or line.startswith("|-----"))
    lines.insert(last + 1, row)
open(path, "w").write("\n".join(lines))
PY
}

worktree_of() {   # the worktree that has branch $1 checked out, or nothing
    git -C "$MAIN" worktree list --porcelain | awk -v b="refs/heads/$1" '
        /^worktree /{ wt=substr($0, 10) } $0=="branch " b { print wt; exit }'
}

tracked_clean() { [ -z "$(git -C "$1" status --porcelain --untracked-files=no)" ]; }

newest_journal() {
    ls "$MAIN/JOURNAL" 2>/dev/null | sed -n 's/^\([0-9][0-9][0-9][0-9]*\)\.md$/\1/p' | sort -n | tail -1
}

subentries() {   # sorted subentry files of Limb $1 (dir $2)
    ls "$2" 2>/dev/null | grep -E "^J[0-9]+_$1_[0-9]{3}\.md$" | sort || true
}

# ---------------------------------------------------------------------------------------------

cmd_status() { sed -n '/^| rXX/,$p' "$REG"; }

cmd_open() {
    local slug="" id="" key="" scope="" want="" mode="worktree" wt=""
    while [ $# -gt 0 ]; do
        case "$1" in
            --slug) slug="$2"; shift 2 ;;
            --identity) id="$2"; shift 2 ;;
            --key) key="$2"; shift 2 ;;
            --scope) scope="$2"; shift 2 ;;
            --index) want="$2"; shift 2 ;;
            --here) mode=here; shift ;;
            --worktree) wt="$2"; shift 2 ;;
            *) die "open: unknown argument $1" ;;
        esac
    done
    [ -n "$slug" ] && [ -n "$id" ] && [ -n "$key" ] && [ -n "$scope" ] || die "open needs --slug --identity --key --scope"
    case "$slug" in *[!a-z0-9-]*) die "slug must be lowercase letters, digits and '-'" ;; esac
    [ -f "$PUBKEYS/$id.pub" ] || die "no PUBKEYS/$id.pub"
    local t base ix d i
    t="$(trunk)"; base="$(git -C "$MAIN" rev-parse --verify "$t^{commit}")"
    mkdir -p "$R/rami"
    if [ -n "$want" ]; then
        ix="r$want"; idx "$ix" >/dev/null; d="$R/rami/$ix"
        [ -f "$d/HANKO.md" ] && die "$ix is already stamped"
        [ -d "$d" ] || mkdir "$d" || die "cannot take $ix"
    else
        i=0
        while :; do
            ix="$(printf 'r%02d' "$i")"; d="$R/rami/$ix"
            mkdir "$d" 2>/dev/null && break     # atomic: two sessions cannot take one index
            i=$((i + 1)); [ "$i" -lt 1000 ] || die "no free index"
        done
    fi
    local branch="ramus/$ix-$slug"
    if [ "$mode" = here ]; then
        wt="$(git rev-parse --show-toplevel)"
        [ "$wt" != "$MAIN" ] || { rmdir "$d" 2>/dev/null || true; die "--here must run in a worktree, not the main checkout"; }
        tracked_clean "$wt" || { rmdir "$d" 2>/dev/null || true; die "--here needs a worktree with no tracked changes"; }
        git -C "$wt" switch -q -c "$branch" "$base" || { rmdir "$d" 2>/dev/null || true; die "cannot create $branch"; }
    else
        [ -n "$wt" ] || wt="$MAIN/.claude/worktrees/$ix-$slug"
        git -C "$MAIN" worktree add -q -b "$branch" "$wt" "$base" || { rmdir "$d" 2>/dev/null || true; die "cannot add worktree"; }
    fi
    [ -e "$wt/REENTRY" ] || ln -s "$R" "$wt/REENTRY"
    if [ -f "$MAIN/.gitignore" ] && [ ! -f "$wt/.gitignore" ]; then cp "$MAIN/.gitignore" "$wt/.gitignore"; fi
    local excl; excl="$(git -C "$MAIN" rev-parse --path-format=absolute --git-common-dir)/info/exclude"
    mkdir -p "$(dirname "$excl")"; grep -qx '/REENTRY' "$excl" 2>/dev/null || printf '/REENTRY\n' >> "$excl"
    local bj; bj="$(newest_journal)"; bj="J${bj:-000}"
    cat > "$d/HANKO.md" <<EOF
# KUMO RAMUS $ix HANKO
Index: $ix
Branch: $branch
Worktree: $wt
Identity: $id
Trunk: $t
Base: $base
Base-Journal: $bj
Opened: $(today)
Scope: $scope
EOF
    sign "$d/HANKO.md" "$key" "KUMO RAMUS $ix opened by $id on $(today)"
    verify_sig "$d/HANKO.md" "$id"
    reg_row "$ix" "| $ix | \`$branch\` | $id | \`$(printf '%.8s' "$base")\` $bj | $(today) | OPEN |"
    say "opened $ix: $branch at $(printf '%.8s' "$base") ($bj), worktree $wt, hanko signed by $id"
    say "next subentry: $(cmd_next "$ix")"
}

cmd_handover() {
    local ix; ix="$(idx "$1")"; shift
    local id="" key=""
    while [ $# -gt 0 ]; do case "$1" in --identity) id="$2"; shift 2 ;; --key) key="$2"; shift 2 ;; *) die "handover: unknown $1" ;; esac; done
    local d h s hn; d="$(ldir "$ix")"; h="$(latest "$d" HANKO)"; s="$(latest "$d" SEAL)"
    [ -n "$s" ] || die "handover: the current owner must seal first"
    hn="$(nextgen "$d" HANKO)"
    # each owner's hanko must be followed by that owner's seal before the next hanko
    [ "$(basename "$hn" | sed 's/HANKO//')" = "$(basename "$(nextgen "$d" SEAL)" | sed 's/SEAL//')" ] || die "handover: latest hanko has no seal"
    { sed -e "s/^# KUMO RAMUS $ix HANKO.*/# KUMO RAMUS $ix HANKO (handover)/" -e '/^Identity: /d' -e '/^Opened: /d' "$h"
      printf 'Identity: %s\nPrevious: %s %s\nOpened: %s\n' "$id" "$(field "$h" Identity)" "$(basename "$s")" "$(today)"; } > "$hn"
    sign "$hn" "$key" "KUMO RAMUS $ix handed to $id on $(today)"
    verify_sig "$hn" "$id"
    say "$ix handed over to $id ($(basename "$hn"))"
}

cmd_next() {
    local ix; ix="$(idx "$1")"; local d h bj last n
    d="$(ldir "$ix")"; h="$d/HANKO.md"; bj="$(field "$h" Base-Journal)"
    last="$(subentries "$ix" "$d" | tail -1)"
    if [ -z "$last" ]; then n=1; else n=$((10#$(printf '%s' "$last" | sed -E 's/.*_([0-9]{3})\.md$/\1/') + 1)); fi
    printf '%s_%s_%03d' "$bj" "$ix" "$n"
}

cmd_subentry() {
    local ix; ix="$(idx "$1")"; shift
    local title=""
    while [ $# -gt 0 ]; do case "$1" in --title) title="$2"; shift 2 ;; *) die "subentry: unknown $1" ;; esac; done
    [ -n "$title" ] || die "subentry needs --title"
    local d h branch id sub from tip prev f
    d="$(ldir "$ix")"; h="$(latest "$d" HANKO)"; branch="$(field "$h" Branch)"; id="$(field "$h" Identity)"
    sub="$(cmd_next "$ix")"; f="$d/$sub.md"
    prev="$(subentries "$ix" "$d" | tail -1)"
    if [ -n "$prev" ]; then from="$(field "$d/$prev" Tip)"; else from="$(field "$d/HANKO.md" Base)"; fi
    tip="$(git -C "$MAIN" rev-parse "$branch")"
    [ "$tip" != "$from" ] || die "no commits since $from: commit with subject '$sub: ...' first"
    local bad; bad="$(git -C "$MAIN" log --format='%h %s' "$from..$tip" | grep -v " $sub: " || true)"
    [ -z "$bad" ] || die "commits since the last subentry must be subjects '$sub: ...':
$bad"
    {
        printf '# %s — %s — %s\n<!-- %s -->\n' "$sub" "$title" "$id" "$sub"
        printf 'Date: %s\nLimb: %s (`%s`)\nFrom: %s\nTip: %s\n\n' "$(today)" "$ix" "$branch" "$from" "$tip"
        printf '## What changed\n\n(TODO)\n\n## Verification\n\n(TODO)\n\n## Commits\n\n'
        git -C "$MAIN" log --reverse --format='- %h %s' "$from..$tip"
        printf '\n## Rollback (internal)\n\n`git apply` the patch below inside the Limb to undo this subentry.\n\n'
        printf '<!-- KUMO-REVERT-PATCH:BEGIN %s -->\n```diff\n' "$sub"
    } > "$f"
    git -C "$MAIN" diff --binary "$tip" "$from" >> "$f"
    printf '```\n<!-- KUMO-REVERT-PATCH:END %s -->\n' "$sub" >> "$f"
    say "wrote $f (fill in What changed / Verification)"
}

cmd_seal() {
    local ix; ix="$(idx "$1")"; shift
    local key="" ritual=""
    while [ $# -gt 0 ]; do case "$1" in --key) key="$2"; shift 2 ;; --ritual) ritual="$2"; shift 2 ;; *) die "seal: unknown $1" ;; esac; done
    [ -n "$key" ] && [ -n "$ritual" ] || die "seal needs --key and --ritual"
    local d h id branch t ttip tip wt s gen rb
    d="$(ldir "$ix")"; h="$(latest "$d" HANKO)"; [ -n "$h" ] || die "$ix has no hanko"
    id="$(field "$h" Identity)"; branch="$(field "$h" Branch)"
    verify_sig "$h" "$id"
    [ -s "$d/HANDOFF.md" ] || die "write $d/HANDOFF.md first (vital files, risks, ritual: GUIDANCE/008 §4)"
    [ -f "$ritual" ] && grep -q 'RITUAL-ALL-GREEN' "$ritual" || die "ritual log $ritual does not say RITUAL-ALL-GREEN"
    t="$(trunk)"; ttip="$(git -C "$MAIN" rev-parse "$t")"; tip="$(git -C "$MAIN" rev-parse "$branch")"
    git -C "$MAIN" merge-base --is-ancestor "$ttip" "$tip" || die "the trunk moved: rebase $branch onto $t, rerun the ritual, then seal"
    [ "$ttip" != "$tip" ] || die "$branch has no commits beyond the trunk"
    wt="$(worktree_of "$branch")"
    [ -z "$wt" ] || tracked_clean "$wt" || die "$wt has uncommitted tracked changes"
    local bad; bad="$(git -C "$MAIN" log --format='%h %s' "$ttip..$tip" | grep -Ev " J[0-9]+_${ix}_[0-9]{3}: " || true)"
    [ -z "$bad" ] || die "every Limb commit subject must start JNNN_${ix}_YYY: —
$bad"
    local subs; subs="$(subentries "$ix" "$d")"; [ -n "$subs" ] || die "no subentries in $d"
    local last; last="$(printf '%s\n' "$subs" | tail -1)"
    [ "$(field "$d/$last" Tip)" = "$tip" ] || die "$last does not reach the tip $tip: write a subentry for the newer commits"
    s="$(nextgen "$d" SEAL)"; gen="$(basename "$s" .md | sed 's/^SEAL//')"
    rb="$d/limb-rollback$gen.diff"
    git -C "$MAIN" diff --binary "$tip" "$ttip" > "$rb"
    {
        printf '# KUMO RAMUS %s SEAL%s\n' "$ix" "$gen"
        printf 'Index: %s\nIdentity: %s\nHanko: %s %s\nBranch: %s\nTrunk: %s\nTrunk-Tip: %s\nTip: %s\n' \
            "$ix" "$id" "$(basename "$h")" "$(sha "$h")" "$branch" "$t" "$ttip" "$tip"
        printf 'Commits: %s\nLimb-Rollback: %s %s\nRitual: %s %s\nSealed: %s\n' \
            "$(git -C "$MAIN" rev-list --count "$ttip..$tip")" "$(basename "$rb")" "$(sha "$rb")" "$ritual" "$(sha "$ritual")" "$(today)"
        printf 'Key-Note: minisign proves only that the key signed; blank-password keys can be used by anyone who can read them (GUIDANCE/008 D5).\n'
        printf '%s\n' "$subs" | while read -r f; do printf 'Subentry: %s %s\n' "$f" "$(sha "$d/$f")"; done
        printf '\n## Commits\n\n'; git -C "$MAIN" log --reverse --format='- %h %s' "$ttip..$tip"
        printf '\n## Handoff\n\n'; cat "$d/HANDOFF.md"
    } > "$s"
    sign "$s" "$key" "KUMO RAMUS $ix sealed by $id at $tip"
    if ! minisign -Vm "$s" -p "$PUBKEYS/$id.pub" -q >/dev/null 2>&1; then
        rm -f "$s" "$s.minisig" "$rb"; die "that key is not $id's: seal discarded"
    fi
    reg_row "$ix" "| $ix | \`$branch\` | $id | \`$(printf '%.8s' "$(field "$d/HANKO.md" Base)")\` $(field "$d/HANKO.md" Base-Journal) | $(field "$d/HANKO.md" Opened) | SEALED \`$(printf '%.8s' "$tip")\` |"
    say "sealed $ix at $(printf '%.8s' "$tip") ($(basename "$s"), signed by $id)"
}

cmd_verify() {
    local ix; ix="$(idx "$1")"
    local d s h id f n hs
    d="$(ldir "$ix")"; s="$(latest "$d" SEAL)"; [ -n "$s" ] || die "$ix is not sealed"
    # every hanko generation verifies as its own identity
    for hs in "$d"/HANKO.md "$d"/HANKO-*.md; do [ -f "$hs" ] && verify_sig "$hs" "$(field "$hs" Identity)"; done
    h="$(latest "$d" HANKO)"; id="$(field "$h" Identity)"
    [ "$(field "$s" Identity)" = "$id" ] || die "seal identity $(field "$s" Identity) is not the hanko's $id"
    verify_sig "$s" "$id"
    set -- $(field "$s" Hanko); [ "$1" = "$(basename "$h")" ] && [ "$2" = "$(sha "$h")" ] || die "seal names a different hanko"
    set -- $(field "$s" Limb-Rollback); [ -f "$d/$1" ] && [ "$2" = "$(sha "$d/$1")" ] || die "limb rollback patch changed or missing"
    n=0
    while read -r f; do
        [ -n "$f" ] || continue
        set -- $f
        [ -f "$d/$1" ] && [ "$2" = "$(sha "$d/$1")" ] || die "subentry $1 changed or missing since the seal"
        n=$((n + 1))
    done <<EOF
$(sed -n 's/^Subentry: //p' "$s")
EOF
    [ "$n" = "$(subentries "$ix" "$d" | grep -c .)" ] || die "unsealed subentries exist in $d"
    local branch t; branch="$(field "$s" Branch)"; t="$(field "$s" Trunk)"
    [ "$(git -C "$MAIN" rev-parse "$branch" 2>/dev/null)" = "$(field "$s" Tip)" ] || die "$branch moved after the seal"
    [ "$(git -C "$MAIN" rev-parse "$t")" = "$(field "$s" Trunk-Tip)" ] || die "the trunk moved after the seal: owner must rebase and reseal"
    say "$ix verified: hanko + seal by $id, $n subentries, rollback patch, tip $(printf '%.8s' "$(field "$s" Tip)") on trunk $(printf '%.8s' "$(field "$s" Trunk-Tip)")"
}

cmd_fold() {
    local ix; ix="$(idx "$1")"; shift
    local folder=""
    while [ $# -gt 0 ]; do case "$1" in --identity) folder="$2"; shift 2 ;; *) die "fold: unknown $1" ;; esac; done
    cmd_verify "$ix"
    local d s h t branch tip ttip nnn j
    d="$(ldir "$ix")"; s="$(latest "$d" SEAL)"; h="$(latest "$d" HANKO)"
    [ -n "$folder" ] || folder="$(field "$s" Identity)"
    [ -f "$PUBKEYS/$folder.pub" ] || die "folder $folder has no PUBKEYS key (D4)"
    t="$(field "$s" Trunk)"; branch="$(field "$s" Branch)"; tip="$(field "$s" Tip)"; ttip="$(field "$s" Trunk-Tip)"
    [ "$(git -C "$MAIN" symbolic-ref --short HEAD)" = "$t" ] || die "the main checkout must be on $t"
    tracked_clean "$MAIN" || die "the main checkout has uncommitted tracked changes"
    git -C "$MAIN" update-ref "refs/rami/$ix/pre-fold" "$ttip"
    git -C "$MAIN" update-ref "refs/rami/$ix/sealed" "$tip"
    git -C "$MAIN" merge -q --ff-only "$branch"
    nnn="$(newest_journal)"; nnn=$((10#${nnn:-0} + 1)); j="$MAIN/JOURNAL/$nnn.md"
    [ ! -e "$j" ] || die "$j already exists"
    mkdir -p "$MAIN/JOURNAL"
    {
        printf '# KUMO JOURNAL %s — %s — %s\n<!-- j%s -->\nDate: %s\n\n' "$nnn" "$(field "$h" Scope)" "$folder" "$nnn" "$(today)"
        printf 'REENTRY: GUIDANCE/008 Limb %s (`%s`), opened by %s at `%s` (%s), sealed at `%s` (%s, signed by %s), folded by %s as a fast-forward from `%s`. Refs `refs/rami/%s/pre-fold` and `refs/rami/%s/sealed`. Subentries and seal: REENTRY/rami/%s/.\n\n' \
            "$ix" "$branch" "$(field "$d/HANKO.md" Identity)" "$(field "$d/HANKO.md" Base)" "$(field "$d/HANKO.md" Base-Journal)" \
            "$tip" "$(basename "$s")" "$(field "$s" Identity)" "$folder" "$ttip" "$ix" "$ix" "$ix"
        printf '## Subentries (in order)\n\n'
        sed -n 's/^Subentry: \([^ ]*\) .*/\1/p' "$s" | while read -r f; do printf -- '- %s\n' "$(head -1 "$d/$f" | sed 's/^# //')"; done
        printf '\n## Commits\n\n'; git -C "$MAIN" log --reverse --format='- %h %s' "$ttip..$tip"
        printf '\n'; sed -n '/^## Handoff$/,$p' "$s"
        printf '\n## Rollback\n\n`./scripts/revert.sh %s` reverts the whole Limb. If that fails: `git reset --hard refs/rami/%s/pre-fold` before a push; after a push, apply REENTRY/rami/%s/%s as a new commit; the sealed work stays at `refs/rami/%s/sealed`.\n\n' \
            "$nnn" "$ix" "$ix" "$(field "$s" Limb-Rollback | cut -d' ' -f1)" "$ix"
        printf '<!-- KUMO-REVERT-PATCH:BEGIN %s -->\n```diff\n' "$nnn"
        cat "$d/$(field "$s" Limb-Rollback | cut -d' ' -f1)"
        printf '```\n<!-- KUMO-REVERT-PATCH:END %s -->\n' "$nnn"
    } > "$j"
    printf 'Journal: %s\nFolded-By: %s\nFolded: %s\nPre-Fold: %s\nSealed: %s\n' "$nnn" "$folder" "$(today)" "$ttip" "$tip" > "$d/FOLD.md"
    reg_row "$ix" "| $ix | \`$branch\` | $(field "$d/HANKO.md" Identity) | \`$(printf '%.8s' "$(field "$d/HANKO.md" Base)")\` $(field "$d/HANKO.md" Base-Journal) | $(field "$d/HANKO.md" Opened) | FOLDED → j$nnn (unsigned) |"
    say "folded $ix: $t is now $(printf '%.8s' "$tip"); wrote JOURNAL/$nnn.md — review it, then: ramus.sh close $ix --key <$folder's key>"
}

cmd_close() {
    local ix; ix="$(idx "$1")"; shift
    local key="" defer=0
    while [ $# -gt 0 ]; do case "$1" in --key) key="$2"; shift 2 ;; --defer-worktree) defer=1; shift ;; *) die "close: unknown $1" ;; esac; done
    [ -n "$key" ] || die "close needs --key"
    local d fold nnn folder j branch wt tip
    d="$(ldir "$ix")"; fold="$d/FOLD.md"; [ -f "$fold" ] || die "$ix is not folded"
    nnn="$(field "$fold" Journal)"; folder="$(field "$fold" Folded-By)"; j="$MAIN/JOURNAL/$nnn.md"
    case "$(head -1 "$j")" in *" $folder") ;; *) die "JOURNAL/$nnn.md's title must end with $folder" ;; esac
    sign "$j" "$key" "KUMO JOURNAL $nnn signed by $folder on $(today)"
    verify_sig "$j" "$folder"
    (cd "$MAIN" && ./scripts/revert.sh --check "$nnn") || die "revert.sh --check $nnn failed"
    branch="$(field "$(latest "$d" SEAL)" Branch)"; tip="$(field "$fold" Sealed)"
    wt="$(worktree_of "$branch")"
    if [ -n "$wt" ]; then
        if [ "$defer" = 1 ]; then
            git -C "$wt" switch -q --detach
            say "worktree $wt detached; it is removed when its session is archived"
        else
            [ -d "$wt/PUBKEYS" ] && chmod -R u+w "$wt/PUBKEYS"
            git -C "$MAIN" worktree remove --force "$wt"
        fi
    fi
    git -C "$MAIN" branch -q -d "$branch"
    reg_row "$ix" "| $ix | \`$branch\` | $(field "$d/HANKO.md" Identity) | \`$(printf '%.8s' "$(field "$d/HANKO.md" Base)")\` $(field "$d/HANKO.md" Base-Journal) | $(field "$d/HANKO.md" Opened) | CLOSED → j$nnn \`$(printf '%.8s' "$tip")\` |"
    say "closed $ix: JOURNAL/$nnn.md signed by $folder, branch deleted"
}

# ---------------------------------------------------------------------------------------------

self_test() {
    local here m k1 k2
    here="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
    RAMUS_TMP="$(mktemp -d "${TMPDIR:-/tmp}/ramus-test.XXXXXX")"; local tmp="$RAMUS_TMP"; m="$tmp/main"
    trap 'rm -rf "$RAMUS_TMP"' EXIT
    expect_fail() {   # expect_fail "message fragment" command...
        local want="$1"; shift
        if "$@" >"$tmp/err" 2>&1; then echo "SELF-TEST FAIL: succeeded: $*"; exit 1; fi
        grep -q -- "$want" "$tmp/err" || { echo "SELF-TEST FAIL: '$*' refused for the wrong reason:"; cat "$tmp/err"; exit 1; }
    }
    mkdir -p "$m/scripts" "$m/JOURNAL" "$m/PUBKEYS" "$m/REENTRY"
    cp "$here" "$m/scripts/ramus.sh"; cp "$MAIN/scripts/revert.sh" "$m/scripts/revert.sh"   # revert.sh is local-only
    printf '/JOURNAL/\n/PUBKEYS/\n/REENTRY/\n' > "$m/.gitignore"
    printf 'one\n' > "$m/a.txt"
    git -C "$m" init -q -b trunk; git -C "$m" add -A; git -C "$m" -c user.name=t -c user.email=t@t commit -q -m j100
    printf '# KUMO JOURNAL 100 — base — TESTER\n' > "$m/JOURNAL/100.md"
    printf 'Trunk: trunk\n\n## Limbs\n\n| rXX | branch | identity | base | opened | state |\n|-----|--------|----------|------|--------|-------|\n' > "$m/REENTRY/RAMI.md"
    k1="$tmp/TESTER.key"; k2="$tmp/OTHER.key"
    minisign -G -W -p "$m/PUBKEYS/TESTER.pub" -s "$k1" >/dev/null
    minisign -G -W -p "$m/PUBKEYS/OTHER.pub" -s "$k2" >/dev/null
    export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
    cd "$m"
    R0="$tmp/wt0"
    ./scripts/ramus.sh open --slug demo --identity TESTER --key "$k1" --scope "self-test Limb" --worktree "$R0" >/dev/null
    ./scripts/ramus.sh open --slug other --identity OTHER --key "$k2" --scope "second" --worktree "$tmp/wt1" >/dev/null
    [ -f REENTRY/rami/r00/HANKO.md.minisig ] && [ -f REENTRY/rami/r01/HANKO.md ] || { echo "SELF-TEST FAIL: indexes"; exit 1; }
    [ -L "$R0/REENTRY" ] || { echo "SELF-TEST FAIL: no REENTRY symlink"; exit 1; }
    [ "$(./scripts/ramus.sh next r00)" = J100_r00_001 ] || { echo "SELF-TEST FAIL: next"; exit 1; }
    printf 'two\n' >> "$R0/a.txt"; git -C "$R0" commit -qam "J100_r00_001: add two"
    expect_fail "HANDOFF.md first" ./scripts/ramus.sh seal r00 --key "$k1" --ritual /dev/null          # no handoff, no ritual
    ./scripts/ramus.sh subentry r00 --title "add two" >/dev/null
    printf 'three\n' >> "$R0/a.txt"; git -C "$R0" commit -qam "wrong subject"
    expect_fail "must be subjects" ./scripts/ramus.sh subentry r00 --title "bad"                     # subject must name the subentry
    git -C "$R0" commit -q --amend -m "J100_r00_002: add three"
    ./scripts/ramus.sh subentry r00 --title "add three" >/dev/null
    printf 'vital: a.txt\n' > REENTRY/rami/r00/HANDOFF.md
    printf 'FAILED\n' > "$tmp/red.log"
    expect_fail "RITUAL-ALL-GREEN" ./scripts/ramus.sh seal r00 --key "$k1" --ritual "$tmp/red.log"
    printf 'RITUAL-ALL-GREEN\n' > "$tmp/ritual.log"
    expect_fail "not TESTER" ./scripts/ramus.sh seal r00 --key "$k2" --ritual "$tmp/ritual.log"   # someone else's key
    [ ! -f REENTRY/rami/r00/SEAL.md ] || { echo "SELF-TEST FAIL: wrong-key seal kept"; exit 1; }
    ./scripts/ramus.sh seal r00 --key "$k1" --ritual "$tmp/ritual.log" >/dev/null
    ./scripts/ramus.sh verify r00 >/dev/null
    cp REENTRY/rami/r00/J100_r00_001.md "$tmp/keep"; printf 'tamper\n' >> REENTRY/rami/r00/J100_r00_001.md
    expect_fail "changed or missing" ./scripts/ramus.sh verify r00   # edited subentry
    cp "$tmp/keep" REENTRY/rami/r00/J100_r00_001.md
    printf 'x\n' > "$R0/b.txt"; git -C "$R0" add b.txt; git -C "$R0" commit -qm "J100_r00_003: late"
    expect_fail "moved after the seal" ./scripts/ramus.sh verify r00   # branch moved after seal
    git -C "$R0" reset -q --hard HEAD~1
    printf 'c\n' > c.txt; git add c.txt; git commit -qm "j101 elsewhere"
    expect_fail "trunk moved" ./scripts/ramus.sh verify r00   # trunk moved
    git reset -q --hard HEAD~1
    ./scripts/ramus.sh fold r00 >/dev/null
    [ "$(git rev-parse trunk)" = "$(git rev-parse refs/rami/r00/sealed)" ] || { echo "SELF-TEST FAIL: not folded"; exit 1; }
    [ -f JOURNAL/101.md ] || { echo "SELF-TEST FAIL: no journal 101"; exit 1; }
    ./scripts/ramus.sh close r00 --key "$k1" >/dev/null
    [ ! -d "$R0" ] && ! git rev-parse -q --verify ramus/r00-demo >/dev/null || { echo "SELF-TEST FAIL: not closed"; exit 1; }
    grep -q '| r00 | .*CLOSED → j101' REENTRY/RAMI.md || { echo "SELF-TEST FAIL: registry"; exit 1; }
    ./scripts/revert.sh 101 >/dev/null; [ "$(cat a.txt)" = one ] || { echo "SELF-TEST FAIL: Limb Rollback"; exit 1; }
    echo "ramus.sh self-test green: open x2 (atomic index), next, subentry subject rule, seal refusals (handoff, ritual, wrong key), verify refusals (subentry edit, branch moved, trunk moved), fold, close, Limb Rollback"
}

case "${1:-}" in
    status) cmd_status ;;
    open) shift; cmd_open "$@" ;;
    handover) shift; cmd_handover "$@" ;;
    next) shift; cmd_next "$@" ;;
    subentry) shift; cmd_subentry "$@" ;;
    seal) shift; cmd_seal "$@" ;;
    verify) shift; cmd_verify "$@" ;;
    fold) shift; cmd_fold "$@" ;;
    close) shift; cmd_close "$@" ;;
    --self-test) self_test ;;
    *) sed -n '3,15p' "$0" >&2; exit 2 ;;
esac
