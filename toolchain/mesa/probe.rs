//j646
//j647
//j650
//j651
//j652

//! Compile/link acceptance, with the real Session codec and modeled hardware only.
//! The native service and Mesa rendering remain later acceptance gates. — KESTREL
#![no_std]

use core::cell::Cell;
use kumo_gpu::{
    c::{with_api, Api, Info, DEVICE_LOST, PROTOCOL, SERVER_ERROR},
    client::{Client, Transport, TransportError},
    session::{Backend, Session, SubmitError},
    Binding, DeviceCore, DeviceInfo, DeviceMmu, DeviceTexture, DeviceThreads, DeviceTiler,
    DeviceTopology, Reply, Request, Status, Submission, REPLY_LEN, REQUEST_LEN,
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
    max_threads_per_core: u32,
    max_threads_per_wg: u32,
    max_tasks_per_core: u32,
    num_registers_per_core: u32,
    max_tls_instance_per_core: u32,
}
extern "C" {
    fn kumo_mesa_probe(api: *const Api, out: *mut Report) -> u32;
}
const _: () = assert!(core::mem::size_of::<Report>() == 168);
// The enlarged 80-byte Info shifts every report field, so the thread block starts at 144,
// not at version6's 120. These pin the C mirror's offsets from the Rust side too.
// — TIMBERDOODLE
const _: () = assert!(core::mem::offset_of!(Report, missing_properties) == 80);
const _: () = assert!(core::mem::offset_of!(Report, model_resolved) == 140);
const _: () = assert!(core::mem::offset_of!(Report, max_threads_per_core) == 144);
const _: () = assert!(core::mem::offset_of!(Report, max_threads_per_wg) == 148);
const _: () = assert!(core::mem::offset_of!(Report, max_tasks_per_core) == 152);
const _: () = assert!(core::mem::offset_of!(Report, num_registers_per_core) == 156);
const _: () = assert!(core::mem::offset_of!(Report, max_tls_instance_per_core) == 160);
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
/// A modeled element 0 word with bit 25 set so upstream's AFRC branch is actually exercised.
/// The board attests 0xc1ffff9e, which clears that bit, so the measured word is covered
/// separately in the matrix below rather than being assumed here. — SAKER
const TEXTURE_FEATURES_0: u32 = 0x0200_0001;
/// Measured on the Orange Pi 5 Plus in J647; RESEARCH/009 §15 had only ever said "measure".
const TEXTURE_FEATURES_0_MEASURED: u32 = 0xc1ff_ff9e;
/// Modeled elements 1..3. They are read from the same powered register block as element 0 and
/// cross verbatim, zero included, so the modeled array carries a zero, a high bit and a
/// full-width word to prove nothing on the KUMO side narrows or decodes them. The board's real
/// values are metal pending. — TIMBERDOODLE
const TEXTURE_FEATURES_1: u32 = 0;
const TEXTURE_FEATURES_2: u32 = 0x8000_0000;
const TEXTURE_FEATURES_3: u32 = 0xffff_ffff;
const TEXTURE_FEATURES: [u32; 4] = [
    TEXTURE_FEATURES_0,
    TEXTURE_FEATURES_1,
    TEXTURE_FEATURES_2,
    TEXTURE_FEATURES_3,
];
/// RESEARCH/009 §15 has never predicted a CORE_FEATURES word either, and J647 warns the variant
/// byte is unrecorded. The modeled word uses variant 0 — the pinned Mesa table's requirement for
/// its G610 entry to resolve — plus an unrelated bit above the byte so nothing on the KUMO side
/// selects [7:0]. The board's real word is measured on metal and pinned separately. — TIMBERDOODLE
const CORE_FEATURES: u32 = 0x0000_1000;
/// Measured on the Orange Pi 5 Plus in J650: CORE_FEATURES reads exactly zero, and variant
/// byte 0 is the pinned table's G610 entry, so the measured word is the resolving one.
/// A zero word is a valid attestation, not a failed read. — TIMBERDOODLE
const CORE_FEATURES_MEASURED: u32 = 0x0000_0000;
/// Modeled thread words shaped like a G610 group: 1024 threads per core and per workgroup,
/// and a THREAD_FEATURES word whose `[21:0]` registers field is 0x0010_0005 and whose
/// `[31:24]` tasks field is 3. RESEARCH/009 §15 has never measured these registers, so the
/// board's real words are metal pending and nothing here is claimed as measured.
/// — TIMBERDOODLE
const THREAD_MAX_THREADS: u32 = 1024;
const THREAD_MAX_WORKGROUP_SIZE: u32 = 1024;
const THREAD_FEATURES: u32 = 0x0310_0005;
const MAX_TASKS_PER_CORE: u32 = THREAD_FEATURES >> 24;
const REGISTERS_PER_CORE: u32 = THREAD_FEATURES & 0x3fffff;
const MISSING_ALL: u64 = 1023;
/// Core features are required, so bit 0 clears on every successful query, whether or not the
/// derived variant lands in Mesa's model table. Bits 3 and 4 clear on the same successful
/// query: G7i attests all four texture words together, and the modeled thread group satisfies
/// upstream's nonzero assumption. Bit 1 clears only when the tiler shape was derived.
/// — TIMBERDOODLE
const MISSING_WITH_VARIANT_AND_TILER_AND_THREADS: u64 =
    MISSING_ALL & !(1 << 0) & !(1 << 1) & !(1 << 3) & !(1 << 4);
/// A tiler word that is absent or cannot be decoded leaves bit 1 set.
const MISSING_WITH_VARIANT_AND_THREADS: u64 = MISSING_ALL & !(1 << 0) & !(1 << 3) & !(1 << 4);
/// A thread group whose derivations contradict upstream's nonzero assumption leaves bit 4 set.
const MISSING_WITH_VARIANT_AND_TILER: u64 = MISSING_ALL & !(1 << 0) & !(1 << 1) & !(1 << 3);
/// An absent texture group (element 0 zero) leaves bit 3 set.
const MISSING_WITH_VARIANT_NO_TEXTURE: u64 = MISSING_ALL & !(1 << 0) & !(1 << 1) & !(1 << 4);

struct Hardware {
    busy_once: bool,
    shader_present: u64,
    mmu_features: u32,
    tiler_features: u32,
    texture_features: [u32; 4],
    core_features: u32,
    max_threads: u32,
    max_workgroup_size: u32,
    thread_features: u32,
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
        // Mirrors the driver: only element 0 is the absence gate, and elements 1..3 cross
        // verbatim, zero included. — TIMBERDOODLE
        if self.texture_features[0] == 0 {
            return Err(Status::NotSupported);
        }
        Ok(DeviceTexture {
            texture_features: self.texture_features,
        })
    }
    fn core_features(&mut self) -> Result<DeviceCore, Status> {
        // Mirrors the driver: the word is required, and the session maps a zero attestation
        // to DeviceLost rather than degrading. — TIMBERDOODLE
        Ok(DeviceCore {
            core_features: self.core_features,
        })
    }
    fn thread_properties(&mut self) -> Result<DeviceThreads, Status> {
        // Mirrors the driver: all three words zero means the group is absent, which the
        // session maps to the recoverable NotSupported status. — TIMBERDOODLE
        if self.max_threads == 0 && self.max_workgroup_size == 0 && self.thread_features == 0 {
            return Err(Status::NotSupported);
        }
        Ok(DeviceThreads {
            max_threads: self.max_threads,
            max_workgroup_size: self.max_workgroup_size,
            thread_features: self.thread_features,
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
            texture_features: TEXTURE_FEATURES,
            core_features: CORE_FEATURES,
            max_threads: THREAD_MAX_THREADS,
            max_workgroup_size: THREAD_MAX_WORKGROUP_SIZE,
            thread_features: THREAD_FEATURES,
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
        // A rejected old ABI must not write the larger version7 report into an
        // old caller's allocation. The extra sentinel words are guard storage, and they
        // span more than the current report so an overrun is observed rather than
        // allowed to smash the stack. — KESTREL, guard widened for v3 — SAKER
        let old_api = Api { version: 7, ..*api };
        let mut guarded = [0xfeedbeefcafebabeu64; 16];
        if unsafe { kumo_mesa_probe(&old_api, guarded.as_mut_ptr().cast()) } != 2
            || guarded != [0xfeedbeefcafebabeu64; 16]
            || sends.get() != 0
        {
            return 11;
        }
        // The C-side version/size/null checks must perform no query I/O. — KESTREL
        for bad in [
            Api { version: 7, ..*api },
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
        for n in [8, 15] {
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
                || report.info.texture_features_1 != TEXTURE_FEATURES_1
                || report.info.texture_features_2 != TEXTURE_FEATURES_2
                || report.info.texture_features_3 != TEXTURE_FEATURES_3
                || report.info.core_features != CORE_FEATURES
                || report.info.thread_max_threads != THREAD_MAX_THREADS
                || report.info.thread_max_workgroup_size != THREAD_MAX_WORKGROUP_SIZE
                || report.info.thread_features != THREAD_FEATURES
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
                || report.max_threads_per_core != THREAD_MAX_THREADS
                || report.max_threads_per_wg != THREAD_MAX_WORKGROUP_SIZE
                || report.max_tasks_per_core != MAX_TASKS_PER_CORE
                || report.num_registers_per_core != REGISTERS_PER_CORE
                || report.max_tls_instance_per_core != THREAD_MAX_THREADS
                || report.missing_properties != MISSING_WITH_VARIANT_AND_TILER_AND_THREADS
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
    if client.is_lost() || client.info() != Ok(INFO) || sends.get() != 16 || closed.get() != 0 {
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
                && sends.get() == 7
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
            if status != code || sends.get() != 7 {
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
            MISSING_WITH_VARIANT_AND_TILER_AND_THREADS,
        ),
        (
            0x0014u32,
            1u32 << 20,
            0u32,
            MISSING_WITH_VARIANT_AND_TILER_AND_THREADS,
        ),
        (
            0x0f00u32,
            1u32,
            15u32,
            MISSING_WITH_VARIANT_AND_TILER_AND_THREADS,
        ),
        (
            0xffff_ff00u32,
            1u32,
            15u32,
            MISSING_WITH_VARIANT_AND_TILER_AND_THREADS,
        ),
        // A bin-size field of 31 would overflow the signed shift inside upstream's own
        // helper, so the shape degrades to unknown instead of being derived. Threads are
        // still attested in this row, so only bits 0, 1 and 4 are clear. — SAKER
        (0x001fu32, 0u32, 0u32, MISSING_WITH_VARIANT_AND_THREADS),
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
                && sends.get() == 7
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
            && report.missing_properties == MISSING_WITH_VARIANT_AND_THREADS
            && report.info.mmu_features == MMU_FEATURES
            && report.va_range_size == MMU_VA_SIZE
            && report.core_count == 4
            && report.compressed_formats == TEXTURE_FEATURES_0
            && report.has_afrc == 1
            && report.info.core_features == CORE_FEATURES
            && report.model_resolved == 1
            && report.max_threads_per_core == THREAD_MAX_THREADS
            && report.max_threads_per_wg == THREAD_MAX_WORKGROUP_SIZE
            && sends.get() == 7
    });
    if !ok || client.is_lost() {
        return 15;
    }

    // AFRC is bit 25 behind upstream's own arch test, and compressed_formats is the word
    // verbatim, so both ends of element 0 matter and neither is reduced early. Elements 1..3
    // are modeled with words that would change both derived fields if anyone decoded them, so
    // this also proves the derivation still reads element 0 only. An absent element 0 still
    // succeeds with both zero and the whole array zero, exactly as for tiler features.
    // — SAKER, TIMBERDOODLE
    for (word, afrc) in [
        (TEXTURE_FEATURES_0, 1u32),
        (TEXTURE_FEATURES_0_MEASURED, 0u32),
        (0x0200_0000u32, 1u32),
        (0x0000_0001u32, 0u32),
        (0x8000_0000u32, 0u32),
        (0xffff_ffffu32, 1u32),
        (0u32, 0u32),
    ] {
        let array = if word == 0 {
            [0; 4]
        } else {
            [word, 0x0200_0000, 0x0000_0001, 0xffff_ffff]
        };
        sends.set(0);
        let mut wire = transport(&sends, &closed, false, false);
        wire.hardware.texture_features = array;
        let mut client = Client::new(wire);
        let ok = with_api(&mut client, |api| {
            let mut report = Report::default();
            (unsafe { kumo_mesa_probe(api, &mut report) }) == 1
                && report.info.texture_features_0 == word
                && report.info.texture_features_1 == array[1]
                && report.info.texture_features_2 == array[2]
                && report.info.texture_features_3 == array[3]
                && report.compressed_formats == word
                && report.has_afrc == afrc
                && report.missing_properties
                    == if word == 0 {
                        MISSING_WITH_VARIANT_NO_TEXTURE
                    } else {
                        MISSING_WITH_VARIANT_AND_TILER_AND_THREADS
                    }
                && sends.get() == 7
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
                && report.missing_properties == MISSING_WITH_VARIANT_AND_TILER_AND_THREADS
                && report.info.mmu_features == MMU_FEATURES
                && report.va_range_size == MMU_VA_SIZE
                && sends.get() == 7
        });
        if !ok {
            return 17;
        }
    }

    // G7h: upstream's thread derivations, except that KUMO never mirrors upstream's nonzero
    // assert. Each row pins the four upstream-asserted fields, the TLS rule (which copies
    // max_threads_per_core) and bit 4. Zero words are an absence the report can state, not a
    // reason to abort, so every row still succeeds and every other property survives.
    // — TIMBERDOODLE
    for (
        max_threads,
        max_workgroup_size,
        thread_features,
        per_core,
        per_wg,
        tasks,
        registers,
        tls,
        missing,
    ) in [
        (
            THREAD_MAX_THREADS,
            THREAD_MAX_WORKGROUP_SIZE,
            THREAD_FEATURES,
            THREAD_MAX_THREADS,
            THREAD_MAX_WORKGROUP_SIZE,
            MAX_TASKS_PER_CORE,
            REGISTERS_PER_CORE,
            THREAD_MAX_THREADS,
            MISSING_WITH_VARIANT_AND_TILER_AND_THREADS,
        ),
        // Every field at its maximum: 255 tasks from the top byte and 0x3fffff registers from
        // the whole low field. This is the derivation boundary, not an error, so bit 4 clears.
        (
            1024,
            1024,
            0xffff_ffff,
            1024,
            1024,
            255,
            0x3fffff,
            1024,
            MISSING_WITH_VARIANT_AND_TILER_AND_THREADS,
        ),
        // Zero tasks with nonzero registers: upstream would assert, because the group is not
        // safely usable. KUMO reports the derivation verbatim and keeps bit 4.
        (
            1024,
            1024,
            0x003f_ffff,
            1024,
            1024,
            0,
            0x3fffff,
            1024,
            MISSING_WITH_VARIANT_AND_TILER,
        ),
        // The inverse contradiction: a full tasks byte and zero registers.
        (
            1024,
            1024,
            0x0300_0000,
            1024,
            1024,
            3,
            0,
            1024,
            MISSING_WITH_VARIANT_AND_TILER,
        ),
        // A zero thread count zeroes the TLS derivation with it, because TLS copies it.
        (
            0,
            1024,
            THREAD_FEATURES,
            0,
            1024,
            MAX_TASKS_PER_CORE,
            REGISTERS_PER_CORE,
            0,
            MISSING_WITH_VARIANT_AND_TILER,
        ),
        (
            1024,
            0,
            THREAD_FEATURES,
            1024,
            0,
            MAX_TASKS_PER_CORE,
            REGISTERS_PER_CORE,
            1024,
            MISSING_WITH_VARIANT_AND_TILER,
        ),
    ] {
        sends.set(0);
        let mut wire = transport(&sends, &closed, false, false);
        wire.hardware.max_threads = max_threads;
        wire.hardware.max_workgroup_size = max_workgroup_size;
        wire.hardware.thread_features = thread_features;
        let mut client = Client::new(wire);
        let ok = with_api(&mut client, |api| {
            let mut report = Report::default();
            (unsafe { kumo_mesa_probe(api, &mut report) }) == 1
                && report.info.thread_max_threads == max_threads
                && report.info.thread_max_workgroup_size == max_workgroup_size
                && report.info.thread_features == thread_features
                && report.max_threads_per_core == per_core
                && report.max_threads_per_wg == per_wg
                && report.max_tasks_per_core == tasks
                && report.num_registers_per_core == registers
                && report.max_tls_instance_per_core == tls
                && report.missing_properties == missing
                && sends.get() == 7
        });
        if !ok || client.is_lost() {
            return 18;
        }
    }

    // The board's plausible case while RESEARCH/009 §15 stays unmeasured: all three words
    // zero means the group is absent. The backend answers NotSupported, the query still
    // succeeds with zero words, bit 4 stays set, and every other property and the client
    // survive. — TIMBERDOODLE
    sends.set(0);
    let mut wire = transport(&sends, &closed, false, false);
    wire.hardware.max_threads = 0;
    wire.hardware.max_workgroup_size = 0;
    wire.hardware.thread_features = 0;
    let mut client = Client::new(wire);
    let ok = with_api(&mut client, |api| {
        let mut report = Report::default();
        (unsafe { kumo_mesa_probe(api, &mut report) }) == 1
            && report.info.thread_max_threads == 0
            && report.info.thread_max_workgroup_size == 0
            && report.info.thread_features == 0
            && report.max_threads_per_core == 0
            && report.max_threads_per_wg == 0
            && report.max_tasks_per_core == 0
            && report.num_registers_per_core == 0
            && report.max_tls_instance_per_core == 0
            && report.missing_properties == MISSING_WITH_VARIANT_AND_TILER
            && report.info.gpu_id == INFO.gpu_id
            && report.info.mmu_features == MMU_FEATURES
            && report.va_range_size == MMU_VA_SIZE
            && report.info.tiler_features == TILER_FEATURES
            && report.tiler_bin_size == TILER_BIN
            && report.info.texture_features_0 == TEXTURE_FEATURES_0
            && report.compressed_formats == TEXTURE_FEATURES_0
            && report.info.core_features == CORE_FEATURES
            && report.model_resolved == 1
            && report.core_count == 4
            && sends.get() == 7
    });
    if !ok || client.is_lost() {
        return 18;
    }

    // A contradicted upstream assumption, stated on its own: the words are nonzero, so the
    // group looks present, but registers per core is zero. Upstream's assert would abort
    // device creation; KUMO publishes every other property, records the zero derivation and
    // keeps bit 4 so no consumer mistakes the group for usable. — TIMBERDOODLE
    sends.set(0);
    let mut wire = transport(&sends, &closed, false, false);
    wire.hardware.thread_features = 0x0300_0000;
    let mut client = Client::new(wire);
    let ok = with_api(&mut client, |api| {
        let mut report = Report::default();
        (unsafe { kumo_mesa_probe(api, &mut report) }) == 1
            && report.info.thread_max_threads == THREAD_MAX_THREADS
            && report.info.thread_max_workgroup_size == THREAD_MAX_WORKGROUP_SIZE
            && report.info.thread_features == 0x0300_0000
            && report.max_threads_per_core == THREAD_MAX_THREADS
            && report.max_threads_per_wg == THREAD_MAX_WORKGROUP_SIZE
            && report.max_tasks_per_core == 3
            && report.num_registers_per_core == 0
            && report.max_tls_instance_per_core == THREAD_MAX_THREADS
            && report.missing_properties == MISSING_WITH_VARIANT_AND_TILER
            && report.info.core_features == CORE_FEATURES
            && report.model_resolved == 1
            && report.va_range_size == MMU_VA_SIZE
            && sends.get() == 7
    });
    if !ok || client.is_lost() {
        return 18;
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

    // Check 19: the whole four-word TEXTURE_FEATURES array crosses, and missing bit 3 tracks
    // whether it was attested. The driver reads all four words in one powered block, so a
    // successful texture query supplies the group; elements 1..3 cross verbatim, zero
    // included, and only element 0 is the absence gate. — TIMBERDOODLE
    for (array, attested) in [
        // Full pass-through: element 1 is a zero the driver really measured, element 2 carries
        // the top bit and element 3 is full width, so nothing narrows or decodes them.
        ([0x0200_0001u32, 0, 0x8000_0000, 0xffff_ffff], true),
        // A nonzero element 0 with every other word zero is still a complete attestation.
        ([0x0000_0001u32, 0, 0, 0], true),
        // Element 0 zero means the group is absent: the query still succeeds, the array is
        // zero, bit 3 stays, and every other property is intact.
        ([0u32, 0xffff_ffff, 0xffff_ffff, 0xffff_ffff], false),
        ([0u32, 0, 0, 0], false),
    ] {
        sends.set(0);
        let mut wire = transport(&sends, &closed, false, false);
        wire.hardware.texture_features = array;
        let mut client = Client::new(wire);
        let ok = with_api(&mut client, |api| {
            let mut report = Report::default();
            let status = unsafe { kumo_mesa_probe(api, &mut report) };
            if status != 1 || sends.get() != 7 {
                return false;
            }
            let words = if attested { array } else { [0; 4] };
            let derived_afrc = if words[0] & (1 << 25) != 0 { 1 } else { 0 };
            let missing = if attested {
                MISSING_WITH_VARIANT_AND_TILER_AND_THREADS
            } else {
                MISSING_WITH_VARIANT_NO_TEXTURE
            };
            report.info.texture_features_0 == words[0]
                && report.info.texture_features_1 == words[1]
                && report.info.texture_features_2 == words[2]
                && report.info.texture_features_3 == words[3]
                && report.compressed_formats == words[0]
                && report.has_afrc == derived_afrc
                && report.missing_properties == missing
                && (report.missing_properties & (1 << 3) != 0) == !attested
                && report.info.gpu_id == INFO.gpu_id
                && report.info.shader_present == 0x500000005
                && report.info.mmu_features == MMU_FEATURES
                && report.va_range_size == MMU_VA_SIZE
                && report.info.tiler_features == TILER_FEATURES
                && report.tiler_bin_size == TILER_BIN
                && report.tiler_max_levels == TILER_LEVELS
                && report.info.core_features == CORE_FEATURES
                && report.gpu_variant == (CORE_FEATURES & 0xff)
                && report.model_resolved == 1
                && report.core_count == 4
        });
        if !ok {
            return 19;
        }
        // An absent texture group is recoverable exactly as an absent tiler word is: the client
        // stays live and one more query costs one more request ID. The merged chain consumes
        // seven IDs per query (the thread query is last), so the follow-up query lands at eight.
        if client.is_lost() || client.info() != Ok(INFO) || sends.get() != 8 {
            return 19;
        }
    }

    // Check 19's version guard: version 7 is refused before any I/O, and this report's own
    // version 8 is the one that is queried. — TIMBERDOODLE
    sends.set(0);
    let mut client = Client::new(transport(&sends, &closed, false, false));
    let ok = with_api(&mut client, |api| {
        let mut report = Report::default();
        let old = Api { version: 7, ..*api };
        (unsafe { kumo_mesa_probe(&old, &mut report) }) == 2
            && sends.get() == 0
            && api.version == 8
            && (unsafe { kumo_mesa_probe(api, &mut report) }) == 1
            && sends.get() == 7
    });
    if !ok || client.is_lost() {
        return 19;
    }
    0
}
