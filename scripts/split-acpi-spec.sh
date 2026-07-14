#!/usr/bin/env bash
#j440
#
# Split the pdftotext-layout ACPI 6.3 extraction at its 21 real chapter heads, appendices,
# and index. The PDF/text source remains the regeneration authority; this script makes
# individual sections small enough to consult directly.
#
# Usage: scripts/split-acpi-spec.sh [input.txt] [output-dir]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INPUT="${1:-$ROOT/resources/docs/ACPI 6.3 Final (30 Jan 2019).txt}"
OUTPUT="${2:-$ROOT/resources/docs/ACPI-6.3-split}"

[ -f "$INPUT" ] || {
  echo "error: ACPI text not found: $INPUT" >&2
  exit 1
}
[ ! -e "$OUTPUT" ] || {
  echo "error: output already exists (remove or choose another directory): $OUTPUT" >&2
  exit 1
}
mkdir -p "$OUTPUT"

awk -v dir="$OUTPUT" '
function slugify(value) {
  value = tolower(value)
  gsub(/[^a-z0-9]+/, "-", value)
  sub(/-+$/, "", value)
  sub(/^-+/, "", value)
  return value
}

function chapter_heading(line, normalized, expected) {
  normalized = line
  sub(/^\f/, "", normalized)
  sub(/^[[:space:]]+/, "", normalized)
  expected = next_chapter " " chapter_title[next_chapter]
  if (normalized != expected)
    return 0
  heading_number = next_chapter
  heading_title = chapter_title[next_chapter]
  return 1
}

function switch_output(path) {
  if (out)
    close(out)
  out = path
}

BEGIN {
  chapter_title[1] = "Introduction"
  chapter_title[2] = "Definition of Terms"
  chapter_title[3] = "ACPI Concepts"
  chapter_title[4] = "ACPI Hardware Specification"
  chapter_title[5] = "ACPI Software Programming Model"
  chapter_title[6] = "Device Configuration"
  chapter_title[7] = "Power and Performance Management"
  chapter_title[8] = "Processor Configuration and Control"
  chapter_title[9] = "ACPI-Defined Devices and Device-Specific Objects"
  chapter_title[10] = "Power Source and Power Meter Devices"
  chapter_title[11] = "Thermal Management"
  chapter_title[12] = "ACPI Embedded Controller Interface"
  chapter_title[13] = "ACPI System Management Bus Interface"
  chapter_title[14] = "Platform Communications Channel (PCC)"
  chapter_title[15] = "System Address Map Interfaces"
  chapter_title[16] = "Waking and Sleeping"
  chapter_title[17] = "Non-Uniform Memory Access (NUMA)"
  chapter_title[18] = "ACPI Platform Error Interfaces (APEI)"
  chapter_title[19] = "ACPI Source Language (ASL) Reference"
  chapter_title[20] = "ACPI Machine Language (AML) Specification"
  chapter_title[21] = "ACPI Data Tables and Table Definition Language"
  next_chapter = 1
  out = dir "/00-frontmatter.txt"
}

next_chapter <= 21 && chapter_heading($0) {
  switch_output(sprintf("%s/ch%02d-%s.txt", dir, heading_number, slugify(heading_title)))
  next_chapter++
  print > out
  next
}

next_chapter == 22 && /^\fAppendix [A-Z]:[[:space:]]*/ {
  appendix_count++
  appendix_title = $0
  sub(/^\fAppendix [A-Z]:[[:space:]]*/, "", appendix_title)
  switch_output(sprintf("%s/zz%02d-appendix-%s.txt", dir, appendix_count, slugify(appendix_title)))
  print > out
  next
}

next_chapter == 22 && appendix_count == 3 && !index_started && \
    /^\fACPI Specification, Version 6\.3[[:space:]]+Index[[:space:]]*$/ {
  switch_output(dir "/zz04-index.txt")
  index_started = 1
  print > out
  next
}

{ print > out }

END {
  if (next_chapter != 22) {
    print "error: expected 21 ordered chapter heads; next was " next_chapter > "/dev/stderr"
    exit 2
  }
  if (appendix_count != 3) {
    print "error: expected 3 appendices; found " appendix_count > "/dev/stderr"
    exit 2
  }
  if (!index_started) {
    print "error: index boundary not found" > "/dev/stderr"
    exit 2
  }
}
' "$INPUT"

# Lexical file order is source order: frontmatter, zero-padded chapters, then zz appendices/index.
# Normalize the source final newline through awk; every source record must reconstruct in order.
if ! cmp -s <(awk '{ print }' "$INPUT") <(cat "$OUTPUT"/*.txt); then
  echo "error: split files do not reconstruct the source line-for-line" >&2
  exit 1
fi

awk 'BEGIN {
  print "# ACPI 6.3 chapter corpus"
  print ""
  print "Lossless split of `ACPI 6.3 Final (30 Jan 2019).txt`; retain the original as authority."
  print "The ordered `.txt` files reconstruct every source record. — KESTREL 2026-07-14"
  print ""
  print "## KUMO routing"
  print ""
  print "- `ch05-acpi-software-programming-model.txt` — RSDP/RSDT/XSDT, table headers and checksums, FADT, MADT, MCFG, HPET, SRAT, GTDT, and related discovery tables."
  print "- `ch06-device-configuration.txt` — namespace device discovery, resources, and PCI host bridges."
  print "- `ch15-system-address-map-interfaces.txt` — system address-map interfaces."
  print "- `ch17-non-uniform-memory-access-numa.txt` — NUMA platform model."
  print "- `ch18-acpi-platform-error-interfaces-apei.txt` — firmware error reporting."
  print "- `ch21-acpi-data-tables-and-table-definition-language.txt` — table-generation definitions."
}' > "$OUTPUT/INDEX.md"

chapter_count="$(find "$OUTPUT" -type f -name 'ch*.txt' | wc -l | tr -d ' ')"
largest_file="$(find "$OUTPUT" -type f -name '*.txt' -exec stat -f '%z %N' {} + | sort -nr | head -n 1)"
largest_bytes="${largest_file%% *}"
if [ "$largest_bytes" -gt 1048576 ]; then
  echo "error: largest split text exceeds 1 MiB: $largest_file" >&2
  exit 1
fi
echo "split $INPUT into $chapter_count chapters + frontmatter, 3 appendices, and index"
echo "lossless line reconstruction verified"
echo "largest split text: $largest_file bytes/path"
