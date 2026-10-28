//j643
//j644
//j645
//j646
//j647
//j650

//! Compile/link acceptance, with the real Session codec and modeled hardware only.
//! The native service and Mesa rendering remain later acceptance gates. — KESTREL
#![no_std]

use core::cell::Cell;
use kumo_gpu::{
    c::{with_api, Api, Info, DEVICE_LOST, PROTOCOL, SERVER_ERROR},
    client::{Client, Transport, TransportError},
    session::{Backend, Session, SubmitError},
    Binding, DeviceCore, DeviceInfo, DeviceMmu, DeviceTexture, DeviceTiler, DeviceTopology, Reply,
    Request, Status, Submission, REPLY_LEN, REQUEST_LEN,
};

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[repr(C)]
#[derive(Default)]
struct Report {
    info: Info,
    missing_properties: u64,
    query_status: u32,
    reserved: u32,
    core_count: u32,
    core_id_range: u32,
    va_range_start: u64,
    va_range_size: u64,
    tiler_bin_size: u32,
    tiler_max_levels: u32,
    compressed_formats: u32,
    has_afrc: u32,
    gpu_variant: u32,
    model_resolved: u32,
}
extern "C" {
    fn kumo_mesa_probe(api: *const Api, out: *mut Report) -> u32;
}
const _: () = assert!(core::mem::size_of::<Report>() == 120);
const _: () = assert!(core::mem::size_of::<Api>() == 24);
const INFO: DeviceInfo = DeviceInfo {
    gpu_id: 0xa8670005,
    cs_features: 0x7085f,
};
/// What the Orange Pi attests: MMU_FEATURES 0x2830, whose [7:0] field is the 48-bit
/// virtual-address width upstream then turns into a 1<<48 user range. — SAKER
const MMU_FEATURES: u32 = 0x2830;
const MMU_VA_BITS: u32 = 48;
const MMU_VA_SIZE: u64 = 1 << 48;
/// RESEARCH/009's predicted G610 word: `[4:0]` log2 bin size 9 and `[11:8]` 8 levels, so
/// upstream should derive a 512-byte bin and 8 levels. The board measures the real value. — SAKER
const TILER_FEATURES: u32 = 0x809;
const TILER_BIN: u32 = 512;
const TILER_LEVELS: u32 = 8;
/// Bit 1 is the tiler group, so it clears once TILER_FEATURES is attested.
/// A modeled word with bit 25 set so upstream's AFRC branch is actually exercised. The board
/// attests 0xc1ffff9e, which clears that bit, so the measured word is covered separately in the
/// matrix below rather than being assumed here. — SAKER
const TEXTURE_FEATURES_0: u32 = 0x0200_0001;
/// Measured on the Orange Pi 5 Plus in J647; RESEARCH/009 §15 had only ever said "measure".
const TEXTURE_FEATURES_0_MEASURED: u32 = 0xc1ff_ff9e;
/// RESEARCH/009 §15 has never predicted a CORE_FEATURES word either, and J647 warns the variant
/// byte is unrecorded. The modeled word uses variant 0 — the pinned Mesa table's requirement for
/// its G610 entry to resolve — plus an unrelated bit above the byte so nothing on the KUMO side
/// selects [7:0]. The board's real word is measured on metal and pinned separately. — TIMBERDOODLE
const CORE_FEATURES: u32 = 0x0000_1000;
/// Measured on the Orange Pi 5 Plus in J650: CORE_FEATURES reads exactly zero, and variant
/// byte 0 is the pinned table's G610 entry, so the measured word is the resolving one.
/// A zero word is a valid attestation, not a failed read. — TIMBERDOODLE
const CORE_FEATURES_MEASURED: u32 = 0x0000_0000;
const MISSING_ALL: u64 = 1023;
const MISSING_WITH_TILER: u64 = MISSING_ALL & !(1 << 1);
/// Core features are required, so bit 0 clears on every successful query, whether or not the
/// derived variant lands in Mesa's model table. — TIMBERDOODLE
const MISSING_WITH_VARIANT: u64 = MISSING_ALL & !(1 << 0);
const MISSING_WITH_VARIANT_AND_TILER: u64 = MISSING_WITH_TILER & !(1 << 0);

struct Hardware {
    busy_once: bool,
    shader_present: u64,
    mmu_features: u32,
    tiler_features: u32,
    texture_features_0: u32,
    core_features: u32,
}
impl Backend for Hardware {
    fn info(&mut self) -> Result<DeviceInfo, Status> {
        if core::mem::replace(&mut self.busy_once, false) {
            Err(Status::Busy)
        } else {
            Ok(INFO)
        }
    }
    fn topology(&mut self) -> Result<DeviceTopology, Status> {
        Ok(DeviceTopology {
            shader_present: self.shader_present,
        })
    }
    fn mmu_features(&mut self) -> Result<DeviceMmu, Status> {
        Ok(DeviceMmu {
            mmu_features: self.mmu_features,
        })
    }
    fn tiler_features(&mut self) -> Result<DeviceTiler, Status> {
        // Mirrors the driver: an absent word degrades rather than retiring the session. — SAKER
        if self.tiler_features == 0 {
            return Err(Status::NotSupported);
        }
        Ok(DeviceTiler {
            tiler_features: self.tiler_features,
        })
    }
    fn texture_features(&mut self) -> Result<DeviceTexture, Status> {
        if self.texture_features_0 == 0 {
            return Err(Status::NotSupported);
        }
        Ok(DeviceTexture {
            texture_features_0: self.texture_features_0,
        })
    }
    fn core_features(&mut self) -> Result<DeviceCore, Status> {
        // Mirrors the driver: the word is required, and the session maps a zero attestation
        // to DeviceLost rather than degrading. — TIMBERDOODLE
        Ok(DeviceCore {
            core_features: self.core_features,
        })
    }
    fn create_vm(&mut self) -> Result<(), Status> {
        Err(Status::NotSupported)
    }
    fn create_buffer(&mut self, _: u32, _: u64) -> Result<(), Status> {
        Err(Status::NotSupported)
    }
    fn bind(&mut self, _: Binding) -> Result<(), Status> {
        Err(Status::NotSupported)
    }
    fn create_group(&mut self) -> Result<(), Status> {
        Err(Status::NotSupported)
    }
    fn submit(&mut self, _: Submission) -> Result<(), SubmitError> {
        Err(SubmitError::Unpublished(Status::NotSupported))
    }
}
struct Loopback<'a> {
    session: Session,
    hardware: Hardware,
    reply: [u8; REPLY_LEN],
    sends: &'a Cell<u32>,
    closed: &'a Cell<u32>,
    rejected_grant: bool,
}
impl Transport for Loopback<'_> {
    type Handle = u32;
    fn send(&mut self, bytes: &[u8; REQUEST_LEN]) -> Result<(), TransportError> {
        let request = Request::decode(bytes).ok_or(TransportError::Failed)?;
        let expected = self.sends.get() + 1;
        if request.id != expected {
            return Err(TransportError::Failed);
        }
        self.sends.set(expected);
        self.reply = Reply {
            id: request.id,
            result: self.session.dispatch(request, &mut self.hardware),
        }
        .encode();
        Ok(())
    }
    fn receive(
        &mut self,
        bytes: &mut [u8; REPLY_LEN + 1],
    ) -> Result<(usize, Option<u32>), TransportError> {
        bytes[..REPLY_LEN].copy_from_slice(&self.reply);
        Ok((REPLY_LEN, self.rejected_grant.then_some(77)))
    }
    fn close_handle(&mut self, handle: u32) {
        if handle == 77 {
            self.closed.set(self.closed.get() + 1);
        }
    }
}
fn transport<'a>(
    sends: &'a Cell<u32>,
    closed: &'a Cell<u32>,
    busy_once: bool,
    rejected_grant: bool,
) -> Loopback<'a> {
    Loopback {
        session: Session::default(),
        hardware: Hardware {
            busy_once,
            shader_present: 0x500000005,
            mmu_features: MMU_FEATURES,
            tiler_features: TILER_FEATURES,
            texture_features_0: TEXTURE_FEATURES_0,
            core_features: CORE_FEATURES,
        },
        reply: [0; REPLY_LEN],
        sends,
        closed,
        rejected_grant,
    }
}

#[no_mangle]
pub extern "C" fn kumo_mesa_bridge_proof() -> u32 {
    let sends = Cell::new(0);
    let closed = Cell::new(0);
    let mut client = Client::new(transport(&sends, &closed, true, false));
    let result = with_api(&mut client, |api| {
        let mut report = Report::default();
        // A rejected old ABI must not write the larger version4 report into an
        // old caller's allocation. The extra sentinel words are guard storage, and they
        // span more than the current report so an overrun is observed rather than
        // allowed to smash the stack. — KESTREL, guard widened for v3 — SAKER
        let old_api = Api { version: 4, ..*api };
        let mut guarded = [0xfeedbeefcafebabeu64; 16];
        if unsafe { kumo_mesa_probe(&old_api, guarded.as_mut_ptr().cast()) } != 2
            || guarded != [0xfeedbeefcafebabeu64; 16]
            || sends.get() != 0
        {
            return 11;
        }
        // The C-side version/size/null checks must perform no query I/O. — KESTREL
        for bad in [
            Api { version: 4, ..*api },
            Api {
                bytes: api.bytes - 1,
                ..*api
            },
            Api {
                query: None,
                ..*api
            },
            Api {
                context: core::ptr::null_mut(),
                ..*api
            },
        ] {
            if unsafe { kumo_mesa_probe(&bad, &mut report) } != 2 || sends.get() != 0 {
                return 1;
            }
        }
        if unsafe { kumo_mesa_probe(core::ptr::null(), &mut report) } != 2
            || unsafe { kumo_mesa_probe(api, core::ptr::null_mut()) } != 2
            || sends.get() != 0
        {
            return 2;
        }
        if unsafe { kumo_mesa_probe(api, &mut report) } != 3
            || report.query_status != SERVER_ERROR + Status::Busy as u32
            || report.info.gpu_id != 0
            || sends.get() != 1
        {
            return 3;
        }
        for n in [7, 13] {
            if unsafe { kumo_mesa_probe(api, &mut report) } != 1
                || report.info.gpu_id != INFO.gpu_id
                || report.info.cs_features != INFO.cs_features
                || report.info.va_start != kumo_gpu::VA_START
                || report.info.va_end != kumo_gpu::VA_END
                || report.info.max_buffer_bytes != kumo_gpu::MAX_BUFFER_BYTES
                || report.info.shader_present != 0x500000005
                || report.info.mmu_features != MMU_FEATURES
                || report.info.tiler_features != TILER_FEATURES
                || report.info.texture_features_0 != TEXTURE_FEATURES_0
                || report.info.core_features != CORE_FEATURES
                || report.compressed_formats != TEXTURE_FEATURES_0
                || report.has_afrc != 1
                || report.core_count != 4
                || report.core_id_range != 35
                || report.va_range_start != 0
                || report.va_range_size != MMU_VA_SIZE
                || report.tiler_bin_size != TILER_BIN
                || report.tiler_max_levels != TILER_LEVELS
                || report.gpu_variant != (CORE_FEATURES & 0xff)
                || report.model_resolved != 1
                || report.missing_properties != MISSING_WITH_VARIANT_AND_TILER
                || report.query_status != 0
                || report.reserved != 0
                || sends.get() != n
            {
                return 4;
            }
        }
        0
    });
    if result != 0 {
        return result;
    }
    if client.is_lost() || client.info() != Ok(INFO) || sends.get() != 14 || closed.get() != 0 {
        return 5;
    }

    for (mask, count, range) in [
        (1, 1, 1),
        (0xf, 4, 4),
        (0x8000000000000001, 2, 64),
        (u64::MAX, 64, 64),
    ] {
        sends.set(0);
        let mut wire = transport(&sends, &closed, false, false);
        wire.hardware.shader_present = mask;
        let mut client = Client::new(wire);
        let ok = with_api(&mut client, |api| {
            let mut report = Report::default();
            (unsafe { kumo_mesa_probe(api, &mut report) }) == 1
                && report.info.shader_present == mask
                && report.core_count == count
                && report.core_id_range == range
                && report.va_range_size == MMU_VA_SIZE
                && sends.get() == 6
        });
        if !ok {
            return 9;
        }
    }

    // Upstream derives the range; KUMO only attests the word. These cases pin that
    // derivation, prove only [7:0] selects the width, and prove that a range which cannot
    // hold this service's window — or a width that would make Mesa's own shift undefined —
    // is refused with nothing published, rather than clamped. — SAKER
    for (features, va_bits, size, code) in [
        (MMU_FEATURES, MMU_VA_BITS, MMU_VA_SIZE, 1u32),
        (0x0000_0030u32, 48u32, MMU_VA_SIZE, 1u32),
        (0xffff_ff20u32, 32u32, 1u64 << 32, 1u32),
        (0x2818u32, 24u32, 1u64 << 24, 4u32),
        (0x2800u32, 0u32, 1u64, 4u32),
        (0x2840u32, 64u32, 0u64, 4u32),
    ] {
        sends.set(0);
        let mut wire = transport(&sends, &closed, false, false);
        wire.hardware.mmu_features = features;
        let mut client = Client::new(wire);
        let ok = with_api(&mut client, |api| {
            let mut report = Report::default();
            let status = unsafe { kumo_mesa_probe(api, &mut report) };
            if status != code || sends.get() != 6 {
                return false;
            }
            if code == 1 {
                (report.info.mmu_features & 0xff) == va_bits
                    && report.va_range_start == 0
                    && report.va_range_size == size
            } else {
                report.info.gpu_id == 0
                    && report.info.mmu_features == 0
                    && report.info.core_features == 0
                    && report.gpu_variant == 0
                    && report.model_resolved == 0
                    && report.core_count == 0
                    && report.va_range_start == 0
                    && report.va_range_size == 0
            }
        });
        if !ok {
            return 12;
        }
    }

    // Upstream decodes the tiler word; KUMO only attests it. These pin the field extraction,
    // that bits above [11:0] are ignored, and that the missing mask tracks reality. — SAKER
    for (features, bin, levels, missing) in [
        (
            TILER_FEATURES,
            TILER_BIN,
            TILER_LEVELS,
            MISSING_WITH_VARIANT_AND_TILER,
        ),
        (0x0014u32, 1u32 << 20, 0u32, MISSING_WITH_VARIANT_AND_TILER),
        (0x0f00u32, 1u32, 15u32, MISSING_WITH_VARIANT_AND_TILER),
        (0xffff_ff00u32, 1u32, 15u32, MISSING_WITH_VARIANT_AND_TILER),
        // A bin-size field of 31 would overflow the signed shift inside upstream's own
        // helper, so the shape degrades to unknown instead of being derived. — SAKER
        (0x001fu32, 0u32, 0u32, MISSING_WITH_VARIANT),
    ] {
        sends.set(0);
        let mut wire = transport(&sends, &closed, false, false);
        wire.hardware.tiler_features = features;
        let mut client = Client::new(wire);
        let ok = with_api(&mut client, |api| {
            let mut report = Report::default();
            (unsafe { kumo_mesa_probe(api, &mut report) }) == 1
                && report.info.tiler_features == features
                && report.tiler_bin_size == bin
                && report.tiler_max_levels == levels
                && report.missing_properties == missing
                && sends.get() == 6
        });
        if !ok {
            return 14;
        }
    }

    // The case the board could actually present, since RESEARCH/009 has never measured this
    // register: an absent word must still yield a successful query with zero shape, the tiler
    // group left missing, every other property intact and the client still live. — SAKER
    sends.set(0);
    let mut wire = transport(&sends, &closed, false, false);
    wire.hardware.tiler_features = 0;
    let mut client = Client::new(wire);
    let ok = with_api(&mut client, |api| {
        let mut report = Report::default();
        (unsafe { kumo_mesa_probe(api, &mut report) }) == 1
            && report.info.tiler_features == 0
            && report.tiler_bin_size == 0
            && report.tiler_max_levels == 0
            && report.missing_properties == MISSING_WITH_VARIANT
            && report.info.mmu_features == MMU_FEATURES
            && report.va_range_size == MMU_VA_SIZE
            && report.core_count == 4
            && report.compressed_formats == TEXTURE_FEATURES_0
            && report.has_afrc == 1
            && report.info.core_features == CORE_FEATURES
            && report.model_resolved == 1
            && sends.get() == 6
    });
    if !ok || client.is_lost() {
        return 15;
    }

    // AFRC is bit 25 behind upstream's own arch test, and compressed_formats is the word
    // verbatim, so both ends of the word matter and neither is reduced early. An absent word
    // still succeeds with both zero, exactly as for tiler features. — SAKER
    for (word, afrc) in [
        (TEXTURE_FEATURES_0, 1u32),
        (TEXTURE_FEATURES_0_MEASURED, 0u32),
        (0x0200_0000u32, 1u32),
        (0x0000_0001u32, 0u32),
        (0x8000_0000u32, 0u32),
        (0xffff_ffffu32, 1u32),
        (0u32, 0u32),
    ] {
        sends.set(0);
        let mut wire = transport(&sends, &closed, false, false);
        wire.hardware.texture_features_0 = word;
        let mut client = Client::new(wire);
        let ok = with_api(&mut client, |api| {
            let mut report = Report::default();
            (unsafe { kumo_mesa_probe(api, &mut report) }) == 1
                && report.info.texture_features_0 == word
                && report.compressed_formats == word
                && report.has_afrc == afrc
                && sends.get() == 6
        });
        if !ok {
            return 16;
        }
    }

    // Upstream derives the variant from bits [7:0] of the attested word and matches
    // (gpu_prod_id, variant) against its static model table; KUMO only attests the word. The
    // modeled word carries variant 0 — the pinned table's requirement for its G610 entry — plus
    // a bit above the byte to prove [7:0] selection stays upstream's. The measured word (zero)
    // is pinned as its own row: variant byte 0 is the G610 entry, so it resolves. A table miss
    // resolves to false without failing the query: the word is attested, only the entry is
    // absent. — TIMBERDOODLE
    for (word, variant, resolved) in [
        (CORE_FEATURES, 0u32, 1u32),
        (CORE_FEATURES_MEASURED, 0u32, 1u32),
        (0x0000_1001u32, 1u32, 0u32),
        (0xffff_ffffu32, 255u32, 0u32),
    ] {
        sends.set(0);
        let mut wire = transport(&sends, &closed, false, false);
        wire.hardware.core_features = word;
        let mut client = Client::new(wire);
        let ok = with_api(&mut client, |api| {
            let mut report = Report::default();
            (unsafe { kumo_mesa_probe(api, &mut report) }) == 1
                && report.info.core_features == word
                && report.gpu_variant == variant
                && report.model_resolved == resolved
                && report.missing_properties == MISSING_WITH_VARIANT_AND_TILER
                && report.info.mmu_features == MMU_FEATURES
                && report.va_range_size == MMU_VA_SIZE
                && sends.get() == 6
        });
        if !ok {
            return 17;
        }
    }

    // A zero MMU attestation retires the session exactly as a zero topology does: C sees
    // the failure with cleared output and cannot issue another request. — SAKER
    sends.set(0);
    let mut wire = transport(&sends, &closed, false, false);
    wire.hardware.mmu_features = 0;
    let mut client = Client::new(wire);
    let ok = with_api(&mut client, |api| {
        let mut report = Report::default();
        (unsafe { kumo_mesa_probe(api, &mut report) }) == 3
            && report.query_status == SERVER_ERROR + Status::DeviceLost as u32
            && report.info.gpu_id == 0
            && report.info.mmu_features == 0
            && report.va_range_size == 0
    });
    if !ok || !client.is_lost() || sends.get() != 3 {
        return 13;
    }

    // Zero is an invalid attestation, not a one-core fallback. The server retires;
    // C sees failure with cleared output and cannot issue another request. — KESTREL
    sends.set(0);
    let mut wire = transport(&sends, &closed, false, false);
    wire.hardware.shader_present = 0;
    let mut client = Client::new(wire);
    let ok = with_api(&mut client, |api| {
        let mut report = Report::default();
        (unsafe { kumo_mesa_probe(api, &mut report) }) == 3
            && report.query_status == SERVER_ERROR + Status::DeviceLost as u32
            && report.info.gpu_id == 0
            && report.info.shader_present == 0
            && report.core_count == 0
            && report.core_id_range == 0
    });
    if !ok || !client.is_lost() || sends.get() != 2 {
        return 10;
    }

    sends.set(0);
    let mut client = Client::new(transport(&sends, &closed, false, true));
    let result = with_api(&mut client, |api| {
        let mut report = Report::default();
        if unsafe { kumo_mesa_probe(api, &mut report) } != 3
            || report.query_status != PROTOCOL
            || report.info.gpu_id != 0
            || sends.get() != 1
            || closed.get() != 1
        {
            return 6;
        }
        if unsafe { kumo_mesa_probe(api, &mut report) } != 3
            || report.query_status != DEVICE_LOST
            || sends.get() != 1
            || closed.get() != 1
        {
            return 7;
        }
        0
    });
    if result != 0 {
        return result;
    }
    if !client.is_lost() {
        return 8;
    }
    0
}
