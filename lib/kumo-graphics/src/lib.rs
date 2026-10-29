#![no_std]
//j657
//j658
//j667
//j668
//j670

//! First software presentation contract (DESIGN/019). One channel grants one surface backed
//! by one ordinary VMO. The presenter chooses placement; clients never name framebuffer
//! addresses or screen coordinates. Only one request may be outstanding. Stop writing before
//! Present, and reuse the buffer only after its matching reply. A reply means copied, not vblank.

pub mod native;

pub const SURFACE_BYTES: usize = 256 * 1024;
/// `KGP2` frame: magic, sequence, surface (four words), damage count, four 16-byte rects.
pub const REQUEST_LEN: usize = 92;
pub const REPLY_LEN: usize = 12;
/// At most four damaged rectangles per present; zero rectangles means the whole surface.
pub const DAMAGE_MAX: usize = 4;
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
/// Horizontal overlap between adjacent server-owned views. The server chooses the order;
/// clients never see either value. — TIMBERDOODLE
pub const VIEW_OVERLAP: u32 = 64;
pub const BAND_MARGIN: u32 = 8;
/// A whole number of 8x16 text rows, so the console starts on a cell row.
pub const BAND_HEIGHT: u32 = 160;
pub const CONSOLE_CELL_W: u32 = 8;
pub const CONSOLE_CELL_H: u32 = 16;
pub const MIN_CONSOLE_ROWS: u32 = 8;
pub const VIEW_BG: u32 = 0x0000_0000;
pub const BAND_BG: u32 = 0x0016_1b22;
pub const VIEW_CHROME_COLOR: u32 = 0x005a_8fc8;
/// Chrome colour of the focused view; the server paints it so the keyboard owner is visible.
pub const VIEW_CHROME_FOCUS: u32 = 0x00f0_b040;
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
    /// Back-to-front view indices; later entries paint above earlier ones. — TIMBERDOODLE
    pub order: [usize; VIEW_COUNT],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConsoleRegion {
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Place the two overlapping views in a centered band, or decline when the framebuffer
/// cannot also hold a usable text console. Declining leaves the whole display to the console.
pub fn band(width: u32, height: u32) -> Option<Band> {
    let content =
        VIEW_WIDTH + (VIEW_COUNT as u32 - 1) * (VIEW_WIDTH - VIEW_OVERLAP) + 2 * VIEW_CHROME;
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
        view.x = left + VIEW_CHROME + index as u32 * (VIEW_WIDTH - VIEW_OVERLAP);
        view.y = top + VIEW_CHROME;
    }
    Some(Band {
        height: BAND_HEIGHT,
        views,
        order: core::array::from_fn(|index| index),
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

/// One client-declared damaged rectangle in surface coordinates. The presenter clips it to
/// the surface and then to its server-owned view; clients never name screen coordinates.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Damage {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Damage {
    /// Nonzero area and no u32 overflow in either extent.
    pub const fn valid(self) -> bool {
        self.width != 0
            && self.height != 0
            && matches!(self.x.checked_add(self.width), Some(_))
            && matches!(self.y.checked_add(self.height), Some(_))
    }

    /// Clip to the surface's pixel extent; `None` when nothing of the rectangle remains.
    pub fn clipped_to(self, surface: Surface) -> Option<Self> {
        let x0 = self.x.min(surface.width);
        let y0 = self.y.min(surface.height);
        let x1 = self.x.checked_add(self.width)?.min(surface.width);
        let y1 = self.y.checked_add(self.height)?.min(surface.height);
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        Some(Self {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        })
    }
}

/// A bounded, ordered set of damaged rectangles. An empty set means "the whole surface":
/// the first present on a view must be full, and a client that does not track damage keeps
/// sending the full set. — TIMBERDOODLE
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DamageSet {
    count: u32,
    rects: [Damage; DAMAGE_MAX],
}

impl DamageSet {
    pub const FULL: Self = Self {
        count: 0,
        rects: [Damage {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        }; DAMAGE_MAX],
    };

    pub fn single(rect: Damage) -> Option<Self> {
        if !rect.valid() {
            return None;
        }
        let mut set = Self::FULL;
        set.count = 1;
        set.rects[0] = rect;
        Some(set)
    }

    pub fn is_full(self) -> bool {
        self.count == 0
    }

    pub fn count(self) -> u32 {
        self.count
    }

    /// Validated sets only; decoding rejects everything else.
    pub fn rects(&self) -> &[Damage] {
        &self.rects[..(self.count as usize).min(DAMAGE_MAX)]
    }

    pub fn valid(self) -> bool {
        self.count as usize <= DAMAGE_MAX && self.rects().iter().all(|rect| rect.valid())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Present {
    pub sequence: u32,
    pub surface: Surface,
    pub damage: DamageSet,
}

impl Present {
    pub fn encode(self) -> [u8; REQUEST_LEN] {
        let mut out = [0; REQUEST_LEN];
        out[..4].copy_from_slice(b"KGP2");
        let words = [
            self.sequence,
            self.surface.width,
            self.surface.height,
            self.surface.stride,
            self.surface.format,
            self.damage.count,
        ];
        for (chunk, word) in out[4..28].chunks_exact_mut(4).zip(words) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        for (slot, rect) in out[28..].chunks_exact_mut(16).zip(self.damage.rects()) {
            for (chunk, word) in
                slot.chunks_exact_mut(4)
                    .zip([rect.x, rect.y, rect.width, rect.height])
            {
                chunk.copy_from_slice(&word.to_le_bytes());
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != REQUEST_LEN || &bytes[..4] != b"KGP2" {
            return None;
        }
        let count = word(bytes, 24);
        if count as usize > DAMAGE_MAX {
            return None;
        }
        let mut rects = [Damage::default(); DAMAGE_MAX];
        for (slot, rect) in bytes[28..].chunks_exact(16).zip(rects.iter_mut()) {
            *rect = Damage {
                x: word(slot, 0),
                y: word(slot, 4),
                width: word(slot, 8),
                height: word(slot, 12),
            };
        }
        let request = Self {
            sequence: word(bytes, 4),
            surface: Surface {
                width: word(bytes, 8),
                height: word(bytes, 12),
                stride: word(bytes, 16),
                format: word(bytes, 20),
            },
            damage: DamageSet { count, rects },
        };
        // Canonical re-encoding also refuses nonzero unused rect slots.
        (request.damage.valid() && request.encode() == bytes).then_some(request)
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

/// One runtime view regrant on Sora's retained drv-fb bootstrap channel: the tag, the view
/// index, and one transferred presentation-channel handle. Clients never see this frame.
/// — TIMBERDOODLE
pub const REGRANT_TAG: u8 = b'R';

pub fn regrant_request(view: usize) -> Option<[u8; 2]> {
    (view < VIEW_COUNT).then_some([REGRANT_TAG, view as u8])
}

pub fn parse_regrant(bytes: &[u8]) -> Option<usize> {
    if bytes.len() == 2 && bytes[0] == REGRANT_TAG && (bytes[1] as usize) < VIEW_COUNT {
        Some(bytes[1] as usize)
    } else {
        None
    }
}

/// A release query on the same control lane: Sora asks drv-fb which views' clients have
/// closed. The reply carries one bit per view; bit N means view N is released. — TIMBERDOODLE
pub const RELEASE_QUERY_TAG: u8 = b'Q';
pub const RELEASE_REPLY_TAG: u8 = b'S';
pub const RELEASE_QUERY: [u8; 1] = [RELEASE_QUERY_TAG];

pub fn release_reply(mask: u8) -> [u8; 2] {
    [RELEASE_REPLY_TAG, mask]
}

pub fn parse_release_reply(bytes: &[u8]) -> Option<u8> {
    if bytes.len() == 2 && bytes[0] == RELEASE_REPLY_TAG {
        Some(bytes[1])
    } else {
        None
    }
}

/// A runtime pointer record on the same control lane: Sora forwards the mouse position its
/// `PointerState` already clamped to the framebuffer. Coordinates are absolute framebuffer
/// pixels; drv-fb owns the sprite and never learns which device moved. — TIMBERDOODLE
pub const POINTER_TAG: u8 = b'P';
pub const POINTER_RECORD_LEN: usize = 6;

pub fn encode_pointer(x: u16, y: u16, buttons: u8) -> [u8; POINTER_RECORD_LEN] {
    let x = x.to_le_bytes();
    let y = y.to_le_bytes();
    [POINTER_TAG, x[0], x[1], y[0], y[1], buttons]
}

pub fn parse_pointer(bytes: &[u8]) -> Option<(u16, u16, u8)> {
    if bytes.len() != POINTER_RECORD_LEN || bytes[0] != POINTER_TAG {
        return None;
    }
    Some((
        u16::from_le_bytes([bytes[1], bytes[2]]),
        u16::from_le_bytes([bytes[3], bytes[4]]),
        bytes[5],
    ))
}

/// A runtime focus record on the same control lane: Sora names the view that owns the keyboard,
/// or clears focus. drv-fb paints that view's chrome in [`VIEW_CHROME_FOCUS`]. — TIMBERDOODLE
pub const FOCUS_TAG: u8 = b'K';
/// The focus record's "no view focused" view byte.
pub const FOCUS_NONE: u8 = u8::MAX;

pub fn focus_record(view: Option<usize>) -> Option<[u8; 2]> {
    match view {
        Some(view) if view >= VIEW_COUNT => None,
        Some(view) => Some([FOCUS_TAG, view as u8]),
        None => Some([FOCUS_TAG, FOCUS_NONE]),
    }
}

pub fn parse_focus(bytes: &[u8]) -> Option<Option<usize>> {
    if bytes.len() != 2 || bytes[0] != FOCUS_TAG {
        return None;
    }
    match bytes[1] {
        FOCUS_NONE => Some(None),
        view if (view as usize) < VIEW_COUNT => Some(Some(view as usize)),
        _ => None,
    }
}

/// The topmost view whose client content contains `(x, y)`, in the band's z-order. Clicks on
/// chrome or between views select nothing, so focus changes only by aiming at content.
/// — TIMBERDOODLE
pub fn band_view_at(band: &Band, x: u32, y: u32) -> Option<usize> {
    for &layer in band.order.iter().rev() {
        let view = &band.views[layer];
        if x >= view.x
            && y >= view.y
            && x < view.x.saturating_add(view.width)
            && y < view.y.saturating_add(view.height)
        {
            return Some(layer);
        }
    }
    None
}

/// A runtime titles toggle on the same control lane: the server draws its own band labels
/// through the glyph atlas, F2c's first visible text consumer. — TIMBERDOODLE
pub const TITLES_TAG: u8 = b'T';

pub fn encode_titles(on: bool) -> [u8; 2] {
    [TITLES_TAG, on as u8]
}

pub fn parse_titles(bytes: &[u8]) -> Option<bool> {
    if bytes.len() == 2 && bytes[0] == TITLES_TAG && bytes[1] <= 1 {
        Some(bytes[1] == 1)
    } else {
        None
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

/// The damage proof's rectangle in surface coordinates and the colour the damaged copy
/// paints. The client poisons every pixel outside this rectangle after its full present, so
/// a presenter that copies more than the declared damage shows magenta on the glass.
/// — TIMBERDOODLE
pub const DAMAGE_RECT: Damage = Damage {
    x: 64,
    y: 32,
    width: 64,
    height: 64,
};
pub const DAMAGE_COLOR: u32 = 0x00ff_8000;

pub fn damage_demo_pixel(x: u32, y: u32) -> u32 {
    let inside = x >= DAMAGE_RECT.x
        && x < DAMAGE_RECT.x + DAMAGE_RECT.width
        && y >= DAMAGE_RECT.y
        && y < DAMAGE_RECT.y + DAMAGE_RECT.height;
    if inside {
        DAMAGE_COLOR
    } else {
        demo_pixel(x, y)
    }
}

/// What the proof client writes after its full present: poison everywhere except the
/// declared damage. — TIMBERDOODLE
pub fn damage_poison_pixel(x: u32, y: u32) -> u32 {
    let inside = x >= DAMAGE_RECT.x
        && x < DAMAGE_RECT.x + DAMAGE_RECT.width
        && y >= DAMAGE_RECT.y
        && y < DAMAGE_RECT.y + DAMAGE_RECT.height;
    if inside {
        DAMAGE_COLOR
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
            damage: DamageSet::FULL,
        };
        let wire = request.encode();
        assert_eq!(&wire[..8], b"KGP2\x78\x56\x34\x12");
        assert_eq!(Present::decode(&wire), Some(request));
        for len in 0..REQUEST_LEN {
            assert_eq!(Present::decode(&wire[..len]), None);
        }
        assert_eq!(Present::decode(&[0; REQUEST_LEN + 1]), None);
        let mut invalid = wire;
        invalid[3] = b'3';
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
    fn band_places_two_overlapping_views_above_the_console() {
        for (width, height) in [(800, 600), (640, 480), (1600, 900), (552, 288), (468, 600)] {
            let layout = band(width, height).expect("views fit");
            assert_eq!(layout.height, BAND_HEIGHT);
            assert_eq!(layout.views.len(), VIEW_COUNT);
            assert_eq!(layout.order, [0, 1]);
            let console = console_region(width, height);
            assert_eq!(console.y, BAND_HEIGHT);
            assert_eq!(console.height, height - BAND_HEIGHT);
            assert!(console.height >= MIN_CONSOLE_ROWS * CONSOLE_CELL_H);
            let first = layout.views[0];
            let second = layout.views[1];
            for view in [first, second] {
                assert_eq!((view.width, view.height), (VIEW_WIDTH, VIEW_HEIGHT));
                assert!(view.x >= VIEW_CHROME && view.y >= VIEW_CHROME);
                assert!(view.x + view.width + VIEW_CHROME <= width);
                assert!(view.y + view.height + VIEW_CHROME <= BAND_HEIGHT);
            }
            assert_eq!(first.y, second.y);
            assert_eq!(second.x, first.x + VIEW_WIDTH - VIEW_OVERLAP);
        }
    }

    #[test]
    fn band_declines_when_the_console_would_not_fit() {
        assert_eq!(band(467, 600), None);
        assert_eq!(band(800, 287), None);
        assert_eq!(
            console_region(800, 287),
            ConsoleRegion {
                y: 0,
                width: 800,
                height: 287
            }
        );
        assert_eq!(console_region(467, 600).y, 0);
    }

    #[test]
    fn clip_demo_poisons_only_the_pixels_the_view_cannot_show() {
        assert!(CLIP_DEMO.fits(SURFACE_BYTES));
        assert_eq!(clip_demo_pixel(VIEW_WIDTH - 1, VIEW_HEIGHT - 1), VIEW_SOLID);
        assert_eq!(clip_demo_pixel(VIEW_WIDTH, VIEW_HEIGHT / 2), CLIP_POISON);
        assert_eq!(clip_demo_pixel(VIEW_WIDTH / 2, VIEW_HEIGHT), CLIP_POISON);
    }

    #[test]
    fn damage_frames_round_trip_and_reject_malformed_sets() {
        let rect = Damage {
            x: 16,
            y: 8,
            width: 32,
            height: 24,
        };
        let request = Present {
            sequence: 7,
            surface: DEMO,
            damage: DamageSet::single(rect).unwrap(),
        };
        let wire = request.encode();
        assert_eq!(&wire[..4], b"KGP2");
        assert_eq!(Present::decode(&wire), Some(request));
        let full = Present {
            damage: DamageSet::FULL,
            ..request
        };
        assert_eq!(Present::decode(&full.encode()), Some(full));
        for len in 0..REQUEST_LEN {
            assert_eq!(Present::decode(&wire[..len]), None);
        }
        // count beyond the fixed capacity
        let mut bad = wire;
        bad[24..28].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(Present::decode(&bad), None);
        // zero-area rectangle
        let mut bad = wire;
        bad[36..40].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(Present::decode(&bad), None);
        // overflowing rectangle
        let mut bad = wire;
        bad[28..32].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(Present::decode(&bad), None);
        // unused rect slots are not an extension point
        let mut bad = wire;
        bad[44..48].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(Present::decode(&bad), None);
        assert!(DamageSet::single(Damage { width: 0, ..rect }).is_none());
        assert!(!Damage {
            x: u32::MAX,
            width: 2,
            ..rect
        }
        .valid());
    }

    #[test]
    fn damage_clips_to_the_surface_and_keeps_its_extent_rules() {
        let surface = Surface {
            width: 100,
            height: 50,
            stride: 100,
            format: XRGB8888,
        };
        assert_eq!(
            Damage {
                x: 90,
                y: 40,
                width: 20,
                height: 20
            }
            .clipped_to(surface),
            Some(Damage {
                x: 90,
                y: 40,
                width: 10,
                height: 10
            })
        );
        assert_eq!(
            Damage {
                x: 200,
                y: 0,
                width: 10,
                height: 10
            }
            .clipped_to(surface),
            None
        );
        assert_eq!(
            Damage {
                x: 0,
                y: 0,
                width: 0,
                height: 1
            }
            .clipped_to(surface),
            None
        );
    }

    #[test]
    fn damage_demo_differs_only_inside_the_rect_and_poison_is_outside() {
        let rect = DAMAGE_RECT;
        assert_eq!(damage_demo_pixel(rect.x, rect.y), DAMAGE_COLOR);
        assert_eq!(
            damage_demo_pixel(rect.x + rect.width - 1, rect.y + rect.height - 1),
            DAMAGE_COLOR
        );
        assert_eq!(
            damage_demo_pixel(rect.x - 1, rect.y),
            demo_pixel(rect.x - 1, rect.y)
        );
        assert_eq!(
            damage_demo_pixel(rect.x, rect.y - 1),
            demo_pixel(rect.x, rect.y - 1)
        );
        assert_eq!(damage_poison_pixel(rect.x, rect.y), DAMAGE_COLOR);
        assert_eq!(damage_poison_pixel(0, 0), CLIP_POISON);
    }

    #[test]
    fn regrant_frames_round_trip_and_reject_out_of_range_views() {
        for view in 0..VIEW_COUNT {
            let frame = regrant_request(view).unwrap();
            assert_eq!(frame[0], REGRANT_TAG);
            assert_eq!(parse_regrant(&frame), Some(view));
        }
        assert_eq!(regrant_request(VIEW_COUNT), None);
        assert_eq!(parse_regrant(&[REGRANT_TAG]), None);
        assert_eq!(parse_regrant(&[REGRANT_TAG, VIEW_COUNT as u8]), None);
        assert_eq!(parse_regrant(&[b'X', 0]), None);
        assert_eq!(parse_regrant(&[REGRANT_TAG, 0, 0]), None);
    }

    #[test]
    fn release_query_and_reply_are_exact() {
        assert_eq!(RELEASE_QUERY, [b'Q']);
        assert_eq!(release_reply(0b101), [b'S', 0b101]);
        assert_eq!(parse_release_reply(&release_reply(0)), Some(0));
        assert_eq!(parse_release_reply(&release_reply(0xff)), Some(0xff));
        assert_eq!(parse_release_reply(&[b'S']), None);
        assert_eq!(parse_release_reply(&[b'X', 0]), None);
        assert_eq!(parse_release_reply(&[b'S', 0, 0]), None);
    }

    #[test]
    fn pointer_records_round_trip_and_reject_bad_frames() {
        let record = encode_pointer(0, 0, 0);
        assert_eq!(record[0], POINTER_TAG);
        assert_eq!(parse_pointer(&record), Some((0, 0, 0)));
        let record = encode_pointer(u16::MAX, 1234, 0b0000_0111);
        assert_eq!(parse_pointer(&record), Some((u16::MAX, 1234, 0x07)));
        assert_eq!(parse_pointer(&record[..5]), None);
        assert_eq!(parse_pointer(&[POINTER_TAG; POINTER_RECORD_LEN + 1]), None);
        assert_eq!(parse_pointer(&[b'X', 0, 0, 0, 0, 0]), None);
    }

    #[test]
    fn focus_records_round_trip_and_reject_bad_frames() {
        for view in 0..VIEW_COUNT {
            let record = focus_record(Some(view)).unwrap();
            assert_eq!(record[0], FOCUS_TAG);
            assert_eq!(parse_focus(&record), Some(Some(view)));
        }
        let none = focus_record(None).unwrap();
        assert_eq!(none, [FOCUS_TAG, FOCUS_NONE]);
        assert_eq!(parse_focus(&none), Some(None));
        assert_eq!(focus_record(Some(VIEW_COUNT)), None);
        assert_eq!(parse_focus(&[FOCUS_TAG]), None);
        assert_eq!(parse_focus(&[FOCUS_TAG, VIEW_COUNT as u8]), None);
        assert_eq!(parse_focus(&[b'X', 0]), None);
        assert_eq!(parse_focus(&[FOCUS_TAG, 0, 0]), None);
    }

    #[test]
    fn band_view_at_returns_the_topmost_content_view() {
        let Some(band) = band(800, 600) else {
            return;
        };
        let lower = band.views[0];
        let upper = band.views[1];
        // The overlap belongs to the upper view; each view's own corner belongs to it.
        let overlap_x = upper.x + 2;
        let overlap_y = upper.y + 2;
        assert!(overlap_x < lower.x + lower.width);
        assert_eq!(band_view_at(&band, overlap_x, overlap_y), Some(1));
        assert_eq!(band_view_at(&band, lower.x + 2, lower.y + 2), Some(0));
        assert_eq!(band_view_at(&band, lower.x - 4, lower.y - 4), None);
        assert_eq!(band_view_at(&band, 0, 0), None);
    }

    #[test]
    fn titles_records_round_trip_and_reject_bad_frames() {
        assert_eq!(encode_titles(true), [TITLES_TAG, 1]);
        assert_eq!(encode_titles(false), [TITLES_TAG, 0]);
        assert_eq!(parse_titles(&[TITLES_TAG, 1]), Some(true));
        assert_eq!(parse_titles(&[TITLES_TAG, 0]), Some(false));
        assert_eq!(parse_titles(&[TITLES_TAG, 2]), None);
        assert_eq!(parse_titles(&[TITLES_TAG]), None);
        assert_eq!(parse_titles(&[b'X', 0]), None);
        assert_eq!(parse_titles(&[TITLES_TAG, 0, 0]), None);
    }
}
