#![no_std]
//j648

//! Decode a bundled BMP into the existing software presentation surface.

use embedded_graphics::{image::GetPixel, pixelcolor::Rgb888, prelude::*};
use kumo_graphics::{VIEW_HEIGHT, VIEW_WIDTH};
use tinybmp::{Bmp, CompressionMethod};

pub const SAMPLE: &[u8] = include_bytes!("../assets/sample.bmp");
pub const ROW_BYTES: usize = VIEW_WIDTH as usize * 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidBitmap,
    UnsupportedCompression,
    MissingPixel,
    InvalidRow,
}

pub struct Bitmap<'a> {
    bmp: Bmp<'a, Rgb888>,
    width: u32,
    height: u32,
    left: u32,
    top: u32,
}

impl<'a> Bitmap<'a> {
    /// Fit inside the view, preserving aspect ratio and leaving black letterboxing.
    pub fn new(bytes: &'a [u8]) -> Result<Self, Error> {
        let bmp = Bmp::<Rgb888>::from_slice(bytes).map_err(|_| Error::InvalidBitmap)?;
        if !matches!(
            bmp.as_raw().header().compression_method,
            CompressionMethod::Rgb | CompressionMethod::Bitfields
        ) {
            return Err(Error::UnsupportedCompression);
        }
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
