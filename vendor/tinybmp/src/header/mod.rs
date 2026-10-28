//! Bitmap header
//!
//! Information gleaned from [wikipedia](https://en.wikipedia.org/wiki/BMP_file_format) and
//! [this website](http://paulbourke.net/dataformats/bmp/)

use embedded_graphics::prelude::*;

use crate::{
    color_table::ColorTable,
    parser::{le_u16, le_u32, take2, take_slice},
    try_const, ParseError,
};

mod dib_header;

use dib_header::DibHeader;

/// Bits per pixel.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
#[non_exhaustive]
pub enum Bpp {
    /// 1 bit per pixel.
    Bits1,
    /// 4 bit per pixel.
    Bits4,
    /// 8 bits per pixel.
    Bits8,
    /// 16 bits per pixel.
    Bits16,
    /// 24 bits per pixel.
    Bits24,
    /// 32 bits per pixel.
    Bits32,
}

impl Bpp {
    const fn new(value: u16) -> Result<Self, ParseError> {
        Ok(match value {
            1 => Self::Bits1,
            4 => Self::Bits4,
            8 => Self::Bits8,
            16 => Self::Bits16,
            24 => Self::Bits24,
            32 => Self::Bits32,
            _ => return Err(ParseError::UnsupportedBpp(value)),
        })
    }

    const fn parse(input: &[u8]) -> Result<(&[u8], Self), ParseError> {
        let (input, value) = try_const!(le_u16(input));
        Ok((input, try_const!(Self::new(value))))
    }

    /// Returns the number of bits.
    pub const fn bits(self) -> u16 {
        match self {
            Self::Bits1 => 1,
            Self::Bits4 => 4,
            Self::Bits8 => 8,
            Self::Bits16 => 16,
            Self::Bits24 => 24,
            Self::Bits32 => 32,
        }
    }
}

/// Image row order.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
#[non_exhaustive]
pub enum RowOrder {
    /// Bottom-up (standard)
    BottomUp,
    /// Top-down
    TopDown,
}

impl Default for RowOrder {
    fn default() -> Self {
        Self::BottomUp
    }
}

/// BMP header information.
///
/// The header can be accessed by using [`RawBmp::header`](crate::RawBmp::header).
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
pub struct Header {
    /// Total file size in bytes.
    pub file_size: u32,

    /// Byte offset from beginning of file at which pixel data begins.
    pub image_data_start: usize,

    /// Image size in pixels.
    pub image_size: Size,

    /// Number of bits per pixel.
    pub bpp: Bpp,

    /// Length in bytes of the image data.
    pub image_data_len: u32,

    /// Bit masks for the color channels.
    pub channel_masks: Option<ChannelMasks>,

    /// Row order of the image data within the file
    pub row_order: RowOrder,

    /// The compression method
    pub compression_method: CompressionMethod,
}

impl Header {
    pub(crate) const fn parse(
        input: &[u8],
    ) -> Result<(&[u8], (Header, Option<ColorTable<'_>>)), ParseError> {
        // File header
        let (input, magic) = try_const!(take2(input));

        if !matches!(&magic, b"BM") {
            return Err(ParseError::InvalidFileSignature(magic));
        }

        let (input, file_size) = try_const!(le_u32(input));
        let (input, _reserved_1) = try_const!(le_u16(input));
        let (input, _reserved_2) = try_const!(le_u16(input));
        let (input, image_data_start) = try_const!(le_u32(input));

        // DIB header
        let (input, dib_header) = try_const!(DibHeader::parse(input));

        let (input, color_table) = if dib_header.color_table_num_entries > 0 {
            // Each color table entry is 4 bytes long
            let (input, table) = try_const!(take_slice(
                input,
                dib_header.color_table_num_entries as usize * 4
            ));
            (input, Some(ColorTable::new(table)))
        } else {
            (input, None)
        };

        Ok((
            input,
            (
                Header {
                    file_size,
                    image_data_start: image_data_start as usize,
                    image_size: dib_header.image_size,
                    image_data_len: dib_header.image_data_len,
                    bpp: dib_header.bpp,
                    channel_masks: dib_header.channel_masks,
                    row_order: dib_header.row_order,
                    compression_method: dib_header.compression,
                },
                color_table,
            ),
        ))
    }

    /// [`Self::bytes_per_row`] without checking the compression.
    ///
    /// Only returns useful values for uncompressed formats.
    pub(crate) const fn bytes_per_row_uncompressed(&self) -> usize {
        let bits_per_row = self.image_size.width as usize * self.bpp.bits() as usize;

        (bits_per_row + 31) / 32 * (32 / 8)
    }

    /// Calculates the row length in bytes for uncompressed BMP files.
    ///
    /// This value, which is also called the stride, is the advancement from one
    /// row to the next and may include padding bytes. It is determined by
    /// multiplying the width of the image with the number of bytes per pixel
    /// and rounding the result up to the next 4-byte boundary.
    ///
    /// Returns `None` for compressed formats where the row lengths vary.
    pub const fn bytes_per_row(&self) -> Option<usize> {
        if self.compression_method.is_compressed() {
            None
        } else {
            Some(self.bytes_per_row_uncompressed())
        }
    }
}

/// Bit masks for the color channels.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, Default)]
pub struct ChannelMasks {
    /// Red channel mask.
    pub red: u32,
    /// Green channel mask.
    pub green: u32,
    /// Blue channel mask.
    pub blue: u32,
    /// Alpha channel mask.
    pub alpha: u32,
}

impl ChannelMasks {
    /// Rgb555 color masks.
    pub const RGB555: Self = Self {
        red: 0b11111_00000_00000,
        green: 0b00000_11111_00000,
        blue: 0b00000_00000_11111,
        alpha: 0,
    };

    /// Rgb565 color masks.
    pub const RGB565: Self = Self {
        red: 0b11111_000000_00000,
        green: 0b00000_111111_00000,
        blue: 0b00000_000000_11111,
        alpha: 0,
    };

    /// Rgb888 color masks.
    pub const RGB888: Self = Self {
        red: 0xFF0000,
        green: 0x00FF00,
        blue: 0x0000FF,
        alpha: 0,
    };
}

/// Describes how the BMP file is compressed.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
pub enum CompressionMethod {
    /// The bitmap is in uncompressed RGB and doesn't use color masks
    Rgb,
    /// The bitmap is in uncompressed RGB, using color masks
    Bitfields,
    /// The bitmap is compressed using run-length encoding (RLE) compression,
    /// with 8 bits per pixel. The compression uses a 2-byte format consisting
    /// of a count byte followed by a byte containing a color index.
    Rle8,
    /// The bitmap is compressed using run-length encoding (RLE) compression,
    /// with 4 bits per pixel. The compression uses a 2-byte format consisting
    /// of a count byte followed by two word-length color indexes.
    Rle4,
}

impl CompressionMethod {
    const fn new(value: u32) -> Result<Self, ParseError> {
        Ok(match value {
            0 => Self::Rgb,
            1 => Self::Rle8,
            2 => Self::Rle4,
            3 => Self::Bitfields,
            _ => return Err(ParseError::UnsupportedCompressionMethod(value)),
        })
    }

    const fn parse(input: &[u8]) -> Result<(&[u8], Self), ParseError> {
        let (input, value) = try_const!(le_u32(input));
        Ok((input, try_const!(Self::new(value))))
    }

    /// Returns `true` if a BMP using this compression method is in fact
    /// compressed.
    pub const fn is_compressed(&self) -> bool {
        match self {
            Self::Rgb | Self::Bitfields => false,
            Self::Rle8 | Self::Rle4 => true,
        }
    }
}
