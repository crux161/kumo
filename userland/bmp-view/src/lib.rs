#![no_std]
//j648
//j649

//! Decode a BMP into the existing software presentation surface.

use embedded_graphics::{image::GetPixel, pixelcolor::Rgb888, prelude::*};
use kumo_graphics::{VIEW_HEIGHT, VIEW_WIDTH};
use tinybmp::{Bmp, ParseError};

pub const SAMPLE: &[u8] = include_bytes!("../assets/sample.bmp");
pub const ROW_BYTES: usize = VIEW_WIDTH as usize * 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidBitmap,
    UnsupportedCompression,
    MissingPixel,
    InvalidRow,
}

/// Bounded file intake; large enough for a 1024x1024 32-bit BMP plus its header.
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
pub const FILE_HEADER_LEN: usize = 16;
pub const PRESENTER_TAG: &[u8] = b"KBP1";

/// A zero length selects the built-in sample and carries no file handle.
pub fn file_header(len: u64) -> Option<[u8; FILE_HEADER_LEN]> {
    if len > MAX_FILE_BYTES {
        return None;
    }
    let mut bytes = [0; FILE_HEADER_LEN];
    bytes[..4].copy_from_slice(b"KBF1");
    bytes[4..12].copy_from_slice(&len.to_le_bytes());
    Some(bytes)
}

pub fn file_length(bytes: &[u8]) -> Option<u64> {
    if bytes.len() != FILE_HEADER_LEN {
        return None;
    }
    let len = u64::from_le_bytes(bytes[4..12].try_into().ok()?);
    (file_header(len)?.as_slice() == bytes).then_some(len)
}

/// Copy exactly one nonempty bounded file, tolerating short reads but never early EOF.
pub fn copy_file(
    len: u64,
    mut read: impl FnMut(u64, &mut [u8]) -> Option<usize>,
    mut write: impl FnMut(u64, &[u8]) -> bool,
) -> bool {
    if len == 0 || len > MAX_FILE_BYTES {
        return false;
    }
    let mut offset = 0;
    let mut bytes = [0; 256];
    while offset < len {
        let wanted = (len - offset).min(bytes.len() as u64) as usize;
        let Some(n) = read(offset, &mut bytes[..wanted]) else {
            return false;
        };
        if n == 0 || n > wanted || !write(offset, &bytes[..n]) {
            return false;
        }
        offset += n as u64;
    }
    true
}

enum Image<'a> {
    Tiny(Bmp<'a, Rgb888>),
    Bgra {
        pixels: &'a [u8],
        size: Size,
        top_down: bool,
    },
}

impl<'a> Image<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        match Bmp::<Rgb888>::from_slice(bytes) {
            Ok(bmp) => {
                if bmp.as_raw().header().compression_method.is_compressed() {
                    return Err(Error::UnsupportedCompression);
                }
                return Ok(Self::Tiny(bmp));
            }
            Err(ParseError::UnsupportedChannelMasks) => {}
            Err(_) => return Err(Error::InvalidBitmap),
        }
        // tinybmp refuses a nonzero alpha mask. The presenter is opaque, so accept only
        // standard BGRA masks and discard alpha, without modifying the file. — SAKER
        let word = |offset: usize| -> Option<u32> {
            Some(u32::from_le_bytes(
                bytes.get(offset..offset + 4)?.try_into().ok()?,
            ))
        };
        let parse = || -> Option<Self> {
            let dib = word(14)? as usize;
            if bytes.get(..2)? != b"BM"
                || ![56, 108, 124].contains(&dib)
                || bytes.get(26..30)? != [1, 0, 32, 0]
                || word(30)? != 3
                || [word(54)?, word(58)?, word(62)?, word(66)?]
                    != [0xFF0000, 0xFF00, 0xFF, 0xFF000000]
            {
                return None;
            }
            let width = word(18)? as i32;
            let height = word(22)? as i32;
            if width <= 0 || height == 0 {
                return None;
            }
            let start = word(10)? as usize;
            if start < 14 + dib {
                return None;
            }
            let len = u64::from(width as u32)
                .checked_mul(u64::from(height.unsigned_abs()))?
                .checked_mul(4)?;
            let end = start.checked_add(usize::try_from(len).ok()?)?;
            Some(Self::Bgra {
                pixels: bytes.get(start..end)?,
                size: Size::new(width as u32, height.unsigned_abs()),
                top_down: height < 0,
            })
        };
        parse().ok_or(Error::InvalidBitmap)
    }

    fn size(&self) -> Size {
        match self {
            Self::Tiny(bmp) => bmp.size(),
            Self::Bgra { size, .. } => *size,
        }
    }

    fn pixel(&self, point: Point) -> Option<Rgb888> {
        match self {
            Self::Tiny(bmp) => bmp.pixel(point),
            Self::Bgra {
                pixels,
                size,
                top_down,
            } => {
                let x = u32::try_from(point.x).ok()?;
                let mut y = u32::try_from(point.y).ok()?;
                if x >= size.width || y >= size.height {
                    return None;
                }
                if !top_down {
                    y = size.height - 1 - y;
                }
                let offset = (u64::from(y) * u64::from(size.width) + u64::from(x)) * 4;
                let offset = usize::try_from(offset).ok()?;
                let pixel = pixels.get(offset..offset + 4)?;
                Some(Rgb888::new(pixel[2], pixel[1], pixel[0]))
            }
        }
    }
}

pub struct Bitmap<'a> {
    bmp: Image<'a>,
    width: u32,
    height: u32,
    left: u32,
    top: u32,
}

impl<'a> Bitmap<'a> {
    /// Fit inside the view, preserving aspect ratio and leaving black letterboxing.
    pub fn new(bytes: &'a [u8]) -> Result<Self, Error> {
        let bmp = Image::parse(bytes)?;
        let size = bmp.size();
        let (width, height) = if u64::from(size.width) * u64::from(VIEW_HEIGHT)
            > u64::from(size.height) * u64::from(VIEW_WIDTH)
        {
            (
                VIEW_WIDTH,
                (u64::from(size.height) * u64::from(VIEW_WIDTH) / u64::from(size.width)).max(1)
                    as u32,
            )
        } else {
            (
                (u64::from(size.width) * u64::from(VIEW_HEIGHT) / u64::from(size.height)).max(1)
                    as u32,
                VIEW_HEIGHT,
            )
        };
        Ok(Self {
            bmp,
            width,
            height,
            left: (VIEW_WIDTH - width) / 2,
            top: (VIEW_HEIGHT - height) / 2,
        })
    }

    /// One row of little-endian B/G/R/ignored pixels, with no heap allocation.
    pub fn row(&self, y: u32, row: &mut [u8; ROW_BYTES]) -> Result<(), Error> {
        if y >= VIEW_HEIGHT {
            return Err(Error::InvalidRow);
        }
        row.fill(0);
        if y < self.top || y >= self.top + self.height {
            return Ok(());
        }
        let size = self.bmp.size();
        let sy = u64::from(y - self.top) * u64::from(size.height) / u64::from(self.height);
        for x in 0..self.width {
            let sx = u64::from(x) * u64::from(size.width) / u64::from(self.width);
            let color = self
                .bmp
                .pixel(Point::new(sx as i32, sy as i32))
                .ok_or(Error::MissingPixel)?;
            let offset = (self.left + x) as usize * 4;
            row[offset..offset + 4].copy_from_slice(&[color.b(), color.g(), color.r(), 0]);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_intake_rejects_oversize_truncation_and_bad_metadata() {
        for len in [0, 1, MAX_FILE_BYTES] {
            assert_eq!(file_length(&file_header(len).unwrap()), Some(len));
        }
        assert!(file_header(MAX_FILE_BYTES + 1).is_none());
        let mut bytes = file_header(42).unwrap();
        bytes[15] = 1;
        assert_eq!(file_length(&bytes), None);
        assert_eq!(file_length(&bytes[..15]), None);
        for len in [0, MAX_FILE_BYTES + 1] {
            assert!(!copy_file(
                len,
                |_, _| panic!("must reject before reading"),
                |_, _| false
            ));
        }
        assert!(!copy_file(10, |_, _| Some(0), |_, _| true));
        assert!(!copy_file(10, |_, _| Some(11), |_, _| true));
        assert!(!copy_file(10, |_, _| Some(10), |_, _| false));
        let mut copied = [0; 600];
        assert!(copy_file(
            600,
            |offset, buf| {
                let n = buf.len().min(17);
                for (i, byte) in buf[..n].iter_mut().enumerate() {
                    *byte = (offset as usize + i) as u8;
                }
                Some(n)
            },
            |offset, buf| {
                let start = offset as usize;
                copied[start..start + buf.len()].copy_from_slice(buf);
                true
            }
        ));
        assert!(copied.iter().enumerate().all(|(i, b)| *b == i as u8));
    }

    // A 3x2 bottom-up 24-bit BMP, with three padding bytes per source row.
    fn fixture() -> [u8; 78] {
        let mut bytes = [0u8; 78];
        bytes[..2].copy_from_slice(b"BM");
        bytes[2..6].copy_from_slice(&78u32.to_le_bytes());
        bytes[10..14].copy_from_slice(&54u32.to_le_bytes());
        bytes[14..18].copy_from_slice(&40u32.to_le_bytes());
        bytes[18..22].copy_from_slice(&3i32.to_le_bytes());
        bytes[22..26].copy_from_slice(&2i32.to_le_bytes());
        bytes[26..28].copy_from_slice(&1u16.to_le_bytes());
        bytes[28..30].copy_from_slice(&24u16.to_le_bytes());
        bytes[54..66].copy_from_slice(&[255, 255, 255, 0, 0, 0, 0, 255, 255, 99, 99, 99]);
        bytes[66..78].copy_from_slice(&[0, 0, 255, 0, 255, 0, 255, 0, 0, 99, 99, 99]);
        bytes
    }

    #[test]
    fn scales_centers_and_decodes_bgr_bottom_up_with_padded_rows() {
        let bytes = fixture();
        let image = Bitmap::new(&bytes).unwrap();
        assert_eq!(
            (image.width, image.height, image.left, image.top),
            (192, 128, 32, 0)
        );
        let mut row = [0; ROW_BYTES];
        image.row(0, &mut row).unwrap();
        assert!(row[..32 * 4].iter().all(|b| *b == 0));
        assert_eq!(&row[32 * 4..33 * 4], &[0, 0, 255, 0]);
        assert_eq!(&row[96 * 4..97 * 4], &[0, 255, 0, 0]);
        assert_eq!(&row[160 * 4..161 * 4], &[255, 0, 0, 0]);
        image.row(127, &mut row).unwrap();
        assert_eq!(&row[32 * 4..33 * 4], &[255, 255, 255, 0]);
        assert_eq!(&row[160 * 4..161 * 4], &[0, 255, 255, 0]);
        assert!(row[224 * 4..].iter().all(|b| *b == 0));
        assert_eq!(image.row(VIEW_HEIGHT, &mut row), Err(Error::InvalidRow));
    }

    #[test]
    fn top_down_and_wide_images_have_correct_orientation_and_letterboxing() {
        let mut bytes = fixture();
        bytes[22..26].copy_from_slice(&(-2i32).to_le_bytes());
        let image = Bitmap::new(&bytes).unwrap();
        let mut row = [0; ROW_BYTES];
        image.row(0, &mut row).unwrap();
        assert_eq!(&row[32 * 4..33 * 4], &[255, 255, 255, 0]);
        bytes[18..22].copy_from_slice(&6i32.to_le_bytes());
        bytes[22..26].copy_from_slice(&1i32.to_le_bytes());
        let image = Bitmap::new(&bytes).unwrap();
        assert_eq!(
            (image.width, image.height, image.left, image.top),
            (256, 42, 0, 43)
        );
        image.row(42, &mut row).unwrap();
        assert!(row.iter().all(|b| *b == 0));
        image.row(85, &mut row).unwrap();
        assert!(row.iter().all(|b| *b == 0));
    }

    #[test]
    fn standard_bgra_masks_are_opaque_and_truncation_or_other_masks_refuse() {
        let mut bytes = [0u8; 78];
        bytes[..2].copy_from_slice(b"BM");
        bytes[10..14].copy_from_slice(&70u32.to_le_bytes());
        bytes[14..18].copy_from_slice(&56u32.to_le_bytes());
        bytes[18..22].copy_from_slice(&2i32.to_le_bytes());
        bytes[22..26].copy_from_slice(&1i32.to_le_bytes());
        bytes[26..30].copy_from_slice(&[1, 0, 32, 0]);
        bytes[30..34].copy_from_slice(&3u32.to_le_bytes());
        bytes[34..38].copy_from_slice(&8u32.to_le_bytes());
        for (index, mask) in [0xFF0000u32, 0xFF00, 0xFF, 0xFF000000].iter().enumerate() {
            bytes[54 + index * 4..58 + index * 4].copy_from_slice(&mask.to_le_bytes());
        }
        bytes[70..].copy_from_slice(&[3, 2, 1, 0, 6, 5, 4, 255]);
        let image = Bitmap::new(&bytes).unwrap();
        let mut row = [0; ROW_BYTES];
        image.row(0, &mut row).unwrap();
        assert_eq!(&row[..4], &[3, 2, 1, 0]);
        assert_eq!(&row[128 * 4..129 * 4], &[6, 5, 4, 0]);
        assert!(matches!(
            Bitmap::new(&bytes[..77]),
            Err(Error::InvalidBitmap)
        ));
        bytes[66..70].copy_from_slice(&0xF0000000u32.to_le_bytes());
        assert!(matches!(Bitmap::new(&bytes), Err(Error::InvalidBitmap)));
    }

    #[test]
    fn bundled_bitmap_renders_every_row_and_bad_input_is_rejected() {
        let image = Bitmap::new(SAMPLE).unwrap();
        assert_eq!(
            (image.width, image.height, image.left, image.top),
            (96, 128, 80, 0)
        );
        let mut row = [0; ROW_BYTES];
        for y in 0..VIEW_HEIGHT {
            image.row(y, &mut row).unwrap();
            assert!(row[..80 * 4].iter().all(|b| *b == 0));
            assert!(row[176 * 4..].iter().all(|b| *b == 0));
        }
        assert!(matches!(
            Bitmap::new(b"\x89PNG\r\n\x1a\n"),
            Err(Error::InvalidBitmap)
        ));
        assert!(matches!(
            Bitmap::new(&SAMPLE[..SAMPLE.len() - 1]),
            Err(Error::InvalidBitmap)
        ));
        let mut bytes = fixture();
        bytes[18..22].fill(0);
        assert!(matches!(Bitmap::new(&bytes), Err(Error::InvalidBitmap)));
    }
}
