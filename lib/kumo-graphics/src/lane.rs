//j774
//! GL3-b (PLAN/015 §8.32): the cooperative GPU client's half of Sora's lane bootstrap
//! (DESIGN/024), and the CPU copy of a public RGBA8 readback into view `GPU_VIEW`'s surface,
//! presented until the hold ends. Everything here is pure: the caller supplies the kernel
//! reads, writes and waits, so each rule is host-tested and the metal client only adapts
//! syscalls.

use crate::{
    DamageSet, Present, Reply, Status, Surface, REPLY_LEN, REQUEST_LEN, VIEW_HEIGHT, VIEW_WIDTH,
    XRGB8888,
};

/// Sora's lane records: `V` carries the view's presentation channel, `S` its surface VMO and
/// `N` the native producer (or handle 0). A default image sends `N` alone; a `view` or `churn`
/// image sends `V`, `S`, then `N`.
pub const VIEW_CHANNEL_TAG: u8 = b'V';
pub const VIEW_SURFACE_TAG: u8 = b'S';
pub const NATIVE_TAG: u8 = b'N';

/// One read of the lane pair as the kernel returned it: the byte count (0 when the peer
/// closed, negative as `i64` on error), the first byte, and the raw transferred handle (0 for
/// none).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LaneRead {
    pub len: u64,
    pub tag: u8,
    pub handle: u64,
}

/// The lane Sora named.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lane {
    /// `N` alone: a default image. `producer` is the native presenter, 0 when there is none.
    Native { producer: u64 },
    /// `V`, `S`, then `N`: view `GPU_VIEW`'s presentation channel and surface. `producer` may
    /// honestly be 0 (a regranted churn session carries none).
    View {
        channel: u64,
        surface: u64,
        producer: u64,
    },
}

/// A usable transferred handle: nonzero, and not an error status.
pub fn valid_handle(raw: u64) -> bool {
    raw != 0 && (raw as i64) > 0
}

fn refuse(
    close: &mut impl FnMut(u64),
    held: &[u64],
    cause: &'static str,
) -> Result<Lane, &'static str> {
    for &raw in held {
        if valid_handle(raw) {
            close(raw);
        }
    }
    Err(cause)
}

/// Read Sora's lane record. A closed pair, a failed read, a record that is not one tag byte,
/// a missing view handle or an unexpected tag fails with a named cause, and `close` releases
/// every handle the decode had received, including one riding the refused record, so a
/// failed decode keeps nothing.
pub fn decode_lane(
    mut read: impl FnMut() -> LaneRead,
    mut close: impl FnMut(u64),
) -> Result<Lane, &'static str> {
    // Classify one record against the tag the protocol expects next.
    fn check(record: LaneRead, closed: &'static str) -> Result<(), &'static str> {
        if record.len == 0 {
            Err(closed)
        } else if (record.len as i64) < 0 {
            Err("lane read failed")
        } else if record.len != 1 {
            Err("lane tag was not one byte")
        } else {
            Ok(())
        }
    }
    let first = read();
    if let Err(cause) = check(first, "lane closed before a tag") {
        return refuse(&mut close, &[first.handle], cause);
    }
    match first.tag {
        NATIVE_TAG => match first.handle {
            0 => Ok(Lane::Native { producer: 0 }),
            raw if valid_handle(raw) => Ok(Lane::Native { producer: raw }),
            _ => Err("native lane carried an invalid handle"),
        },
        VIEW_CHANNEL_TAG => {
            let channel = first.handle;
            if !valid_handle(channel) {
                return Err("view lane carried no view channel");
            }
            let second = read();
            if let Err(cause) = check(second, "view lane closed before its surface") {
                return refuse(&mut close, &[channel, second.handle], cause);
            }
            if second.tag != VIEW_SURFACE_TAG {
                return refuse(
                    &mut close,
                    &[channel, second.handle],
                    "view lane surface tag missing",
                );
            }
            let surface = second.handle;
            if !valid_handle(surface) {
                return refuse(&mut close, &[channel], "view lane carried no view surface");
            }
            let third = read();
            if let Err(cause) = check(third, "view lane closed before its native producer") {
                return refuse(&mut close, &[channel, surface, third.handle], cause);
            }
            if third.tag != NATIVE_TAG {
                return refuse(
                    &mut close,
                    &[channel, surface, third.handle],
                    "view lane native tag missing",
                );
            }
            match third.handle {
                0 => Ok(Lane::View {
                    channel,
                    surface,
                    producer: 0,
                }),
                raw if valid_handle(raw) => Ok(Lane::View {
                    channel,
                    surface,
                    producer: raw,
                }),
                _ => refuse(
                    &mut close,
                    &[channel, surface],
                    "view lane carried an invalid native producer",
                ),
            }
        }
        _ => refuse(&mut close, &[first.handle], "unknown lane tag"),
    }
}

/// The surface GL3-b presents: the whole 256x128 view, unpadded XRGB8888.
pub const VIEW_SURFACE: Surface = Surface {
    width: VIEW_WIDTH,
    height: VIEW_HEIGHT,
    stride: VIEW_WIDTH,
    format: XRGB8888,
};

/// Bytes per surface write: the VmoWrite syscall's 256-byte chunk (64 pixels), the shape the
/// Rust gpu-client and drv-gpu use for view surfaces.
pub const COPY_CHUNK: usize = 256;

/// One packed RGBA8 pixel as `glReadPixels(GL_RGBA, GL_UNSIGNED_BYTE)` stores it (bytes R, G,
/// B, A, read as a little-endian word), as XRGB8888: `0x00RRGGBB`, the layout drv-fb reads
/// (red in bits 16-23). Alpha is dropped; XRGB has none.
pub const fn rgba8_to_xrgb8888(rgba: u32) -> u32 {
    let [r, g, b, _] = rgba.to_le_bytes();
    u32::from_le_bytes([b, g, r, 0])
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CopyError {
    /// The surface is not a valid unpadded view-class surface, or the readback is not
    /// exactly its size. Nothing was written.
    Geometry,
    /// The write at this byte offset was refused; earlier chunks were written.
    Write { offset: u64 },
}

/// Copy a readback of `surface.width` x `surface.height` RGBA8 pixels, rows bottom-up as
/// `glReadPixels` returns them, into the top-down XRGB8888 surface, in [`COPY_CHUNK`]-byte
/// writes at increasing offsets. Surface row `y` is readback row `height - 1 - y`, so the
/// image appears upright. Returns the number of writes. Geometry is checked before the
/// first write.
pub fn copy_readback(
    pixels: &[u32],
    surface: Surface,
    mut write: impl FnMut(u64, &[u8]) -> bool,
) -> Result<u32, CopyError> {
    let Some(bytes) = surface.byte_len() else {
        return Err(CopyError::Geometry);
    };
    let (width, height) = (surface.width as usize, surface.height as usize);
    if surface.stride != surface.width || pixels.len() != width * height {
        return Err(CopyError::Geometry);
    }
    let mut chunk = [0u8; COPY_CHUNK];
    let (mut offset, mut writes) = (0usize, 0u32);
    while offset < bytes {
        let len = (bytes - offset).min(COPY_CHUNK);
        for (index, out) in chunk[..len].chunks_exact_mut(4).enumerate() {
            let pixel = offset / 4 + index;
            let (x, y) = (pixel % width, pixel / width);
            let source = pixels[(height - 1 - y) * width + x];
            out.copy_from_slice(&rgba8_to_xrgb8888(source).to_le_bytes());
        }
        if !write(offset as u64, &chunk[..len]) {
            return Err(CopyError::Write {
                offset: offset as u64,
            });
        }
        writes += 1;
        offset += len;
    }
    Ok(writes)
}

/// The client side of one presentation channel as GL3-b's hold drives it: `send` writes one
/// request, `receive` waits (bounded) for the next reply and returns its length, and `pause`
/// sleeps between presents so the frame stays on the glass.
pub trait PresentLink {
    fn send(&mut self, request: &[u8; REQUEST_LEN]) -> Result<(), &'static str>;
    fn receive(&mut self, reply: &mut [u8; REPLY_LEN + 1]) -> Result<usize, &'static str>;
    fn pause(&mut self);
}

/// What the hold observed. `Retired` and `Copied` stay distinct (DESIGN/019).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Tally {
    pub presents: u32,
    pub retired: u32,
    pub copied: u32,
}

/// Why a hold ended early, naming the 1-based present that failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HoldError {
    /// The link failed sending or awaiting this present.
    Link { present: u32, cause: &'static str },
    /// The reply did not decode.
    Malformed { present: u32 },
    /// The reply named another sequence. Only one request is outstanding, so this is a
    /// protocol fault.
    Sequence { present: u32, got: u32 },
    /// drv-fb refused the present.
    Refused { present: u32, status: Status },
}

/// Present `surface` `count` times, each with a fresh sequence (`first`, `first + 1`, ...)
/// and FULL damage, one request outstanding at a time, pausing after each reply. Both honest
/// successes count, separately: `Retired` (a scanout consumed the frame) and `Copied` (the
/// pixels are in the scanout buffer, retirement unproven). A client never reads `Copied` as
/// `Retired`. Any other status, a malformed or mismatched reply, or a link failure ends the
/// hold.
pub fn present_hold<L: PresentLink>(
    link: &mut L,
    surface: Surface,
    first: u32,
    count: u32,
) -> Result<Tally, HoldError> {
    let mut tally = Tally::default();
    for index in 0..count {
        let present = index + 1;
        let sequence = first.wrapping_add(index);
        let request = Present {
            sequence,
            surface,
            damage: DamageSet::FULL,
        }
        .encode();
        link.send(&request)
            .map_err(|cause| HoldError::Link { present, cause })?;
        let mut bytes = [0u8; REPLY_LEN + 1];
        let len = link
            .receive(&mut bytes)
            .map_err(|cause| HoldError::Link { present, cause })?;
        let reply = bytes
            .get(..len)
            .and_then(Reply::decode)
            .ok_or(HoldError::Malformed { present })?;
        if reply.sequence != sequence {
            return Err(HoldError::Sequence {
                present,
                got: reply.sequence,
            });
        }
        match reply.status {
            Status::Retired => tally.retired += 1,
            Status::Copied => tally.copied += 1,
            status => return Err(HoldError::Refused { present, status }),
        }
        tally.presents = present;
        link.pause();
    }
    Ok(tally)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    fn record(tag: u8, handle: u64) -> LaneRead {
        LaneRead {
            len: 1,
            tag,
            handle,
        }
    }
    const CLOSED: LaneRead = LaneRead {
        len: 0,
        tag: 0,
        handle: 0,
    };

    /// Decode a scripted lane, returning the result and the handles it closed.
    fn decode(records: &[LaneRead]) -> (Result<Lane, &'static str>, Vec<u64>) {
        let mut next = records.iter().copied();
        let mut closed = Vec::new();
        let result = decode_lane(|| next.next().unwrap_or(CLOSED), |raw| closed.push(raw));
        (result, closed)
    }

    #[test]
    fn decodes_both_lanes_sora_sends() {
        assert_eq!(
            decode(&[record(b'N', 7)]),
            (Ok(Lane::Native { producer: 7 }), Vec::new())
        );
        assert_eq!(
            decode(&[record(b'N', 0)]),
            (Ok(Lane::Native { producer: 0 }), Vec::new())
        );
        let view = [record(b'V', 11), record(b'S', 12), record(b'N', 13)];
        assert_eq!(
            decode(&view),
            (
                Ok(Lane::View {
                    channel: 11,
                    surface: 12,
                    producer: 13
                }),
                Vec::new()
            )
        );
        let regranted = [record(b'V', 11), record(b'S', 12), record(b'N', 0)];
        assert_eq!(
            decode(&regranted).0,
            Ok(Lane::View {
                channel: 11,
                surface: 12,
                producer: 0
            })
        );
    }

    #[test]
    fn a_failed_decode_keeps_no_handle() {
        let cases: [(&[LaneRead], &str, &[u64]); 11] = [
            (&[], "lane closed before a tag", &[]),
            (
                &[LaneRead {
                    len: u64::MAX,
                    tag: 0,
                    handle: 0,
                }],
                "lane read failed",
                &[],
            ),
            (
                &[LaneRead {
                    len: 2,
                    tag: b'N',
                    handle: 5,
                }],
                "lane tag was not one byte",
                &[5],
            ),
            (
                &[record(b'N', u64::MAX)],
                "native lane carried an invalid handle",
                &[],
            ),
            (&[record(b'X', 5)], "unknown lane tag", &[5]),
            (&[record(b'V', 0)], "view lane carried no view channel", &[]),
            (
                &[record(b'V', 11)],
                "view lane closed before its surface",
                &[11],
            ),
            (
                &[record(b'V', 11), record(b'N', 12)],
                "view lane surface tag missing",
                &[11, 12],
            ),
            (
                &[record(b'V', 11), record(b'S', 0)],
                "view lane carried no view surface",
                &[11],
            ),
            (
                &[record(b'V', 11), record(b'S', 12)],
                "view lane closed before its native producer",
                &[11, 12],
            ),
            (
                &[record(b'V', 11), record(b'S', 12), record(b'N', u64::MAX)],
                "view lane carried an invalid native producer",
                &[11, 12],
            ),
        ];
        for (records, cause, closed) in cases {
            assert_eq!(decode(records), (Err(cause), closed.to_vec()), "{cause}");
        }
        assert_eq!(
            decode(&[record(b'V', 11), record(b'S', 12), record(b'S', 13)]),
            (Err("view lane native tag missing"), [11, 12, 13].to_vec())
        );
    }

    #[test]
    fn rgba8_readback_words_become_xrgb8888() {
        // GL3-a's measured colors: the triangle and the clear.
        assert_eq!(rgba8_to_xrgb8888(0xff66_33cc), 0x00cc_3366);
        assert_eq!(rgba8_to_xrgb8888(0xff99_6633), 0x0033_6699);
        assert_eq!(rgba8_to_xrgb8888(0x0000_00ff), 0x00ff_0000);
        assert_eq!(rgba8_to_xrgb8888(0x00ff_0000), 0x0000_00ff);
    }

    #[test]
    fn copy_flips_rows_upright_in_chunks() {
        let surface = Surface {
            width: 2,
            height: 2,
            stride: 2,
            format: XRGB8888,
        };
        // Readback rows bottom-up: row 0 is the bottom of the image.
        let pixels = [0x0000_00a1, 0x0000_00a2, 0x0000_00b1, 0x0000_00b2];
        let mut writes = Vec::new();
        let count = copy_readback(&pixels, surface, |offset, bytes| {
            writes.push((offset, bytes.to_vec()));
            true
        });
        assert_eq!(count, Ok(1));
        let words: Vec<u32> = writes[0]
            .1
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        // The top surface row holds the readback's top (last) row.
        assert_eq!(words, [0x00b1_0000, 0x00b2_0000, 0x00a1_0000, 0x00a2_0000]);
    }

    #[test]
    fn the_gl3_triangle_fills_view_one_upright() {
        const SHADED: u32 = 0xff66_33cc;
        const CLEARED: u32 = 0xff99_6633;
        let (width, height) = (VIEW_WIDTH as usize, VIEW_HEIGHT as usize);
        let mut pixels = std::vec![0u32; width * height];
        for y in 0..height {
            for x in 0..width {
                pixels[y * width + x] = if x + 2 * y <= 254 { SHADED } else { CLEARED };
            }
        }
        let mut surface = std::vec![0u8; width * height * 4];
        let mut offsets = Vec::new();
        let count = copy_readback(&pixels, VIEW_SURFACE, |offset, bytes| {
            offsets.push(offset);
            surface[offset as usize..offset as usize + bytes.len()].copy_from_slice(bytes);
            true
        });
        assert_eq!(count, Ok(512));
        assert!(offsets
            .iter()
            .enumerate()
            .all(|(i, &o)| o == i as u64 * 256));
        let at = |x: usize, y: usize| {
            let i = (y * width + x) * 4;
            u32::from_le_bytes(surface[i..i + 4].try_into().unwrap())
        };
        // The right angle sits bottom-left on the glass; only (0,0) is shaded on the top row.
        assert_eq!(at(0, 0), 0x00cc_3366);
        assert_eq!(at(1, 0), 0x0033_6699);
        assert_eq!(at(0, 127), 0x00cc_3366);
        assert_eq!(at(254, 127), 0x00cc_3366);
        assert_eq!(at(255, 127), 0x0033_6699);
        assert_eq!(at(255, 0), 0x0033_6699);
        let shaded = (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .filter(|&(x, y)| at(x, y) == 0x00cc_3366)
            .count();
        assert_eq!(shaded, 16384);
    }

    #[test]
    fn copy_checks_geometry_first_and_stops_at_a_refused_write() {
        let mut wrote = false;
        let short = [0u32; 3];
        let surface = Surface {
            width: 2,
            height: 2,
            stride: 2,
            format: XRGB8888,
        };
        assert_eq!(
            copy_readback(&short, surface, |_, _| {
                wrote = true;
                true
            }),
            Err(CopyError::Geometry)
        );
        let padded = Surface {
            stride: 3,
            ..surface
        };
        let pixels = [0u32; 4];
        assert_eq!(
            copy_readback(&pixels, padded, |_, _| {
                wrote = true;
                true
            }),
            Err(CopyError::Geometry)
        );
        let wrong_format = Surface {
            format: 7,
            ..surface
        };
        assert_eq!(
            copy_readback(&pixels, wrong_format, |_, _| {
                wrote = true;
                true
            }),
            Err(CopyError::Geometry)
        );
        assert!(!wrote);
        let pixels = std::vec![0u32; 256 * 128];
        let mut calls = 0;
        assert_eq!(
            copy_readback(&pixels, VIEW_SURFACE, |offset, _| {
                calls += 1;
                offset < 512
            }),
            Err(CopyError::Write { offset: 512 })
        );
        assert_eq!(calls, 3);
    }

    /// A scripted presentation channel: each queued reply is raw bytes or a link failure.
    struct Script {
        replies: Vec<Result<Vec<u8>, &'static str>>,
        sent: Vec<Present>,
        pauses: u32,
        send_fails_at: Option<usize>,
    }
    impl Script {
        fn statuses(first: u32, statuses: &[Status]) -> Self {
            let replies = statuses
                .iter()
                .enumerate()
                .map(|(i, &status)| {
                    Ok(Reply {
                        sequence: first + i as u32,
                        status,
                    }
                    .encode()
                    .to_vec())
                })
                .collect();
            Self {
                replies,
                sent: Vec::new(),
                pauses: 0,
                send_fails_at: None,
            }
        }
    }
    impl PresentLink for Script {
        fn send(&mut self, request: &[u8; REQUEST_LEN]) -> Result<(), &'static str> {
            if self.send_fails_at == Some(self.sent.len()) {
                return Err("present request write failed");
            }
            self.sent
                .push(Present::decode(request).expect("canonical request"));
            Ok(())
        }
        fn receive(&mut self, reply: &mut [u8; REPLY_LEN + 1]) -> Result<usize, &'static str> {
            let next = self.replies.remove(0)?;
            reply[..next.len()].copy_from_slice(&next);
            Ok(next.len())
        }
        fn pause(&mut self) {
            self.pauses += 1;
        }
    }

    #[test]
    fn a_hold_presents_fresh_full_frames_and_counts_both_successes() {
        let statuses = [
            Status::Copied,
            Status::Copied,
            Status::Retired,
            Status::Copied,
        ];
        let mut link = Script::statuses(1, &statuses);
        let tally = present_hold(&mut link, VIEW_SURFACE, 1, 4);
        assert_eq!(
            tally,
            Ok(Tally {
                presents: 4,
                retired: 1,
                copied: 3
            })
        );
        assert_eq!(link.pauses, 4);
        for (i, present) in link.sent.iter().enumerate() {
            assert_eq!(present.sequence, 1 + i as u32);
            assert_eq!(present.surface, VIEW_SURFACE);
            assert!(present.damage.is_full());
        }
        let mut none = Script::statuses(1, &[]);
        assert_eq!(
            present_hold(&mut none, VIEW_SURFACE, 1, 0),
            Ok(Tally::default())
        );
        assert!(none.sent.is_empty());
    }

    #[test]
    fn a_hold_ends_named_on_refusal_mismatch_or_link_failure() {
        let mut refused = Script::statuses(1, &[Status::Copied, Status::BadSurface]);
        assert_eq!(
            present_hold(&mut refused, VIEW_SURFACE, 1, 5),
            Err(HoldError::Refused {
                present: 2,
                status: Status::BadSurface
            })
        );
        assert_eq!(refused.pauses, 1);

        let mut mismatched = Script::statuses(9, &[Status::Copied]);
        assert_eq!(
            present_hold(&mut mismatched, VIEW_SURFACE, 1, 1),
            Err(HoldError::Sequence { present: 1, got: 9 })
        );

        let mut malformed = Script::statuses(1, &[]);
        malformed.replies.push(Ok(b"KGR1short".to_vec()));
        assert_eq!(
            present_hold(&mut malformed, VIEW_SURFACE, 1, 1),
            Err(HoldError::Malformed { present: 1 })
        );

        let mut timed_out = Script::statuses(1, &[Status::Copied]);
        timed_out.replies.push(Err("present reply timed out"));
        assert_eq!(
            present_hold(&mut timed_out, VIEW_SURFACE, 1, 2),
            Err(HoldError::Link {
                present: 2,
                cause: "present reply timed out"
            })
        );

        let mut unsent = Script::statuses(1, &[Status::Copied]);
        unsent.send_fails_at = Some(0);
        assert_eq!(
            present_hold(&mut unsent, VIEW_SURFACE, 1, 1),
            Err(HoldError::Link {
                present: 1,
                cause: "present request write failed"
            })
        );
    }
}
