#![no_std]
//j567
//j568

//! First software presentation contract (DESIGN/019). One channel grants one surface backed
//! by one ordinary VMO. The presenter chooses placement; clients never name framebuffer
//! addresses or screen coordinates. Only one request may be outstanding. Stop writing before
//! Present, and reuse the buffer only after its matching reply. A reply means copied, not vblank.

pub const SURFACE_BYTES: usize = 256 * 1024;
pub const REQUEST_LEN: usize = 24;
pub const REPLY_LEN: usize = 12;
/// Linear little-endian pixels: B, G, R, ignored. No alpha, tiling, or compression.
pub const XRGB8888: u32 = 1;

/// Server-owned presentation layout. The presenter places these rectangles; clients never
/// name coordinates or screen addresses. The text console owns the region below the band,
/// and the presenter and Sora compute the console grid from this one function, so the
/// shell's winsize always matches the pixels the console actually owns.
pub const VIEW_COUNT: usize = 2;
pub const VIEW_WIDTH: u32 = 256;
pub const VIEW_HEIGHT: u32 = 128;
/// Server-drawn border around each view; client pixels never reach it.
pub const VIEW_CHROME: u32 = 2;
pub const VIEW_GAP: u32 = 16;
pub const BAND_MARGIN: u32 = 8;
/// A whole number of 8x16 text rows, so the console starts on a cell row.
pub const BAND_HEIGHT: u32 = 160;
pub const CONSOLE_CELL_W: u32 = 8;
pub const CONSOLE_CELL_H: u32 = 16;
pub const MIN_CONSOLE_ROWS: u32 = 8;
pub const VIEW_BG: u32 = 0x0000_0000;
pub const BAND_BG: u32 = 0x0016_1b22;
pub const VIEW_CHROME_COLOR: u32 = 0x005a_8fc8;
pub const VIEW_SOLID: u32 = 0x0000_a0ff;
pub const CLIP_POISON: u32 = 0x00ff_00ff;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct View {
    /// Left edge of the client-visible rectangle, in framebuffer pixels.
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Band {
    pub height: u32,
    pub views: [View; VIEW_COUNT],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConsoleRegion {
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Place the two views in a centered band, or decline when the framebuffer cannot also hold
/// a usable text console. Declining leaves the whole display to the console.
pub fn band(width: u32, height: u32) -> Option<Band> {
    let content =
        (VIEW_WIDTH + 2 * VIEW_CHROME) * VIEW_COUNT as u32 + VIEW_GAP * (VIEW_COUNT as u32 - 1);
    if width < content + 2 * BAND_MARGIN || height < BAND_HEIGHT + MIN_CONSOLE_ROWS * CONSOLE_CELL_H
    {
        return None;
    }
    let left = (width - content) / 2;
    let top = (BAND_HEIGHT - (VIEW_HEIGHT + 2 * VIEW_CHROME)) / 2;
    let mut views = [View {
        x: 0,
        y: 0,
        width: VIEW_WIDTH,
        height: VIEW_HEIGHT,
    }; VIEW_COUNT];
    for (index, view) in views.iter_mut().enumerate() {
        view.x = left + index as u32 * (VIEW_WIDTH + 2 * VIEW_CHROME + VIEW_GAP) + VIEW_CHROME;
        view.y = top + VIEW_CHROME;
    }
    Some(Band {
        height: BAND_HEIGHT,
        views,
    })
}

/// The text console's region: below the band when views exist, the whole display otherwise.
pub fn console_region(width: u32, height: u32) -> ConsoleRegion {
    match band(width, height) {
        Some(band) => ConsoleRegion {
            y: band.height,
            width,
            height: height - band.height,
        },
        None => ConsoleRegion {
            y: 0,
            width,
            height,
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Surface {
    pub width: u32,
    pub height: u32,
    /// Pixels per row, including padding.
    pub stride: u32,
    pub format: u32,
}

impl Surface {
    /// Validate all address arithmetic before a presenter reads any pixel. The whole final
    /// padded row must exist too, so clients and backends share a single extent rule.
    pub fn byte_len(self) -> Option<usize> {
        if self.width == 0
            || self.height == 0
            || self.stride < self.width
            || self.format != XRGB8888
        {
            return None;
        }
        let bytes = u64::from(self.stride)
            .checked_mul(u64::from(self.height))?
            .checked_mul(4)?;
        usize::try_from(bytes)
            .ok()
            .filter(|bytes| *bytes <= SURFACE_BYTES)
    }

    pub fn fits(self, backing_bytes: usize) -> bool {
        self.byte_len().is_some_and(|bytes| bytes <= backing_bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Present {
    pub sequence: u32,
    pub surface: Surface,
}

impl Present {
    pub fn encode(self) -> [u8; REQUEST_LEN] {
        let mut out = [0; REQUEST_LEN];
        out[..4].copy_from_slice(b"KGP1");
        for (chunk, word) in out[4..].chunks_exact_mut(4).zip([
            self.sequence,
            self.surface.width,
            self.surface.height,
            self.surface.stride,
            self.surface.format,
        ]) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != REQUEST_LEN || &bytes[..4] != b"KGP1" {
            return None;
        }
        Some(Self {
            sequence: word(bytes, 4),
            surface: Surface {
                width: word(bytes, 8),
                height: word(bytes, 12),
                stride: word(bytes, 16),
                format: word(bytes, 20),
            },
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Status {
    Copied = 0,
    BadRequest = 1,
    BadSurface = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reply {
    pub sequence: u32,
    pub status: Status,
}

impl Reply {
    pub fn encode(self) -> [u8; REPLY_LEN] {
        let mut out = [0; REPLY_LEN];
        out[..4].copy_from_slice(b"KGR1");
        out[4..8].copy_from_slice(&self.sequence.to_le_bytes());
        out[8..].copy_from_slice(&(self.status as u32).to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != REPLY_LEN || &bytes[..4] != b"KGR1" {
            return None;
        }
        Some(Self {
            sequence: word(bytes, 4),
            status: match word(bytes, 8) {
                0 => Status::Copied,
                1 => Status::BadRequest,
                2 => Status::BadSurface,
                _ => return None,
            },
        })
    }
}

fn word(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

pub const DEMO: Surface = Surface {
    width: 256,
    height: 128,
    stride: 256,
    format: XRGB8888,
};

/// Opaque RGB bars above a gradient and checkerboard. Useful for catching swapped channels,
/// incorrect pitch, and text-only output without shipping an asset or a font dependency.
pub fn demo_pixel(x: u32, y: u32) -> u32 {
    if y < 32 {
        [0xff0000, 0x00ff00, 0x0000ff, 0xffffff][(x / 64).min(3) as usize]
    } else {
        let blue = if ((x / 16) + (y / 16)).is_multiple_of(2) {
            0x40
        } else {
            0xc0
        };
        ((x & 255) << 16) | (((y * 2) & 255) << 8) | blue
    }
}

/// A deliberately over-large demo surface. Its visible 256x128 corner is a solid colour;
/// every pixel outside the view is poison that must never reach the glass.
pub const CLIP_DEMO: Surface = Surface {
    width: 320,
    height: 160,
    stride: 320,
    format: XRGB8888,
};

pub fn clip_demo_pixel(x: u32, y: u32) -> u32 {
    if x < VIEW_WIDTH && y < VIEW_HEIGHT {
        VIEW_SOLID
    } else {
        CLIP_POISON
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_untrusted_geometry_and_short_backing() {
        assert!(DEMO.fits(SURFACE_BYTES));
        assert!(CLIP_DEMO.fits(SURFACE_BYTES));
        assert!(!CLIP_DEMO.fits(CLIP_DEMO.byte_len().unwrap() - 1));
        for surface in [
            Surface { width: 0, ..DEMO },
            Surface { height: 0, ..DEMO },
            Surface {
                stride: 255,
                ..DEMO
            },
            Surface { format: 2, ..DEMO },
            Surface {
                width: u32::MAX,
                height: u32::MAX,
                stride: u32::MAX,
                ..DEMO
            },
        ] {
            assert!(!surface.fits(usize::MAX));
        }
        let padded = Surface {
            width: 3,
            height: 2,
            stride: 4,
            format: XRGB8888,
        };
        assert_eq!(padded.byte_len(), Some(32));
        assert!(!padded.fits(28));
    }

    #[test]
    fn wire_format_is_versioned_exact_and_little_endian() {
        let request = Present {
            sequence: 0x12345678,
            surface: DEMO,
        };
        let wire = request.encode();
        assert_eq!(&wire[..8], b"KGP1\x78\x56\x34\x12");
        assert_eq!(Present::decode(&wire), Some(request));
        for len in 0..REQUEST_LEN {
            assert_eq!(Present::decode(&wire[..len]), None);
        }
        assert_eq!(Present::decode(&[0; REQUEST_LEN + 1]), None);
        let mut invalid = wire;
        invalid[3] = b'2';
        assert_eq!(Present::decode(&invalid), None);
        for status in [Status::Copied, Status::BadRequest, Status::BadSurface] {
            let reply = Reply {
                sequence: 17,
                status,
            };
            assert_eq!(Reply::decode(&reply.encode()), Some(reply));
        }
        let mut reply = Reply {
            sequence: 17,
            status: Status::Copied,
        }
        .encode();
        reply[8] = 99;
        assert_eq!(Reply::decode(&reply), None);
    }

    #[test]
    fn band_places_two_views_above_the_console() {
        for (width, height) in [(800, 600), (640, 480), (1600, 900), (552, 288)] {
            let layout = band(width, height).expect("views fit");
            assert_eq!(layout.height, BAND_HEIGHT);
            assert_eq!(layout.views.len(), VIEW_COUNT);
            let console = console_region(width, height);
            assert_eq!(console.y, BAND_HEIGHT);
            assert_eq!(console.height, height - BAND_HEIGHT);
            assert!(console.height >= MIN_CONSOLE_ROWS * CONSOLE_CELL_H);
            for (index, view) in layout.views.iter().enumerate() {
                assert_eq!((view.width, view.height), (VIEW_WIDTH, VIEW_HEIGHT));
                assert!(view.x >= VIEW_CHROME && view.y >= VIEW_CHROME);
                assert!(view.x + view.width + VIEW_CHROME <= width);
                assert!(view.y + view.height + VIEW_CHROME <= BAND_HEIGHT);
                if index > 0 {
                    let previous = layout.views[index - 1];
                    assert_eq!(
                        view.x,
                        previous.x + previous.width + 2 * VIEW_CHROME + VIEW_GAP
                    );
                }
            }
        }
    }

    #[test]
    fn band_declines_when_the_console_would_not_fit() {
        assert_eq!(band(551, 600), None);
        assert_eq!(band(800, 287), None);
        assert_eq!(
            console_region(800, 287),
            ConsoleRegion {
                y: 0,
                width: 800,
                height: 287
            }
        );
        assert_eq!(console_region(551, 600).y, 0);
    }

    #[test]
    fn clip_demo_poisons_only_the_pixels_the_view_cannot_show() {
        assert!(CLIP_DEMO.fits(SURFACE_BYTES));
        assert_eq!(clip_demo_pixel(VIEW_WIDTH - 1, VIEW_HEIGHT - 1), VIEW_SOLID);
        assert_eq!(clip_demo_pixel(VIEW_WIDTH, VIEW_HEIGHT / 2), CLIP_POISON);
        assert_eq!(clip_demo_pixel(VIEW_WIDTH / 2, VIEW_HEIGHT), CLIP_POISON);
    }
}
