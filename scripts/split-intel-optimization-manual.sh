#!/usr/bin/env bash
#j437
#
# Split Intel's pdftotext-layout Optimization Reference Manual extraction into the updated
# chapters present in that document. The PDF remains the regeneration source; this script reads
# only the smaller text extraction.
#
# Usage: scripts/split-intel-optimization-manual.sh [input.txt] [output-dir]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INPUT="${1:-$ROOT/resources/Intel64_IA32-Architecture_Optimization_Reference_Manual.txt}"
OUTPUT="${2:-$ROOT/resources/Intel64_IA32-Architecture_Optimization_Reference_Manual-split}"

[ -f "$INPUT" ] || {
  echo "error: text manual not found: $INPUT" >&2
  exit 1
}
[ ! -e "$OUTPUT" ] || {
  echo "error: output already exists (remove or choose another directory): $OUTPUT" >&2
  exit 1
}
mkdir -p "$OUTPUT"

awk -v dir="$OUTPUT" '
function chapter_number(line, value) {
  value = line
  sub(/^.*[Cc][Hh][Aa][Pp][Tt][Ee][Rr][[:space:]]+/, "", value)
  sub(/[^0-9].*$/, "", value)
  return value + 0
}

function title_slug(line, value) {
  value = tolower(line)
  gsub(/[^a-z0-9]+/, "-", value)
  sub(/-+$/, "", value)
  sub(/^-+/, "", value)
  return value
}

function append_buffer(line) {
  buffered[++buffer_count] = line
}

function open_chapter(title, slug, cursor) {
  slug = title_slug(title)
  out = sprintf("%s/ch%02d-%s.txt", dir, chapter, slug)
  for (cursor = 1; cursor <= buffer_count; cursor++)
    print buffered[cursor] > out
  delete buffered
  buffer_count = 0
  buffering = 0
  waiting_for_title = 0
}

BEGIN {
  front = dir "/00-frontmatter.txt"
  out = front
}

# The extracted manual places a numbered change-note block immediately before every real chapter.
# Start the new file there so those notes do not become the previous chapter tail. The TOC lines
# omit the dot after their list number, which keeps them in frontmatter. — KESTREL 2026-07-14
/^[[:space:]]*[0-9]+\.[[:space:]]+Updates to Chapter[[:space:]]+[0-9]+[[:space:]]*$/ {
  if (out)
    close(out)
  out = ""
  chapter = chapter_number($0)
  buffering = 1
  waiting_for_title = 0
  append_buffer($0)
  next
}

# A real chapter marker occupies its own form-feed-prefixed line. Prose references and the TOC
# have additional text, so they cannot match this anchored form.
/^[[:space:]]*CHAPTER[[:space:]]+[0-9]+[[:space:]]*$/ {
  marker_chapter = chapter_number($0)
  if (!buffering) {
    if (out)
      close(out)
    out = ""
    chapter = marker_chapter
    buffering = 1
  }
  if (marker_chapter != chapter) {
    print "error: change-note/chapter number mismatch" > "/dev/stderr"
    exit 2
  }
  append_buffer($0)
  waiting_for_title = 1
  next
}

buffering {
  append_buffer($0)
  if (waiting_for_title && $0 ~ /[^[:space:]]/)
    open_chapter($0)
  next
}

{ print > out }

END {
  if (buffering) {
    print "error: incomplete chapter heading at end of input" > "/dev/stderr"
    exit 2
  }
}
' "$INPUT"

# Lexical order is document order (`00`, then zero-padded chapter numbers). Normalize the source's
# missing final newline through awk too; all actual records must still reconstruct line-for-line.
if ! cmp -s <(awk '{ print }' "$INPUT") <(cat "$OUTPUT"/*.txt); then
  echo "error: split files do not reconstruct the source line-for-line" >&2
  exit 1
fi

chapter_count="$(find "$OUTPUT" -type f -name 'ch*.txt' | wc -l | tr -d ' ')"
echo "split $INPUT into $chapter_count chapters + frontmatter at $OUTPUT"
echo "lossless line reconstruction verified"
