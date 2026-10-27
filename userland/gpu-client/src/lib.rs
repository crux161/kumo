#![no_std]
//j595
//j596
//j637

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
}
