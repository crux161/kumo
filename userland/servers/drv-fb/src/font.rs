//j552
//j673
//j687

//! Fixed console bitmaps: ASCII and box drawing in one GLYPH_W x GLYPH_H cell; CJK in a
//! 16x16 bitmap within a double-width cell.

use super::{GLYPH_H, GLYPH_W};

const ASCII_FONT: &[u8] = include_bytes!("../font8x16.psf");
// Share the existing offline-generated data, not the HAL implementation. This
// adds only userspace rodata; neither the kernel nor BootInfo grows. — KESTREL
const CJK_FONT: &[u8] = include_bytes!("../../../../hal/kumo-hal-aarch64/src/cjk_font.bin");
const CJK_RECORD_BYTES: usize = 34;
const MISSING: [u8; 32] = [
    0, 0, 0, 0, 0x3f, 0xfc, 0x20, 4, 0x20, 4, 0x20, 4, 0x20, 4, 0x20, 4, 0x20, 4, 0x20, 4, 0x20, 4,
    0x20, 4, 0x20, 4, 0x3f, 0xfc, 0, 0, 0, 0,
];

pub(super) struct Glyph {
    pub columns: usize,
    /// Sixteen bits per scanline, with the leftmost pixel in bit 15.
    pub rows: [u16; GLYPH_H],
}

impl Glyph {
    fn narrow(rows: &[u8]) -> Self {
        Self {
            columns: 1,
            rows: core::array::from_fn(|y| u16::from(rows[y]) << 8),
        }
    }
}

fn font_field(offset: usize) -> usize {
    u32::from_le_bytes(ASCII_FONT[offset..offset + 4].try_into().unwrap()) as usize
}

pub(super) fn ascii_rows(ch: u8) -> &'static [u8] {
    let header_size = font_field(8);
    let charsize = font_field(20);
    let index = if (0x20..=0x7e).contains(&ch) {
        ch
    } else {
        b'?'
    } as usize;
    let start = header_size + index * charsize;
    &ASCII_FONT[start..start + charsize]
}

fn cjk_rows(scalar: u32) -> Option<&'static [u8]> {
    let codepoint = u16::try_from(scalar).ok()?;
    let (mut lo, mut hi) = (0, CJK_FONT.len() / CJK_RECORD_BYTES);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let offset = mid * CJK_RECORD_BYTES;
        let code = u16::from_le_bytes([CJK_FONT[offset], CJK_FONT[offset + 1]]);
        match code.cmp(&codepoint) {
            core::cmp::Ordering::Equal => {
                return Some(&CJK_FONT[offset + 2..offset + CJK_RECORD_BYTES]);
            }
            core::cmp::Ordering::Less => lo = mid + 1,
            core::cmp::Ordering::Greater => hi = mid,
        }
    }
    None
}

fn box_glyph(scalar: u32) -> Option<Glyph> {
    const UP: u8 = 1;
    const DOWN: u8 = 2;
    const LEFT: u8 = 4;
    const RIGHT: u8 = 8;
    let (arms, heavy) = match scalar {
        0x2500 => (LEFT | RIGHT, false),
        0x2501 => (LEFT | RIGHT, true),
        0x2502 => (UP | DOWN, false),
        0x2503 => (UP | DOWN, true),
        0x250c => (DOWN | RIGHT, false),
        0x250f => (DOWN | RIGHT, true),
        0x2510 => (DOWN | LEFT, false),
        0x2513 => (DOWN | LEFT, true),
        0x2514 => (UP | RIGHT, false),
        0x2517 => (UP | RIGHT, true),
        0x2518 => (UP | LEFT, false),
        0x251b => (UP | LEFT, true),
        0x251c => (UP | DOWN | RIGHT, false),
        0x2523 => (UP | DOWN | RIGHT, true),
        0x2524 => (UP | DOWN | LEFT, false),
        0x252b => (UP | DOWN | LEFT, true),
        0x252c => (DOWN | LEFT | RIGHT, false),
        0x2533 => (DOWN | LEFT | RIGHT, true),
        0x2534 => (UP | LEFT | RIGHT, false),
        0x253b => (UP | LEFT | RIGHT, true),
        0x253c => (UP | DOWN | LEFT | RIGHT, false),
        0x254b => (UP | DOWN | LEFT | RIGHT, true),
        _ => return None,
    };
    // Arms reach the cell edges so neighbouring cells join without seams. Rows are built
    // directly across the cell's GLYPH_W columns: each arm reaches past the middle so
    // adjacent cells overlap at the join, and heavy forms are two columns heavier and
    // two rows taller. — TIMBERDOODLE
    let (first, end) = if heavy {
        (GLYPH_H / 2 - 2, GLYPH_H / 2 + 2)
    } else {
        (GLYPH_H / 2 - 1, GLYPH_H / 2 + 1)
    };
    let (vertical_first, vertical_end) = if heavy {
        ((GLYPH_W - 4) / 2, (GLYPH_W - 4) / 2 + 4)
    } else {
        ((GLYPH_W - 2) / 2, (GLYPH_W - 2) / 2 + 2)
    };
    let mut rows = [0u16; GLYPH_H];
    for (y, row) in rows.iter_mut().enumerate() {
        for column in 0..GLYPH_W {
            let vertical = column >= vertical_first && column < vertical_end;
            let up = arms & UP != 0 && vertical && y < end;
            let down = arms & DOWN != 0 && vertical && y >= first;
            let horizontal = (first..end).contains(&y);
            let left = arms & LEFT != 0 && column <= GLYPH_W / 2;
            let right = arms & RIGHT != 0 && column + 1 >= GLYPH_W / 2;
            if up || down || (horizontal && (left || right)) {
                *row |= 0x8000 >> column;
            }
        }
    }
    Some(Glyph { columns: 1, rows })
}

/// ASCII glyphs now come from the vendored face at a scale whose advance is one cell (8 px),
/// thresholded into the same 8x16 mask the PSF source produced, so the console's geometry,
/// scroll, winsize and CJK path are untouched. Falls back to the PSF bitmap when the face
/// cannot be parsed. — TIMBERDOODLE
fn face_ascii(ch: u8) -> Option<Glyph> {
    use ab_glyph::{Font as _, FontRef, PxScale, ScaleFont as _};
    let font = FontRef::try_from_slice(super::FACE_TTF).ok()?;
    // Maple Mono's advance is 0.6 em, so one 8-px cell puts the scale at 8 / 0.6 pixels.
    let scale = PxScale::from(GLYPH_W as f32 / 0.6);
    let scaled = font.as_scaled(scale);
    // Keep the PSF source's tofu rule: a non-printable byte renders as '?'.
    let printable = if (0x20..=0x7e).contains(&ch) {
        ch
    } else {
        b'?'
    };
    let glyph_id = font.glyph_id(printable as char);
    let glyph = glyph_id.with_scale_and_position(scale, ab_glyph::point(0.0, scaled.ascent()));
    let outlined = font.outline_glyph(glyph)?;
    let bounds = outlined.px_bounds();
    let mut rows = [0u16; GLYPH_H];
    outlined.draw(|gx, gy, coverage| {
        if coverage < 0.5 {
            return;
        }
        let px = bounds.min.x + gx as f32;
        let py = bounds.min.y + gy as f32;
        if px < 0.0 || py < 0.0 {
            return;
        }
        let (px, py) = (px as usize, py as usize);
        if px >= GLYPH_W || py >= GLYPH_H {
            return;
        }
        rows[py] |= 0x8000 >> px;
    });
    Some(Glyph { columns: 1, rows })
}

/// Whether the console's ASCII source is the vendored face rather than the PSF bitmap.
pub(super) fn face_ascii_ready() -> bool {
    ab_glyph::FontRef::try_from_slice(super::FACE_TTF).is_ok()
}

pub(super) fn glyph(scalar: u32) -> Glyph {
    if scalar < 0x80 {
        return face_ascii(scalar as u8).unwrap_or_else(|| Glyph::narrow(ascii_rows(scalar as u8)));
    }
    if let Some(glyph) = box_glyph(scalar) {
        return glyph;
    }
    let rows = cjk_rows(scalar).unwrap_or(&MISSING);
    Glyph {
        columns: 2,
        // The bitmap is 16 rows tall in a GLYPH_H-row cell; blank-pad rather than index
        // past the record. — TIMBERDOODLE
        rows: core::array::from_fn(|y| match (rows.get(2 * y), rows.get(2 * y + 1)) {
            (Some(high), Some(low)) => u16::from_be_bytes([*high, *low]),
            _ => 0,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_font_is_sorted_and_covers_japanese() {
        assert_eq!(CJK_FONT.len(), 16121 * CJK_RECORD_BYTES);
        let code = |i: usize| {
            let offset = i * CJK_RECORD_BYTES;
            u16::from_le_bytes([CJK_FONT[offset], CJK_FONT[offset + 1]])
        };
        assert!((1..16121).all(|i| code(i - 1) < code(i)));
        for ch in "日本語あいうえおアイウエオ。「」：가힣".chars() {
            assert!(cjk_rows(ch as u32).is_some(), "missing {ch}");
        }
        assert!(cjk_rows(0x3400).is_none());
        assert!(cjk_rows(0x1f642).is_none());
        assert_eq!(glyph(0x1f642).columns, 2);
        assert!(glyph(0x1f642).rows.iter().any(|&bits| bits != 0));
    }

    #[test]
    fn ascii_glyphs_come_from_the_face_within_one_cell() {
        assert!(face_ascii_ready());
        let glyph = glyph(b'A' as u32);
        assert_eq!(glyph.columns, 1);
        assert!(glyph.rows.iter().any(|&bits| bits != 0), "no ink in A");
        // Every set bit is inside the GLYPH_W columns of one cell.
        let outside_cell = 0xffffu16 >> GLYPH_W;
        assert!(glyph.rows.iter().all(|&bits| bits & outside_cell == 0));
        // The face's outline differs from the PSF bitmap in at least one row.
        let psf = Glyph::narrow(ascii_rows(b'A'));
        assert!(glyph.rows.iter().zip(psf.rows.iter()).any(|(a, b)| a != b));
    }

    #[test]
    fn light_and_heavy_box_arms_join_at_cell_edges() {
        let left_edge = 0x8000u16;
        let right_edge = 0x8000u16 >> (GLYPH_W - 1);
        let full_row = (((1u32 << GLYPH_W) - 1) as u16) << (16 - GLYPH_W);
        for (left, horizontal, right, vertical, first, end) in
            [('┌', '─', '┐', '│', 9, 11), ('┏', '━', '┓', '┃', 8, 12)]
        {
            let l = glyph(left as u32);
            let h = glyph(horizontal as u32);
            let r = glyph(right as u32);
            let v = glyph(vertical as u32);
            assert_eq!((l.columns, h.columns, r.columns, v.columns), (1, 1, 1, 1));
            for y in first..end {
                assert_ne!(l.rows[y] & right_edge, 0);
                assert_eq!(h.rows[y], full_row);
                assert_ne!(r.rows[y] & left_edge, 0);
            }
            assert_eq!(l.rows[GLYPH_H - 1], v.rows[0]);
            assert_eq!(r.rows[GLYPH_H - 1], v.rows[0]);
            assert_eq!(l.rows[0], 0);
            assert_eq!(r.rows[0], 0);
        }
    }
}
