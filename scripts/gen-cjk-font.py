#!/usr/bin/env python3
"""Generate KUMO's Stage-A CJK console glyph table from GNU Unifont.

This is an *offline* developer tool. Its outputs — `cjk_font.bin` (the packed glyphs) and
`cjk_font.rs` (a thin `include_bytes!` wrapper) under `hal/kumo-hal-aarch64/src/` — are
checked in, so a normal `cargo build` needs no font, no Unihan, and no network. Re-run this
only to change the coverage.

Format (DESIGN/005): glyphs are packed as fixed 34-byte records, **sorted ascending by
codepoint** for binary search — `u16` little-endian codepoint, then a 16x16 1bpp bitmap
(16 rows x 2 bytes, MSB = leftmost pixel, set bit = foreground). A codepoint absent from
the table renders as the hardcoded tofu box in the HAL; this tool never decides the fallback.

Coverage (`--coverage`):
  * `broad` (default): "newspaper literacy" — Jōyō kanji (Japanese) ∪ GB2312 Level-1 (common
    simplified Chinese) ∪ Hangul compatibility jamo. The Han sets come from the Unicode
    Unihan database (auto-downloaded + cached); jamo is a fixed block. ~4.6k glyphs, ~150 KiB.
    Everything else (full CJK, traditional Han, precomposed Hangul syllables) is deferred to
    the later BDF/GPU stage.
  * `curated`: the tiny VEIL-name + diagnostics allowlist (`GLYPHS`). ~33 glyphs.

Glyph bitmaps always come from GNU Unifont's `.hex` (canonical 16x16); a `magick` fallback
(ImageMagick rasterizing a vector font) is retained for `curated` where no `.hex` is present.

Usage:
    python3 scripts/gen-cjk-font.py                      # broad, Unifont
    python3 scripts/gen-cjk-font.py --coverage curated   # tiny allowlist, Unifont
    python3 scripts/gen-cjk-font.py --coverage curated --source magick
    UNIFONT_HEX=/path/unifont.hex UNIHAN_ZIP=/path/Unihan.zip python3 scripts/gen-cjk-font.py
"""

import argparse
import glob
import os
import subprocess
import sys
import urllib.request
import zipfile
from pathlib import Path

# Curated allowlist (VEIL names + diagnostics), used only with `--coverage curated`.
GLYPHS = (
    "雲紫微虹空座后土勾陈太一南极四御起動記憶時計検査正常異常警告錯誤完了"
)

# Hangul: the modern Compatibility Jamo block (the 24 basic letters + compound jamo).
HANGUL_JAMO = range(0x3131, 0x3164)

UNIHAN_URL = "https://www.unicode.org/Public/UCD/latest/ucd/Unihan.zip"
ROOT = Path(__file__).resolve().parent.parent
CACHE = Path(__file__).resolve().parent / ".cache"
BIN_OUT = ROOT / "hal/kumo-hal-aarch64/src/cjk_font.bin"
RS_OUT = ROOT / "hal/kumo-hal-aarch64/src/cjk_font.rs"
RECORD = 34  # 2-byte codepoint + 32-byte bitmap

MAGICK_FONT = "/System/Library/Fonts/Hiragino Sans GB.ttc"
MAGICK_POINTSIZE = 16


# ---- glyph bitmaps ---------------------------------------------------------------------

def find_unifont_hex() -> str:
    env = os.environ.get("UNIFONT_HEX")
    if env:
        return env
    patterns = [
        "/opt/homebrew/Caskroom/font-gnu-unifont/*/unifont-*/font/precompiled/unifont-*.hex",
        "/usr/share/unifont/unifont.hex",
        "/usr/share/fonts/unifont/unifont.hex",
        "/opt/homebrew/share/unifont/unifont.hex",
    ]
    for pattern in patterns:
        hits = sorted(g for g in glob.glob(pattern) if "sample" not in g)
        if hits:
            return hits[-1]
    raise SystemExit("could not find Unifont .hex; set UNIFONT_HEX=/path/to/unifont.hex")


def load_unifont(hex_path: str) -> dict[int, list[int]]:
    table = {}
    with open(hex_path, encoding="ascii") as handle:
        for line in handle:
            line = line.strip()
            if not line or ":" not in line:
                continue
            code, bits = line.split(":", 1)
            data = [int(bits[i:i + 2], 16) for i in range(0, len(bits), 2)]
            if len(data) == 16:  # narrow 8x16 -> left-align into the 16-wide cell
                data = [b for byte in data for b in (byte, 0x00)]
            table[int(code, 16)] = data
    return table


def render_magick(cp: int) -> list[int]:
    pbm = subprocess.run(
        ["magick", "-size", "16x16", "xc:black", "-font", MAGICK_FONT, "-fill", "white",
         "-pointsize", str(MAGICK_POINTSIZE), "-gravity", "center", "-annotate", "+0+0",
         chr(cp), "-threshold", "50%", "-depth", "1", "pbm:-"],
        capture_output=True, check=True).stdout
    body_start = pbm.index(b"\n", pbm.index(b"\n") + 1) + 1
    return [(~b) & 0xFF for b in pbm[body_start:body_start + 32]]


# ---- coverage selection (Unihan) -------------------------------------------------------

def unihan_zip() -> Path:
    env = os.environ.get("UNIHAN_ZIP")
    if env:
        return Path(env)
    CACHE.mkdir(exist_ok=True)
    dst = CACHE / "Unihan.zip"
    if not dst.exists() or dst.stat().st_size == 0:
        print(f"downloading {UNIHAN_URL} -> {dst}", file=sys.stderr)
        urllib.request.urlretrieve(UNIHAN_URL, dst)
    return dst


def unihan_field(zf: zipfile.ZipFile, member: str, key: str) -> dict[int, str]:
    out = {}
    for line in zf.read(member).decode("utf-8").splitlines():
        if line.startswith("#") or not line.strip():
            continue
        cp, k, val = line.split("\t", 2)
        if k == key:
            out[int(cp[2:], 16)] = val
    return out


def broad_codepoints() -> set[int]:
    """Jōyō kanji ∪ GB2312 Level-1 (ku 16–55) ∪ Hangul compatibility jamo."""
    with zipfile.ZipFile(unihan_zip()) as zf:
        joyo = set(unihan_field(zf, "Unihan_OtherMappings.txt", "kJoyoKanji"))
        gb0 = unihan_field(zf, "Unihan_OtherMappings.txt", "kGB0")
    gb_l1 = {cp for cp, v in gb0.items() if 16 <= int(v) // 100 <= 55}
    return joyo | gb_l1 | set(HANGUL_JAMO)


# ---- emit ------------------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--coverage", choices=("broad", "curated"), default="broad")
    parser.add_argument("--source", choices=("unifont", "magick"), default="unifont")
    args = parser.parse_args()

    if args.coverage == "broad" and args.source == "magick":
        raise SystemExit("broad coverage requires --source unifont")

    if args.source == "unifont":
        unifont = load_unifont(find_unifont_hex())
        render = lambda cp: unifont.get(cp)  # noqa: E731
        font_note = "GNU Unifont"
    else:
        render = lambda cp: render_magick(cp)  # noqa: E731
        font_note = f"{Path(MAGICK_FONT).name} {MAGICK_POINTSIZE}px (ImageMagick)"

    if args.coverage == "broad":
        codepoints = sorted(cp for cp in broad_codepoints() if cp <= 0xFFFF)
        coverage_note = ("newspaper literacy: Jōyō kanji ∪ GB2312 Level-1 (common simplified "
                         "Chinese) ∪ Hangul jamo")
    else:
        codepoints = sorted({ord(ch) for ch in GLYPHS if ord(ch) <= 0xFFFF})
        coverage_note = "curated VEIL names + diagnostics"

    records = []
    skipped = 0
    for cp in codepoints:
        rows = render(cp)
        if rows is None or len(rows) != 32:
            skipped += 1
            continue
        records.append((cp, bytes(rows)))
    records.sort()

    blob = bytearray()
    for cp, rows in records:
        blob += cp.to_bytes(2, "little") + rows
    BIN_OUT.write_bytes(blob)

    rs = f"""// @generated by scripts/gen-cjk-font.py — do not edit by hand.
//
//! CJK console glyphs for the Stage-A framebuffer (DESIGN/005), {coverage_note}.
//! {len(records)} glyphs from {font_note}, packed as fixed {RECORD}-byte records sorted
//! ascending by codepoint (binary-searchable): a `u16` little-endian codepoint, then a
//! 16x16 1bpp bitmap (16 rows x 2 bytes, MSB = leftmost pixel, set bit = foreground).
//! Codepoints absent here render as the hardcoded tofu box. Beyond this set (full CJK,
//! traditional Han, precomposed Hangul syllables) waits for the BDF/GPU font stage.
//! Regenerate: `python3 scripts/gen-cjk-font.py`.

/// Packed glyph records, sorted ascending by codepoint. See [`RECORD`].
pub static CJK_FONT: &[u8] = include_bytes!("cjk_font.bin");

/// Bytes per record: 2 (`u16` LE codepoint) + 32 (16x16 bitmap).
pub const RECORD: usize = {RECORD};

/// Number of glyph records in [`CJK_FONT`].
pub const GLYPH_COUNT: usize = {len(records)};
"""
    RS_OUT.write_text(rs)
    note = f" ({skipped} skipped: absent/narrow)" if skipped else ""
    print(f"wrote {len(records)} glyphs ({len(blob)} bytes) from {font_note}{note}")
    print(f"  -> {BIN_OUT}\n  -> {RS_OUT}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
