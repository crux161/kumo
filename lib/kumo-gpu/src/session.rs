//j646
//j647
//j650
//j651
//j652

//! Bounded per-channel object table. Commit metadata only after backend success. The backend
//! must retire on any partial map or uncertain publication; it must never free live DMA.

use crate::{
    valid_buffer_bytes, Binding, Command, DeviceCore, DeviceInfo, DeviceMmu, DeviceTexture,
    DeviceThreads, DeviceTiler, DeviceTopology, Request, RequestIds, Response, Status, Submission,
    Timeline, MAX_BUFFER_BYTES,
};

pub const MAX_BUFFERS: usize = 4;
pub const MAX_BINDINGS: usize = 8;

/// Backend submission failures distinguish work that never reached the ring from work whose
/// ownership is uncertain. The latter retires the whole session without inventing completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitError {
    Unpublished(Status),
    DeviceLost,
}

pub trait Backend {
    fn info(&mut self) -> Result<DeviceInfo, Status>;
    /// Optional read-only extension. An unimplemented backend must not invent topology.
    fn topology(&mut self) -> Result<DeviceTopology, Status> {
        Err(Status::NotSupported)
    }
    /// Optional read-only extension. An unimplemented backend must not invent MMU properties.
    fn mmu_features(&mut self) -> Result<DeviceMmu, Status> {
        Err(Status::NotSupported)
    }
    /// Optional read-only extension. An unimplemented backend must not invent tiler properties.
    fn tiler_features(&mut self) -> Result<DeviceTiler, Status> {
        Err(Status::NotSupported)
    }
    /// Optional read-only extension. An unimplemented backend must not invent texture formats.
    fn texture_features(&mut self) -> Result<DeviceTexture, Status> {
        Err(Status::NotSupported)
    }
    /// Optional read-only extension. An unimplemented backend must not invent core features.
    fn core_features(&mut self) -> Result<DeviceCore, Status> {
        Err(Status::NotSupported)
    }
    /// Optional read-only extension. An unimplemented backend must not invent thread properties.
    fn thread_properties(&mut self) -> Result<DeviceThreads, Status> {
        Err(Status::NotSupported)
    }
    fn create_vm(&mut self) -> Result<(), Status>;
    fn create_buffer(&mut self, id: u32, bytes: u64) -> Result<(), Status>;
    fn bind(&mut self, binding: Binding) -> Result<(), Status>;
    fn create_group(&mut self) -> Result<(), Status>;
    fn submit(&mut self, submission: Submission) -> Result<(), SubmitError>;
}

#[derive(Default)]
pub struct Session {
    ids: RequestIds,
    vm: bool,
    group: bool,
    retired: bool,
    buffers: [u64; MAX_BUFFERS],
    bindings: [Option<Binding>; MAX_BINDINGS],
    timeline: Timeline,
}

impl Session {
    pub fn dispatch(
        &mut self,
        request: Request,
        backend: &mut impl Backend,
    ) -> Result<Response, Status> {
        self.ids.accept(request.id)?;
        if self.retired {
            return Err(Status::DeviceLost);
        }
        let result = self.execute(request.command, backend);
        if result == Err(Status::DeviceLost) {
            self.retired = true;
        }
        result
    }

    pub fn complete(&mut self, point: u64, status: u32) -> Result<(), Status> {
        if self.retired {
            return Err(Status::DeviceLost);
        }
        self.timeline.complete(point, status)
    }

    fn execute(
        &mut self,
        command: Command,
        backend: &mut impl Backend,
    ) -> Result<Response, Status> {
        match command {
            Command::GetInfo => backend.info().map(Response::Info),
            Command::GetTopology => {
                let topology = backend.topology()?;
                if topology.shader_present == 0 {
                    return Err(Status::DeviceLost);
                }
                Ok(Response::Topology(topology))
            }
            Command::GetMmuFeatures => {
                let mmu = backend.mmu_features()?;
                if mmu.mmu_features == 0 {
                    return Err(Status::DeviceLost);
                }
                Ok(Response::Mmu(mmu))
            }
            Command::GetTilerFeatures => {
                let tiler = backend.tiler_features()?;
                // A zero word cannot cross the wire. Unlike topology or MMU features it means
                // the tiler's shape is unknown rather than the device unusable, so it degrades
                // to a recoverable refusal instead of retiring the session. — SAKER
                if tiler.tiler_features == 0 {
                    return Err(Status::NotSupported);
                }
                Ok(Response::Tiler(tiler))
            }
            Command::GetTextureFeatures => {
                let texture = backend.texture_features()?;
                // Same optional-property rule as tiler, applied to element 0 only: a zero
                // there is unknown capability, not an unusable device, so it degrades instead
                // of retiring. Elements 1..3 cross verbatim, zero included — they were read
                // in the same powered block, so a zero is an attested value. — TIMBERDOODLE
                if texture.texture_features[0] == 0 {
                    return Err(Status::NotSupported);
                }
                Ok(Response::Texture(texture))
            }
            Command::GetCoreFeatures => {
                // Required in the query sense — any failure aborts the report — but the word
                // crosses verbatim, zero included: the Orange Pi 5 Plus measures exactly
                // 0x00000000 (J650), and variant byte 0 is the pinned Mesa table's G610 entry,
                // so zero is a valid attestation, not a failed read. — TIMBERDOODLE
                Ok(Response::Core(backend.core_features()?))
            }
            Command::GetThreadProperties => {
                // Crossed verbatim, all three words zero included: unlike tiler and texture
                // features, an all-zero group is a legitimate absence the consumer records by
                // keeping missing-property bit 4, not a refusal this layer invents. Only the
                // backend's own NotSupported crosses as the recoverable NotSupported error,
                // exactly as the tiler and texture arms do. — TIMBERDOODLE
                Ok(Response::Threads(backend.thread_properties()?))
            }
            Command::VmCreate => {
                if self.vm {
                    return Err(Status::BadState);
                }
                backend.create_vm()?;
                self.vm = true;
                Ok(Response::VmCreated { vm: 1 })
            }
            Command::BufferCreate { bytes } => {
                if !self.vm || self.group {
                    return Err(Status::BadState);
                }
                if !valid_buffer_bytes(bytes) {
                    return Err(Status::BadRequest);
                }
                let index = self
                    .buffers
                    .iter()
                    .position(|len| *len == 0)
                    .ok_or(Status::NoMemory)?;
                let used: u64 = self.buffers.iter().sum();
                if used + bytes > MAX_BUFFER_BYTES {
                    return Err(Status::NoMemory);
                }
                let id = index as u32 + 1;
                backend.create_buffer(id, bytes)?;
                self.buffers[index] = bytes;
                Ok(Response::BufferCreated { buffer: id, bytes })
            }
            Command::VmBind(binding) => {
                if !self.vm || self.group {
                    return Err(Status::BadState);
                }
                if binding.vm != 1 {
                    return Err(Status::NotFound);
                }
                let bytes = binding
                    .buffer
                    .checked_sub(1)
                    .and_then(|i| self.buffers.get(i as usize))
                    .filter(|bytes| **bytes != 0)
                    .ok_or(Status::NotFound)?;
                binding.validate(*bytes)?;
                if self
                    .bindings
                    .iter()
                    .flatten()
                    .any(|other| binding.overlaps(*other))
                {
                    return Err(Status::BadRequest);
                }
                let index = self
                    .bindings
                    .iter()
                    .position(Option::is_none)
                    .ok_or(Status::NoMemory)?;
                backend.bind(binding)?;
                self.bindings[index] = Some(binding);
                Ok(Response::VmBound)
            }
            Command::GroupCreate { vm } => {
                if !self.vm || self.group {
                    return Err(Status::BadState);
                }
                if vm != 1 {
                    return Err(Status::NotFound);
                }
                backend.create_group()?;
                self.group = true;
                Ok(Response::GroupCreated { group: 1 })
            }
            Command::Submit(job) => {
                if !self.group {
                    return Err(Status::BadState);
                }
                if job.group != 1 {
                    return Err(Status::NotFound);
                }
                if !self
                    .bindings
                    .iter()
                    .flatten()
                    .any(|b| job.validate_for(1, *b).is_ok())
                {
                    return Err(Status::BadRequest);
                }
                self.timeline.reserve(job.signal_point)?;
                match backend.submit(job) {
                    Ok(()) => Ok(Response::Submitted {
                        point: job.signal_point,
                    }),
                    Err(SubmitError::Unpublished(status)) => {
                        self.timeline.abort_unpublished(job.signal_point)?;
                        Err(status)
                    }
                    Err(SubmitError::DeviceLost) => Err(Status::DeviceLost),
                }
            }
            Command::SyncRead { group, point } => {
                if !self.group {
                    return Err(Status::BadState);
                }
                if group != 1 {
                    return Err(Status::NotFound);
                }
                Ok(Response::Sync {
                    point,
                    state: self.timeline.query(point)?,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::{BindFlags, SyncState, PAGE_BYTES, VA_START};
    use std::vec::Vec;

    #[derive(Default)]
    struct Fake {
        calls: Vec<Command>,
        fail: Option<Status>,
        submit_error: Option<SubmitError>,
        topology: Option<u64>,
        mmu_features: Option<u32>,
        tiler_features: Option<u32>,
        texture_features: Option<[u32; 4]>,
        core_features: Option<u32>,
        threads: Option<DeviceThreads>,
    }
    impl Fake {
        fn record(&mut self, command: Command) -> Result<(), Status> {
            self.calls.push(command);
            self.fail.take().map_or(Ok(()), Err)
        }
    }
    impl Backend for Fake {
        fn topology(&mut self) -> Result<DeviceTopology, Status> {
            self.record(Command::GetTopology)?;
            self.topology
                .map(|shader_present| DeviceTopology { shader_present })
                .ok_or(Status::NotSupported)
        }
        fn mmu_features(&mut self) -> Result<DeviceMmu, Status> {
            self.record(Command::GetMmuFeatures)?;
            self.mmu_features
                .map(|mmu_features| DeviceMmu { mmu_features })
                .ok_or(Status::NotSupported)
        }
        fn tiler_features(&mut self) -> Result<DeviceTiler, Status> {
            self.record(Command::GetTilerFeatures)?;
            self.tiler_features
                .map(|tiler_features| DeviceTiler { tiler_features })
                .ok_or(Status::NotSupported)
        }
        fn texture_features(&mut self) -> Result<DeviceTexture, Status> {
            self.record(Command::GetTextureFeatures)?;
            self.texture_features
                .map(|texture_features| DeviceTexture { texture_features })
                .ok_or(Status::NotSupported)
        }
        fn core_features(&mut self) -> Result<DeviceCore, Status> {
            self.record(Command::GetCoreFeatures)?;
            self.core_features
                .map(|core_features| DeviceCore { core_features })
                .ok_or(Status::NotSupported)
        }
        fn thread_properties(&mut self) -> Result<DeviceThreads, Status> {
            self.record(Command::GetThreadProperties)?;
            self.threads.ok_or(Status::NotSupported)
        }
        fn info(&mut self) -> Result<DeviceInfo, Status> {
            self.record(Command::GetInfo)?;
            Ok(DeviceInfo {
                gpu_id: 0xa8670005,
                cs_features: 0x7085f,
            })
        }
        fn create_vm(&mut self) -> Result<(), Status> {
            self.record(Command::VmCreate)
        }
        fn create_buffer(&mut self, _: u32, bytes: u64) -> Result<(), Status> {
            self.record(Command::BufferCreate { bytes })
        }
        fn bind(&mut self, b: Binding) -> Result<(), Status> {
            self.record(Command::VmBind(b))
        }
        fn create_group(&mut self) -> Result<(), Status> {
            self.record(Command::GroupCreate { vm: 1 })
        }
        fn submit(&mut self, s: Submission) -> Result<(), SubmitError> {
            self.calls.push(Command::Submit(s));
            self.submit_error.take().map_or(Ok(()), Err)
        }
    }
    fn binding() -> Binding {
        Binding {
            vm: 1,
            buffer: 1,
            va: VA_START,
            offset: 0,
            bytes: PAGE_BYTES,
            flags: BindFlags(BindFlags::READ_ONLY | BindFlags::NO_EXECUTE),
        }
    }
    fn job(point: u64) -> Command {
        Command::Submit(Submission {
            group: 1,
            stream_va: VA_START,
            stream_bytes: 48,
            signal_point: point,
        })
    }
    fn send(s: &mut Session, b: &mut Fake, command: Command) -> Result<Response, Status> {
        let id = s.ids.last + 1;
        let request = Request { id, command };
        let decoded = Request::decode(&request.encode()).unwrap();
        s.dispatch(decoded, b)
    }
    fn ready() -> (Session, Fake) {
        let (mut s, mut b) = (Session::default(), Fake::default());
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }).unwrap();
        send(&mut s, &mut b, Command::VmBind(binding())).unwrap();
        send(&mut s, &mut b, Command::GroupCreate { vm: 1 }).unwrap();
        (s, b)
    }
    #[test]
    fn topology_is_read_only_unsupported_is_recoverable_and_zero_retires() {
        let mut session = Session::default();
        let mut backend = Fake::default();
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTopology),
            Err(Status::NotSupported)
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::VmCreate),
            Ok(Response::VmCreated { vm: 1 })
        );
        backend.topology = Some(0x8000000000000001);
        let topology = Response::Topology(DeviceTopology {
            shader_present: 0x8000000000000001,
        });
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTopology),
            Ok(topology)
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::GroupCreate { vm: 1 }),
            Ok(Response::GroupCreated { group: 1 })
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTopology),
            Ok(topology)
        );
        assert!(session.buffers.iter().all(|bytes| *bytes == 0));
        assert!(session.bindings.iter().all(Option::is_none));
        backend.topology = Some(0);
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTopology),
            Err(Status::DeviceLost)
        );
        let calls = backend.calls.len();
        assert_eq!(
            send(&mut session, &mut backend, Command::GetInfo),
            Err(Status::DeviceLost)
        );
        assert_eq!(backend.calls.len(), calls);
    }

    #[test]
    fn mmu_features_are_read_only_unsupported_is_recoverable_and_zero_retires() {
        let mut session = Session::default();
        let mut backend = Fake::default();
        assert_eq!(
            send(&mut session, &mut backend, Command::GetMmuFeatures),
            Err(Status::NotSupported)
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::VmCreate),
            Ok(Response::VmCreated { vm: 1 })
        );
        // The register word crosses undecoded. 0x2830 is what the Orange Pi attests; its
        // [7:0] field is the 48-bit virtual-address width the consumer derives. — SAKER
        backend.mmu_features = Some(0x2830);
        let mmu = Response::Mmu(DeviceMmu {
            mmu_features: 0x2830,
        });
        assert_eq!(
            send(&mut session, &mut backend, Command::GetMmuFeatures),
            Ok(mmu)
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::GroupCreate { vm: 1 }),
            Ok(Response::GroupCreated { group: 1 })
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::GetMmuFeatures),
            Ok(mmu)
        );
        assert!(session.buffers.iter().all(|bytes| *bytes == 0));
        assert!(session.bindings.iter().all(Option::is_none));
        backend.mmu_features = Some(0);
        assert_eq!(
            send(&mut session, &mut backend, Command::GetMmuFeatures),
            Err(Status::DeviceLost)
        );
        let calls = backend.calls.len();
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTopology),
            Err(Status::DeviceLost)
        );
        assert_eq!(backend.calls.len(), calls);
    }

    #[test]
    fn core_features_are_read_only_unsupported_is_recoverable_and_zero_is_a_valid_attestation() {
        let mut session = Session::default();
        let mut backend = Fake::default();
        assert_eq!(
            send(&mut session, &mut backend, Command::GetCoreFeatures),
            Err(Status::NotSupported)
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::VmCreate),
            Ok(Response::VmCreated { vm: 1 })
        );
        // The register word crosses undecoded. The modeled word carries variant byte 0 — the
        // pinned Mesa table's requirement for a G610 entry to resolve — plus an unrelated bit
        // above the byte so nothing on this side selects [7:0]. — TIMBERDOODLE
        backend.core_features = Some(0x0000_1000);
        let core = Response::Core(DeviceCore {
            core_features: 0x0000_1000,
        });
        assert_eq!(
            send(&mut session, &mut backend, Command::GetCoreFeatures),
            Ok(core)
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::GroupCreate { vm: 1 }),
            Ok(Response::GroupCreated { group: 1 })
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::GetCoreFeatures),
            Ok(core)
        );
        assert!(session.buffers.iter().all(|bytes| *bytes == 0));
        assert!(session.bindings.iter().all(Option::is_none));
        // The board's measured word is exactly zero: variant byte 0 is the pinned table's
        // G610 entry, so zero is a valid attestation, not a failed read, and the session
        // stays live. — TIMBERDOODLE
        backend.core_features = Some(0);
        assert_eq!(
            send(&mut session, &mut backend, Command::GetCoreFeatures),
            Ok(Response::Core(DeviceCore { core_features: 0 }))
        );
        let calls = backend.calls.len();
        assert_eq!(
            send(&mut session, &mut backend, Command::GetInfo),
            Ok(Response::Info(DeviceInfo {
                gpu_id: 0xa8670005,
                cs_features: 0x7085f,
            }))
        );
        assert_eq!(backend.calls.len(), calls + 1);
    }

    #[test]
    fn tiler_features_refuse_zero_recoverably_instead_of_retiring_the_session() {
        let mut session = Session::default();
        let mut backend = Fake::default();
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTilerFeatures),
            Err(Status::NotSupported)
        );
        // RESEARCH/009 predicts 0x809: a 512-byte bin and 8 levels. The session only carries
        // the word; upstream decodes it. — SAKER
        backend.tiler_features = Some(0x809);
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTilerFeatures),
            Ok(Response::Tiler(DeviceTiler {
                tiler_features: 0x809
            }))
        );
        // What separates this opcode from topology and MMU: an unknown tiler shape degrades
        // instead of killing the session, and later work still proceeds. — SAKER
        backend.tiler_features = Some(0);
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTilerFeatures),
            Err(Status::NotSupported)
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::VmCreate),
            Ok(Response::VmCreated { vm: 1 })
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::GroupCreate { vm: 1 }),
            Ok(Response::GroupCreated { group: 1 })
        );
        assert!(session.buffers.iter().all(|bytes| *bytes == 0));
        assert!(session.bindings.iter().all(Option::is_none));
    }

    #[test]
    fn texture_features_follow_the_same_optional_rule_as_tiler_features() {
        let mut session = Session::default();
        let mut backend = Fake::default();
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTextureFeatures),
            Err(Status::NotSupported)
        );
        // Bit 25 of element 0 is AFRC presence on arch 10, and elements 1..3 are attested by
        // the same register read, so the whole array must cross with every bit intact rather
        // than being reduced to one boolean on this side. — TIMBERDOODLE
        backend.texture_features = Some([0x0200_0001, 0, 0x8000_0000, 0]);
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTextureFeatures),
            Ok(Response::Texture(DeviceTexture {
                texture_features: [0x0200_0001, 0, 0x8000_0000, 0]
            }))
        );
        // Only element 0 is the absence gate; zeroes in elements 1..3 are attested values.
        backend.texture_features = Some([0x0200_0000, 0, 0, 0]);
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTextureFeatures),
            Ok(Response::Texture(DeviceTexture {
                texture_features: [0x0200_0000, 0, 0, 0]
            }))
        );
        backend.texture_features = Some([0, 0xffff_ffff, 0xffff_ffff, 0xffff_ffff]);
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTextureFeatures),
            Err(Status::NotSupported)
        );
        // Absence degrades: the session still creates its VM and group afterwards. — SAKER
        assert_eq!(
            send(&mut session, &mut backend, Command::VmCreate),
            Ok(Response::VmCreated { vm: 1 })
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::GroupCreate { vm: 1 }),
            Ok(Response::GroupCreated { group: 1 })
        );
        assert!(session.buffers.iter().all(|bytes| *bytes == 0));
        assert!(session.bindings.iter().all(Option::is_none));
    }

    #[test]
    fn thread_properties_cross_verbatim_unsupported_is_recoverable_and_zero_words_are_an_absence() {
        let mut session = Session::default();
        let mut backend = Fake::default();
        assert_eq!(
            send(&mut session, &mut backend, Command::GetThreadProperties),
            Err(Status::NotSupported)
        );
        // A modeled G610-shaped group: 1024 threads/core, 1024 threads/workgroup, and a
        // THREAD_FEATURES word carrying 3 tasks/core in [31:24] and 0x100005 registers in
        // [21:0]. Upstream derives those fields; this layer must not. — TIMBERDOODLE
        let threads = DeviceThreads {
            max_threads: 1024,
            max_workgroup_size: 1024,
            thread_features: 0x0310_0005,
        };
        backend.threads = Some(threads);
        assert_eq!(
            send(&mut session, &mut backend, Command::GetThreadProperties),
            Ok(Response::Threads(threads))
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::VmCreate),
            Ok(Response::VmCreated { vm: 1 })
        );
        // All-zero words are a legitimate absence, not a refusal: the backend owns that
        // decision, and the words still cross verbatim so the consumer can keep its missing
        // bit. — TIMBERDOODLE
        backend.threads = Some(DeviceThreads {
            max_threads: 0,
            max_workgroup_size: 0,
            thread_features: 0,
        });
        assert_eq!(
            send(&mut session, &mut backend, Command::GetThreadProperties),
            Ok(Response::Threads(DeviceThreads {
                max_threads: 0,
                max_workgroup_size: 0,
                thread_features: 0
            }))
        );
        // The session is still live afterwards. — TIMBERDOODLE
        assert_eq!(
            send(&mut session, &mut backend, Command::GroupCreate { vm: 1 }),
            Ok(Response::GroupCreated { group: 1 })
        );
        assert!(session.buffers.iter().all(|bytes| *bytes == 0));
        assert!(session.bindings.iter().all(Option::is_none));
    }

    #[test]
    fn info_query_is_available_before_vm_creation_and_does_not_consume_objects() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        assert_eq!(
            send(&mut s, &mut b, Command::GetInfo),
            Ok(Response::Info(DeviceInfo {
                gpu_id: 0xa8670005,
                cs_features: 0x7085f
            }))
        );
        assert_eq!(
            send(&mut s, &mut b, Command::VmCreate),
            Ok(Response::VmCreated { vm: 1 })
        );
        assert_eq!(b.calls.as_slice(), &[Command::GetInfo, Command::VmCreate]);
    }

    #[test]
    fn two_submissions_remain_pending_until_backend_completion() {
        let (mut s, mut b) = ready();
        for point in 1..=2 {
            assert_eq!(
                send(&mut s, &mut b, job(point)),
                Ok(Response::Submitted { point })
            );
            assert_eq!(
                send(&mut s, &mut b, Command::SyncRead { group: 1, point }),
                Ok(Response::Sync {
                    point,
                    state: SyncState::Pending
                })
            );
            s.complete(point, 0).unwrap();
            assert_eq!(
                send(&mut s, &mut b, Command::SyncRead { group: 1, point }),
                Ok(Response::Sync {
                    point,
                    state: SyncState::Complete { gpu_status: 0 }
                })
            );
        }
    }
    #[test]
    fn invalid_ids_overlap_frozen_binds_and_replay_never_reach_backend() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }).unwrap();
        send(&mut s, &mut b, Command::VmBind(binding())).unwrap();
        let calls = b.calls.len();
        assert_eq!(
            send(&mut s, &mut b, Command::VmBind(binding())),
            Err(Status::BadRequest)
        );
        assert_eq!(
            send(
                &mut s,
                &mut b,
                Command::VmBind(Binding {
                    buffer: 99,
                    ..binding()
                })
            ),
            Err(Status::NotFound)
        );
        assert_eq!(b.calls.len(), calls);
        send(&mut s, &mut b, Command::GroupCreate { vm: 1 }).unwrap();
        let calls = b.calls.len();
        assert_eq!(
            send(&mut s, &mut b, Command::VmBind(binding())),
            Err(Status::BadState)
        );
        assert_eq!(
            send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }),
            Err(Status::BadState)
        );
        assert_eq!(
            send(
                &mut s,
                &mut b,
                Command::Submit(Submission {
                    group: 99,
                    stream_va: VA_START,
                    stream_bytes: 48,
                    signal_point: 1
                })
            ),
            Err(Status::NotFound)
        );
        assert_eq!(
            s.dispatch(
                Request {
                    id: 1,
                    command: Command::VmCreate
                },
                &mut b
            ),
            Err(Status::BadRequest)
        );
        assert_eq!(b.calls.len(), calls);
    }
    #[test]
    fn failures_do_not_commit_allocations_or_bindings_and_enforce_budget() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        b.fail = Some(Status::NoMemory);
        assert_eq!(
            send(&mut s, &mut b, Command::VmCreate),
            Err(Status::NoMemory)
        );
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        b.fail = Some(Status::NoMemory);
        assert_eq!(
            send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }),
            Err(Status::NoMemory)
        );
        assert_eq!(
            send(
                &mut s,
                &mut b,
                Command::BufferCreate {
                    bytes: MAX_BUFFER_BYTES
                }
            ),
            Ok(Response::BufferCreated {
                buffer: 1,
                bytes: MAX_BUFFER_BYTES
            })
        );
        assert_eq!(
            send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }),
            Err(Status::NoMemory)
        );
        b.fail = Some(Status::NoMemory);
        assert_eq!(
            send(&mut s, &mut b, Command::VmBind(binding())),
            Err(Status::NoMemory)
        );
        assert_eq!(
            send(&mut s, &mut b, Command::VmBind(binding())),
            Ok(Response::VmBound)
        );
    }
    #[test]
    fn object_quotas_and_partial_map_failure_bound_the_session() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        assert_eq!(
            send(&mut s, &mut b, Command::GroupCreate { vm: 1 }),
            Err(Status::BadState)
        );
        assert!(b.calls.is_empty());
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        for _ in 0..MAX_BUFFERS {
            send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }).unwrap();
        }
        let calls = b.calls.len();
        assert_eq!(
            send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }),
            Err(Status::NoMemory)
        );
        assert_eq!(b.calls.len(), calls);
        for i in 0..MAX_BINDINGS {
            send(
                &mut s,
                &mut b,
                Command::VmBind(Binding {
                    va: VA_START + i as u64 * PAGE_BYTES,
                    ..binding()
                }),
            )
            .unwrap();
        }
        let calls = b.calls.len();
        assert_eq!(
            send(
                &mut s,
                &mut b,
                Command::VmBind(Binding {
                    va: VA_START + 8 * PAGE_BYTES,
                    ..binding()
                })
            ),
            Err(Status::NoMemory)
        );
        assert_eq!(b.calls.len(), calls);

        let (mut s, mut b) = (Session::default(), Fake::default());
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }).unwrap();
        b.fail = Some(Status::DeviceLost);
        assert_eq!(
            send(&mut s, &mut b, Command::VmBind(binding())),
            Err(Status::DeviceLost)
        );
        let calls = b.calls.len();
        assert_eq!(
            send(&mut s, &mut b, Command::VmBind(binding())),
            Err(Status::DeviceLost)
        );
        assert_eq!(b.calls.len(), calls);
    }

    #[test]
    fn publication_uncertainty_retires_and_unpublished_failure_can_retry() {
        let (mut s, mut b) = ready();
        b.submit_error = Some(SubmitError::Unpublished(Status::Busy));
        assert_eq!(send(&mut s, &mut b, job(1)), Err(Status::Busy));
        assert_eq!(
            send(&mut s, &mut b, job(1)),
            Ok(Response::Submitted { point: 1 })
        );
        assert_eq!(send(&mut s, &mut b, job(2)), Err(Status::Busy));
        s.complete(1, 0).unwrap();
        b.submit_error = Some(SubmitError::DeviceLost);
        assert_eq!(send(&mut s, &mut b, job(2)), Err(Status::DeviceLost));
        assert_eq!(s.complete(2, 0), Err(Status::DeviceLost));
        assert_eq!(
            send(&mut s, &mut b, Command::SyncRead { group: 1, point: 2 }),
            Err(Status::DeviceLost)
        );
    }
}
