#![no_std]
//j596

//! Shared Mali-G610 CSF instructions, vendored compute kernel and descriptor packing. Both
//! drv-gpu's boot proof and the separate client use these same packers. This library owns
//! no MMIO, mappings, queue, session or synchronization objects. — TIMBERDOODLE

const PAGE_SIZE: u64 = 4096;

/// CS_FEATURES (§9.6): what a wrapper is built against. Metal reports `0x0007085f`: 96
/// registers, 8 scoreboard slots.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CsFeatures {
    pub work_regs: u32,
    pub scoreboards: u32,
}

impl CsFeatures {
    pub const fn decode(raw: u32) -> Self {
        Self {
            work_regs: (raw & 0xFF) + 1,
            scoreboards: (raw >> 8) & 0xFF,
        }
    }
}

/// MOVE48: a 48-bit immediate into a register pair (§10.3).
pub fn move48(destination_pair: u8, address: u64) -> u64 {
    (1u64 << 56) | (u64::from(destination_pair) << 48) | (address & 0x0000_FFFF_FFFF_FFFF)
}

/// MOVE32: a 32-bit immediate into one register (§10.3).
pub fn move32(destination: u8, value: u32) -> u64 {
    (2u64 << 56) | (u64::from(destination) << 48) | u64::from(value)
}

/// FLUSH_CACHE2 with explicit modes: L2 [3:0], LSC [7:4], other [11:8] (§10.3-§10.4).
pub fn flush_caches(signal_slot: u8, flush_id_reg: u8, modes: u16) -> u64 {
    (36u64 << 56)
        | (u64::from(signal_slot) << 48)
        | (u64::from(flush_id_reg) << 40)
        | u64::from(modes & 0x0FFF)
}

/// WAIT: block until every scoreboard in the mask has signalled (§10.3).
pub fn wait_slot(mask: u16) -> u64 {
    (3u64 << 56) | (u64::from(mask) << 16)
}

/// FLUSH_CACHE2's mode word for L2 clean and LSC clean, other untouched: panvk's end-of-stream
/// flush (`finish_cs`), enough to make the stream's writes visible (§10.4, §13).
pub const FLUSH_CLEAN: u16 = 0x011;

/// The pattern fills one DESIGN/019 view: linear XRGB8888 (`0x00RRGGBB`), stride = width.
pub const PAINT_WIDTH: u32 = 256;
pub const PAINT_HEIGHT: u32 = 128;
pub const PAINT_BYTES: u64 = PAINT_WIDTH as u64 * PAINT_HEIGHT as u64 * 4;
/// gfx-demo's channel-order bars: red, green, blue, white.
pub const PAINT_BARS: [u32; 4] = [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0x00FF_FFFF];
/// The CPU reference. The top half is four 64-pixel bars; the bottom half is a ramp, blue
/// following x and green following twice the row.
pub const fn paint_pixel(x: u32, y: u32) -> u32 {
    if y < PAINT_HEIGHT / 2 {
        PAINT_BARS[(x / (PAINT_WIDTH / PAINT_BARS.len() as u32)) as usize]
    } else {
        (((y - PAINT_HEIGHT / 2) * 2) << 8) | x
    }
}

/// The comparison of a painted surface against [`paint_pixel`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PaintCheck {
    pub mismatches: u32,
    /// The first wrong pixel as (x, y, got, want).
    pub first: Option<(u32, u32, u32, u32)>,
    /// FNV-1a over the surface's pixels, row-major, little-endian.
    pub checksum: u32,
}

fn fnv1a(hash: u32, pixel: u32) -> u32 {
    pixel.to_le_bytes().iter().fold(hash, |h, byte| {
        (h ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
    })
}

/// Compare every pixel `read(x, y)` returns against the reference. The caller reads the
/// surface with volatile loads; this never sees GPU memory itself.
pub fn check_paint(mut read: impl FnMut(u32, u32) -> u32) -> PaintCheck {
    let mut check = PaintCheck {
        checksum: 0x811C_9DC5,
        ..PaintCheck::default()
    };
    for y in 0..PAINT_HEIGHT {
        for x in 0..PAINT_WIDTH {
            let got = read(x, y);
            let want = paint_pixel(x, y);
            if got != want {
                check.mismatches += 1;
                check.first.get_or_insert((x, y, got, want));
            }
            check.checksum = fnv1a(check.checksum, got);
        }
    }
    check
}

/// The reference image's checksum, for the boot log beside the measured one.
pub fn paint_reference_checksum() -> u32 {
    check_paint(paint_pixel).checksum
}

// ---- G5c: the offline-compiled compute kernel (shaders/mali, PLAN/013 decision 2 option A) ----

/// What Mesa's compiler reported for one vendored kernel. A host test checks every field
/// against the kernel's `.manifest`, which `scripts/build-mali-shaders.sh` writes.
#[derive(Clone, Copy, Debug)]
pub struct ComputeKernel {
    pub binary: &'static [u8],
    pub local_size: [u32; 3],
    pub work_reg_count: u32,
    /// Registers the hardware preloads (bit n = rn); r60/r61 carry the global x/y.
    pub preload: u64,
    pub tls_size: u32,
    pub wls_size: u32,
    pub allow_merging_workgroups: bool,
    pub ftz_fp16: bool,
    pub ftz_fp32: bool,
    /// The kernel's arguments start after Mesa's precompiled-kernel sysvals (§12.9).
    pub args_offset: u32,
    pub args_bytes: u32,
}

/// `shaders/mali/kumo_paint.cl` compiled for arch 10: the G5a pattern as a compute kernel, one
/// invocation per pixel. Arguments: `out` (u64) at +0 and `stride` (u32) at +8.
pub const KUMO_PAINT_V10: ComputeKernel = ComputeKernel {
    binary: include_bytes!("../../../shaders/mali/kumo_paint.v10.bin"),
    local_size: [16, 16, 1],
    work_reg_count: 32,
    preload: 0x3000_0000_0000_0000,
    tls_size: 0,
    wls_size: 0,
    allow_merging_workgroups: false,
    ftz_fp16: false,
    ftz_fp32: false,
    args_offset: 24,
    args_bytes: 12,
};

/// Mesa's precompiled-kernel sysvals, the first 24 bytes of the push data: the workgroup counts
/// x, y, z, padding, then a printf buffer address (`bifrost_precompiled_kernel_sysvals`).
pub const KERNEL_SYSVALS_BYTES: u32 = 24;
/// panvk uploads shader code 128-byte aligned (`panvk_pool_upload_aligned(..., 128)`).
pub const SHADER_BINARY_ALIGN: u64 = 128;
/// The manifest's reference address: the GPU reads exactly the SPD pinned against Mesa.
pub const KUMO_PAINT_BINARY_VA: u64 = 0x1004_0000;
/// A separate read-only, no-execute page: SPD, TSD, then the 40-byte FAU push data.
pub const KUMO_PAINT_DATA_VA: u64 = KUMO_PAINT_BINARY_VA + PAGE_SIZE;
pub const KUMO_PAINT_TSD_VA: u64 = KUMO_PAINT_DATA_VA + 32;
pub const KUMO_PAINT_PUSH_VA: u64 = KUMO_PAINT_DATA_VA + 64;
pub const KUMO_PAINT_WORKGROUPS: [u32; 3] = [
    PAINT_WIDTH / KUMO_PAINT_V10.local_size[0],
    PAINT_HEIGHT / KUMO_PAINT_V10.local_size[1],
    1,
];
const _: () = assert!(KUMO_PAINT_V10.binary.len() as u64 <= PAGE_SIZE);
const _: () = assert!(PAINT_WIDTH % KUMO_PAINT_V10.local_size[0] == 0);
const _: () = assert!(PAINT_HEIGHT % KUMO_PAINT_V10.local_size[1] == 0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelError {
    /// Only the "preserve subnormals" flush-to-zero mode has been pinned against Mesa.
    FlushToZero,
    /// Only kernels without thread-local or workgroup memory are supported so far.
    LocalStorage { tls_size: u32, wls_size: u32 },
    /// A workgroup dimension outside 1-1024.
    WorkgroupSize([u32; 3]),
    /// The binary's GPU address is not 128-byte aligned or wider than 48 bits.
    BinaryAddress(u64),
    /// The push data is not 8-byte aligned, empty, or more than 255 FAU slots.
    PushData { va: u64, bytes: u32 },
    /// The fixed compute registers plus wrapper scratch, and separate endpoint/flush slots.
    DispatchFeatures(CsFeatures),
    /// The descriptor block is not 32-byte aligned or does not fit a 48-bit GPU range.
    DescriptorAddress(u64),
    /// The pixel target is not word aligned or the image crosses the 48-bit GPU limit.
    OutputAddress(u64),
}

/// RESEARCH/009 §12.2's Shader Program descriptor for a compute kernel, packed the way panvk's
/// `panvk_shader_upload` fills it: type 8, stage compute (1), 32 or 64 registers per thread
/// (`pan_register_allocation`: 32 when `work_reg_count <= 32`), the r48-r63 preload window, and
/// the binary's address. Pinned byte for byte to Mesa's `pan_pack` output in the manifest.
/// — TIMBERDOODLE
pub fn shader_program_descriptor(
    kernel: &ComputeKernel,
    binary_va: u64,
) -> Result<[u32; 8], KernelError> {
    if kernel.ftz_fp16 || kernel.ftz_fp32 {
        return Err(KernelError::FlushToZero);
    }
    if binary_va % SHADER_BINARY_ALIGN != 0 || binary_va > 0x0000_FFFF_FFFF_FFFF {
        return Err(KernelError::BinaryAddress(binary_va));
    }
    let register_allocation = if kernel.work_reg_count <= 32 { 2 } else { 0 };
    let mut words = [0u32; 8];
    words[0] = 8 | (1 << 4) | (register_allocation << 30);
    words[1] = ((kernel.preload >> 48) & 0xFFFF) as u32;
    words[2] = binary_va as u32;
    words[3] = (binary_va >> 32) as u32;
    Ok(words)
}

/// RESEARCH/009 §12.3's Local Storage descriptor for a kernel with no TLS and no WLS, as Mesa's
/// `pan_emit_tls` packs it: everything zero except the "no workgroup memory" WLS instance count
/// (log2 field `0x1F`).
pub fn local_storage_descriptor(kernel: &ComputeKernel) -> Result<[u32; 8], KernelError> {
    if kernel.tls_size != 0 || kernel.wls_size != 0 {
        return Err(KernelError::LocalStorage {
            tls_size: kernel.tls_size,
            wls_size: kernel.wls_size,
        });
    }
    let mut words = [0u32; 8];
    words[1] = 0x1F;
    Ok(words)
}

/// RESEARCH/009 §12.6's workgroup-size word for r33: each dimension minus one in 10 bits, and
/// bit 31 when the compiler allows merging workgroups.
pub fn workgroup_size_word(kernel: &ComputeKernel) -> Result<u32, KernelError> {
    let [x, y, z] = kernel.local_size;
    if [x, y, z].iter().any(|d| !(1..=1024).contains(d)) {
        return Err(KernelError::WorkgroupSize(kernel.local_size));
    }
    Ok((x - 1)
        | ((y - 1) << 10)
        | ((z - 1) << 20)
        | (u32::from(kernel.allow_merging_workgroups) << 31))
}

/// The FAU register pair's value: the push data's GPU address with its size in 8-byte slots in
/// bits [63:56] (panvk's `push_uniforms.gpu | (fau_count << 56)`).
pub fn fau_pointer(push_va: u64, push_bytes: u32) -> Result<u64, KernelError> {
    let slots = u64::from(push_bytes).div_ceil(8);
    if push_va % 8 != 0 || push_va > 0x00FF_FFFF_FFFF_FFFF || slots == 0 || slots > 255 {
        return Err(KernelError::PushData {
            va: push_va,
            bytes: push_bytes,
        });
    }
    Ok(push_va | (slots << 56))
}

/// The push data for one `kumo_paint` dispatch: the sysvals (workgroup counts, no printf
/// buffer), then `out` and `stride`, padded to whole 8-byte FAU slots.
pub fn kumo_paint_push(workgroups: [u32; 3], out: u64, stride: u32) -> [u8; 40] {
    let mut push = [0u8; 40];
    for (index, count) in workgroups.iter().enumerate() {
        push[index * 4..index * 4 + 4].copy_from_slice(&count.to_le_bytes());
    }
    let args = KUMO_PAINT_V10.args_offset as usize;
    push[args..args + 8].copy_from_slice(&out.to_le_bytes());
    push[args + 8..args + 12].copy_from_slice(&stride.to_le_bytes());
    push
}

/// One page's initialized bytes: Mesa-pinned SPD and TSD followed by the precompiled ABI's
/// sysvals/arguments. The rest of the allocation is zeroed by the caller.
pub fn kumo_paint_data(out: u64) -> Result<[u8; 104], KernelError> {
    kumo_paint_data_at(KUMO_PAINT_BINARY_VA, out)
}

/// The same pinned descriptor layout at a caller-owned binary/target address. Only the
/// binary pointer and output argument differ from the boot proof. — TIMBERDOODLE
pub fn kumo_paint_data_at(binary_va: u64, out: u64) -> Result<[u8; 104], KernelError> {
    if out % 4 != 0
        || out
            .checked_add(PAINT_BYTES)
            .is_none_or(|end| end > (1u64 << 48))
    {
        return Err(KernelError::OutputAddress(out));
    }
    let mut data = [0u8; 104];
    for (index, word) in shader_program_descriptor(&KUMO_PAINT_V10, binary_va)?
        .iter()
        .chain(local_storage_descriptor(&KUMO_PAINT_V10)?.iter())
        .enumerate()
    {
        data[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    data[64..].copy_from_slice(&kumo_paint_push(KUMO_PAINT_WORKGROUPS, out, PAINT_WIDTH));
    Ok(data)
}

/// G5c-2, Mesa v10.xml and panvk's dispatch_precomp: select resource set 0, task increment 1
/// on X. Use endpoint scoreboard 1 and other scoreboard 0, then wait before the unconditional
/// L2/LSC clean; the wrapper must never publish completion ahead of the shader's writes.
/// MOVE48 would discard the FAU count in bits 63:56, so its pair uses two MOVE32s.
/// — TIMBERDOODLE
pub fn kumo_paint_stream(features: CsFeatures) -> Result<[u64; 24], KernelError> {
    kumo_paint_stream_at(features, KUMO_PAINT_DATA_VA)
}

/// A dispatch using the caller's descriptor block (SPD +0, TSD +32, push +64). This is shared
/// by the privileged boot proof and the unprivileged protocol client. — TIMBERDOODLE
pub fn kumo_paint_stream_at(features: CsFeatures, data_va: u64) -> Result<[u64; 24], KernelError> {
    if !(44..=256).contains(&features.work_regs) || !(2..=16).contains(&features.scoreboards) {
        return Err(KernelError::DispatchFeatures(features));
    }
    if data_va % 32 != 0
        || data_va
            .checked_add(104)
            .is_none_or(|end| end > (1u64 << 48))
    {
        return Err(KernelError::DescriptorAddress(data_va));
    }
    let fau = fau_pointer(data_va + 64, 40)?;
    let [x, y, z] = KUMO_PAINT_WORKGROUPS;
    let wait_all = ((1u32 << features.scoreboards) - 1) as u16;
    Ok([
        (34u64 << 56) | 1, // REQ_RESOURCE compute
        (23u64 << 56) | 1, // SET_SB_ENTRY endpoint=1, other=0
        move48(0, 0),
        move32(8, fau as u32),
        move32(9, (fau >> 32) as u32),
        move48(16, data_va),
        move48(24, data_va + 32),
        move32(32, 0),
        move32(33, workgroup_size_word(&KUMO_PAINT_V10)?),
        move32(34, 0),
        move32(35, 0),
        move32(36, 0),
        move32(37, x),
        move32(38, y),
        move32(39, z),
        (4u64 << 56) | 1, // RUN_COMPUTE task increment=1, X, resource set 0
        wait_slot(wait_all),
        move32(40, 0),
        flush_caches(0, 40, FLUSH_CLEAN),
        wait_slot(1),
        34u64 << 56, // release resources after completion and cache clean
        0,
        0,
        0,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    const METAL: CsFeatures = CsFeatures::decode(0x7085f);

    #[test]
    fn relocation_changes_only_the_shader_and_output_addresses_in_the_upload() {
        let mut reference = kumo_paint_data(0x1010_0000).unwrap();
        let relocated = kumo_paint_data_at(0x2000_2000, 0x2001_0000).unwrap();
        reference[8..16].copy_from_slice(&0x2000_2000u64.to_le_bytes());
        reference[88..96].copy_from_slice(&0x2001_0000u64.to_le_bytes());
        assert_eq!(relocated, reference);
        assert_eq!(&relocated[64..76], &[16, 0, 0, 0, 8, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(paint_reference_checksum(), 0x8720_cbc5);
    }

    #[test]
    fn relocated_dispatch_preserves_geometry_and_the_full_fau_count() {
        assert_eq!(
            kumo_paint_stream(METAL).unwrap(),
            kumo_paint_stream_at(METAL, KUMO_PAINT_DATA_VA).unwrap()
        );
        let stream = kumo_paint_stream_at(METAL, 0x2000_3000).unwrap();
        let mut regs = [u32::MAX; 96];
        for word in &stream[..15] {
            let dst = ((word >> 48) & 0xff) as usize;
            match word >> 56 {
                1 => {
                    regs[dst] = *word as u32;
                    regs[dst + 1] = ((word >> 32) & 0xffff) as u32;
                }
                2 => regs[dst] = *word as u32,
                23 | 34 => (),
                _ => panic!("unexpected prelude opcode"),
            }
        }
        let pair = |r: usize| u64::from(regs[r]) | (u64::from(regs[r + 1]) << 32);
        assert_eq!(pair(0), 0);
        assert_eq!(pair(8), 0x0500_0000_2000_3040);
        assert_eq!(pair(16), 0x2000_3000);
        assert_eq!(pair(24), 0x2000_3020);
        assert_eq!(&regs[32..40], &[0, 0x3c0f, 0, 0, 0, 16, 8, 1]);
        assert_eq!(stream[15], 0x0400_0000_0000_0001);
        assert_eq!(stream[16], 0x0300_0000_00ff_0000);
    }

    #[test]
    fn relocation_refuses_misalignment_overflow_and_unsupported_capabilities() {
        for address in [0x2000_3004, (1u64 << 48) - 32, u64::MAX - 31] {
            assert_eq!(
                kumo_paint_stream_at(METAL, address),
                Err(KernelError::DescriptorAddress(address))
            );
        }
        for address in [0x2001_0001, (1u64 << 48) - 4, u64::MAX - 3] {
            assert_eq!(
                kumo_paint_data_at(0x2000_2000, address),
                Err(KernelError::OutputAddress(address))
            );
        }
        assert_eq!(
            kumo_paint_data_at(0x2000_2001, 0x2001_0000),
            Err(KernelError::BinaryAddress(0x2000_2001))
        );
        let limited = CsFeatures {
            work_regs: 40,
            scoreboards: 1,
        };
        assert_eq!(
            kumo_paint_stream_at(limited, 0x2000_3000),
            Err(KernelError::DispatchFeatures(limited))
        );
    }
}
