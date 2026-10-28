#!/usr/bin/env bash
#j661
# bmp-convert.sh - convert images into the BMP form KUMO's bmp-view consumes quickly.
#
# bmp-view accepts an uncompressed 24-bit or 32-bit BMP up to 16 MiB, but its inline file
# read copies 256 bytes per request, so large photos crawl over a removable volume. A
# 256x128 24-bit BMP is 98358 bytes and copies promptly. This script fits any image inside
# the view, preserves aspect ratio, centers it on black, and writes an uncompressed 24-bit
# BMP (ImageMagick's BMP3 coder).
#
# Usage:
#   ./scripts/bmp-convert.sh [--out DIR] IMAGE [IMAGE...]
#
# Output is written next to each input as <stem>.kumo.bmp, or into DIR. Copy the result to
# the KUMO volume and run: bmp-view <name>.kumo.bmp
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
usage: ./scripts/bmp-convert.sh [--out DIR] IMAGE [IMAGE...]

Create 256x128, 24-bit, uncompressed BMPs for bmp-view. Requires ImageMagick
(magick or convert). Aspect ratio is preserved and the image is centered on black.
USAGE
}

OUT=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    --out)
      if [ "$#" -lt 2 ]; then
        echo "error: --out needs a directory" >&2
        exit 2
      fi
      OUT="$2"
      shift 2
      ;;
    --)
      shift
      break
      ;;
    -*)
      echo "error: unknown option '$1'" >&2
      usage
      exit 2
      ;;
    *)
      break
      ;;
  esac
done

if [ "$#" -eq 0 ]; then
  usage
  exit 2
fi

if command -v magick >/dev/null 2>&1; then
  CONVERT=(magick)
elif command -v convert >/dev/null 2>&1; then
  CONVERT=(convert)
else
  echo "error: ImageMagick is required (install 'magick', or 'convert' on older packages)" >&2
  exit 1
fi

if [ -n "$OUT" ]; then
  mkdir -p "$OUT"
fi

for input in "$@"; do
  if [ ! -f "$input" ]; then
    echo "error: not a file: $input" >&2
    exit 1
  fi
  base=$(basename "$input")
  stem=${base%.*}
  if [ -n "$OUT" ]; then
    output="$OUT/$stem.kumo.bmp"
  else
    output="$(dirname "$input")/$stem.kumo.bmp"
  fi
  "${CONVERT[@]}" "$input" \
    -auto-orient \
    -resize 256x128 \
    -background black \
    -gravity center \
    -extent 256x128 \
    -type TrueColor \
    BMP3:"$output"
  size=$(wc -c <"$output" | tr -d ' ')
  echo "$output ($size bytes, 256x128 24-bit BMP)"
done
