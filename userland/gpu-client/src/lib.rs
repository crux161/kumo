#![no_std]
//j595
//j596
//j637
//j711

//! The G6b proof client's actual CSF stream, exercised by drv-gpu's host command-stream model.
//! It stores one value with system scope and waits. Output cache cleaning belongs to the
//! driver's completion wrapper, not this client. — TIMBERDOODLE

pub const VALUES: [u32; 2] = [0xc0de_0001, 0xc0de_0002];

pub const SCRATCH_OFFSET: u64 = kumo_gpu::PAGE_BYTES;
pub const BINARY_OFFSET: u64 = 2 * kumo_gpu::PAGE_BYTES;
pub const DATA_OFFSET: u64 = 3 * kumo_gpu::PAGE_BYTES;
pub const TRIANGLE_BINARY_OFFSET: u64 = BINARY_OFFSET + 0x800;
pub const TRIANGLE_DATA_OFFSET: u64 = DATA_OFFSET + 0x100;
const _: () = assert!(kumo_mali::KUMO_PAINT_V10.binary.len() <= 0x800);
const _: () = assert!(kumo_mali::KUMO_TRIANGLE_V10.binary.len() <= 0x800);
pub const PAINT_OFFSET: u64 = 0x1_0000;
pub const BUFFER_BYTES: u64 = PAINT_OFFSET + kumo_mali::PAINT_BYTES;
pub const PIXEL_POISON: u32 = 0x00ff_00ff;

//j711
//j711
/// The churn lane's per-frame buffer: 160 KiB (40 pages). One hundred counted frames churn
/// exactly 16,384,000 bytes of buffer objects through the session. — TIMBERDOODLE
pub const FRAME_BYTES: u64 = 0x2_8000;
/// The churn window: page-aligned, inside the service [VA_START, VA_END) range and far
/// above the fixed bring-up layout. Each frame binds its whole buffer here, and the paint
/// kernel writes its first PAINT_BYTES at this GPU address. — TIMBERDOODLE
pub const CHURN_VA: u64 = kumo_gpu::VA_START + 0x0080_0000;
/// Byte offset of the paint kernel's output-address word in its 104-byte descriptor block:
/// the sysvals start at byte 64 and `out` is the first push argument (`args_offset` 24).
pub const PAINT_OUTPUT_VA_WORD: usize = 88;

//j711
/// Point the pinned paint descriptor at `target`. The bring-up upload fixed the output at
/// `VA_START + PAINT_OFFSET`; the churn lane rewrites that one word per frame and applies
/// the same alignment and 48-bit extent check kumo-mali made when it built the block.
/// — TIMBERDOODLE
pub fn retarget_paint(data: &mut [u8; 104], target: u64) -> Result<(), kumo_mali::KernelError> {
    if target % 4 != 0
        || target
            .checked_add(kumo_mali::PAINT_BYTES)
            .is_none_or(|end| end > (1u64 << 48))
    {
        return Err(kumo_mali::KernelError::OutputAddress(target));
    }
    data[PAINT_OUTPUT_VA_WORD..PAINT_OUTPUT_VA_WORD + 8].copy_from_slice(&target.to_le_bytes());
    Ok(())
}

/// The same backing VMO, with separate GPU permissions for commands, code, descriptors and
/// output. Every range is installed before GroupCreate freezes the bindings. — TIMBERDOODLE
pub fn bindings(vm: u32, buffer: u32) -> [kumo_gpu::Binding; 5] {
    use kumo_gpu::{BindFlags as F, Binding, PAGE_BYTES, VA_START};
    core::array::from_fn(|index| {
        let (offset, bytes, flags) = [
            (0, PAGE_BYTES, F::READ_ONLY | F::NO_EXECUTE | F::UNCACHED),
            (SCRATCH_OFFSET, PAGE_BYTES, F::NO_EXECUTE | F::UNCACHED),
            (BINARY_OFFSET, PAGE_BYTES, F::READ_ONLY),
            (
                DATA_OFFSET,
                PAGE_BYTES,
                F::READ_ONLY | F::NO_EXECUTE | F::UNCACHED,
            ),
            (
                PAINT_OFFSET,
                kumo_mali::PAINT_BYTES,
                F::NO_EXECUTE | F::UNCACHED,
            ),
        ][index];
        Binding {
            vm,
            buffer,
            va: VA_START + offset,
            offset,
            bytes,
            flags: F(flags),
        }
    })
}

pub fn compute_data() -> Result<[u8; 104], kumo_mali::KernelError> {
    kumo_mali::kumo_paint_data_at(
        kumo_gpu::VA_START + BINARY_OFFSET,
        kumo_gpu::VA_START + PAINT_OFFSET,
    )
}

pub fn compute_stream(
    features: kumo_mali::CsFeatures,
) -> Result<[u64; 24], kumo_mali::KernelError> {
    kumo_mali::kumo_paint_stream_at(features, kumo_gpu::VA_START + DATA_OFFSET)
}

pub fn triangle_data() -> Result<[u8; 104], kumo_mali::KernelError> {
    kumo_mali::kumo_triangle_data_at(
        kumo_gpu::VA_START + TRIANGLE_BINARY_OFFSET,
        kumo_gpu::VA_START + PAINT_OFFSET,
    )
}
pub fn triangle_stream(
    features: kumo_mali::CsFeatures,
) -> Result<[u64; 24], kumo_mali::KernelError> {
    kumo_mali::kumo_triangle_stream_at(features, kumo_gpu::VA_START + TRIANGLE_DATA_OFFSET)
}

pub fn proof_stream(target: u64, value: u32) -> [u64; 4] {
    [
        (1u64 << 56) | (target & 0x0000_ffff_ffff_ffff), // MOVE48 r0, target
        (2u64 << 56) | (2u64 << 48) | u64::from(value),  // MOVE32 r2, value
        (38u64 << 56) | (2u64 << 32),                    // SYNC_SET32 slot0, [r0], r2
        (3u64 << 56) | (1u64 << 16),                     // WAIT slot0
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use kumo_gpu::{BindFlags as F, PAGE_BYTES, VA_START};

    #[test]
    fn both_kernel_uploads_fit_distinct_immutable_slots_in_the_frozen_bindings() {
        let binds = bindings(1, 1);
        let code = binds[2];
        let descriptors = binds[3];
        assert!(
            BINARY_OFFSET + kumo_mali::KUMO_PAINT_V10.binary.len() as u64 <= TRIANGLE_BINARY_OFFSET
        );
        assert!(
            TRIANGLE_BINARY_OFFSET + kumo_mali::KUMO_TRIANGLE_V10.binary.len() as u64
                <= code.offset + code.bytes
        );
        assert!(DATA_OFFSET + 104 <= TRIANGLE_DATA_OFFSET);
        assert!(TRIANGLE_DATA_OFFSET + 104 <= descriptors.offset + descriptors.bytes);
        assert_eq!(TRIANGLE_BINARY_OFFSET % kumo_mali::SHADER_BINARY_ALIGN, 0);
        let data = triangle_data().unwrap();
        assert_eq!(
            &data[8..16],
            &(VA_START + TRIANGLE_BINARY_OFFSET).to_le_bytes()
        );
        assert_eq!(&data[88..96], &(VA_START + PAINT_OFFSET).to_le_bytes());
        let stream = triangle_stream(kumo_mali::CsFeatures::decode(0x7085f)).unwrap();
        assert_eq!(
            stream[5] & 0x0000ffffffffffff,
            VA_START + TRIANGLE_DATA_OFFSET
        );
        assert_eq!(
            stream[6] & 0x0000ffffffffffff,
            VA_START + TRIANGLE_DATA_OFFSET + 32
        );
    }

    #[test]
    fn client_layout_has_distinct_permissions_and_descriptor_pointers_inside_its_bindings() {
        let binds = bindings(7, 11);
        for (i, b) in binds.iter().enumerate() {
            b.validate(BUFFER_BYTES).unwrap();
            assert_eq!((b.vm, b.buffer), (7, 11));
            assert!(binds[i + 1..].iter().all(|other| !b.overlaps(*other)));
        }
        assert_eq!(binds[2].flags.0, F::READ_ONLY);
        assert!(binds[0].flags.contains(F::READ_ONLY | F::NO_EXECUTE));
        assert!(binds[3].flags.contains(F::READ_ONLY | F::NO_EXECUTE));
        assert!(!binds[4].flags.contains(F::READ_ONLY));
        let data = compute_data().unwrap();
        assert_eq!(
            u64::from_le_bytes(data[8..16].try_into().unwrap()),
            binds[2].va
        );
        assert_eq!(
            u64::from_le_bytes(data[88..96].try_into().unwrap()),
            binds[4].va
        );
        assert_eq!(binds[4].offset + binds[4].bytes, BUFFER_BYTES);
        let stream = compute_stream(kumo_mali::CsFeatures::decode(0x7085f)).unwrap();
        assert!((stream.len() * 8) as u64 <= PAGE_BYTES);
        assert_eq!(stream[5] & 0x0000_ffff_ffff_ffff, VA_START + DATA_OFFSET);
        assert_eq!(
            stream[6] & 0x0000_ffff_ffff_ffff,
            VA_START + DATA_OFFSET + 32
        );
        let fau = u64::from(stream[3] as u32) | (u64::from(stream[4] as u32) << 32);
        assert_eq!(fau, 0x0500_0000_2000_3040);
    }

    //j711
    #[test]
    fn churn_frames_fit_the_service_window_and_the_live_buffer_budget() {
        use kumo_gpu::{Binding, MAX_TOTAL_BUFFER_BYTES, VA_END};
        assert_eq!(FRAME_BYTES, 0x2_8000);
        assert_eq!(FRAME_BYTES / PAGE_BYTES, 40);
        assert_eq!(CHURN_VA % PAGE_BYTES, 0);
        // Disjoint from every fixed slot and inside the service window.
        assert!(CHURN_VA >= VA_START + BUFFER_BYTES);
        assert!(CHURN_VA
            .checked_add(FRAME_BYTES)
            .is_some_and(|end| end <= VA_END));
        // The fixed bring-up buffer plus all 100 counted frames stay inside the budget.
        assert!(100 * FRAME_BYTES + BUFFER_BYTES <= MAX_TOTAL_BUFFER_BYTES);
        Binding {
            vm: 1,
            buffer: 1,
            va: CHURN_VA,
            offset: 0,
            bytes: FRAME_BYTES,
            flags: F(F::NO_EXECUTE | F::UNCACHED),
        }
        .validate(FRAME_BYTES)
        .unwrap();
    }

    //j711
    #[test]
    fn the_churn_rewrite_moves_only_the_paint_output_word() {
        let mut data = compute_data().unwrap();
        let original = data;
        assert_eq!(
            &original[PAINT_OUTPUT_VA_WORD..PAINT_OUTPUT_VA_WORD + 8],
            &(VA_START + PAINT_OFFSET).to_le_bytes()
        );
        retarget_paint(&mut data, CHURN_VA).unwrap();
        assert_eq!(
            &data[PAINT_OUTPUT_VA_WORD..PAINT_OUTPUT_VA_WORD + 8],
            &CHURN_VA.to_le_bytes()
        );
        assert_eq!(
            &data[..PAINT_OUTPUT_VA_WORD],
            &original[..PAINT_OUTPUT_VA_WORD]
        );
        assert_eq!(
            &data[PAINT_OUTPUT_VA_WORD + 8..],
            &original[PAINT_OUTPUT_VA_WORD + 8..]
        );
        // The shader program still names the fixed binary page.
        assert_eq!(&data[8..16], &(VA_START + BINARY_OFFSET).to_le_bytes());
        // Misaligned targets are refused and leave the descriptor untouched.
        let before = data;
        assert!(retarget_paint(&mut data, 2).is_err());
        assert_eq!(data, before);
    }
}
