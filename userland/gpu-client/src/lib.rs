#![no_std]
//j595

//! The G6b proof client's actual CSF stream, exercised by drv-gpu's host command-stream model.
//! It stores one value with system scope and waits. Output cache cleaning belongs to the
//! driver's completion wrapper, not this client. — TIMBERDOODLE

pub const VALUES: [u32; 2] = [0xc0de_0001, 0xc0de_0002];

pub fn proof_stream(target: u64, value: u32) -> [u64; 4] {
    [
        (1u64 << 56) | (target & 0x0000_ffff_ffff_ffff), // MOVE48 r0, target
        (2u64 << 56) | (2u64 << 48) | u64::from(value),  // MOVE32 r2, value
        (38u64 << 56) | (2u64 << 32),                    // SYNC_SET32 slot0, [r0], r2
        (3u64 << 56) | (1u64 << 16),                     // WAIT slot0
    ]
}
