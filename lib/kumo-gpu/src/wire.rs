//j681
//j707
//j742
//j752
//j754

//! Exact, versioned little-endian channel frames. All unused fields must be zero. No repr(C),
//! pointers, native padding or process-local handles cross the wire. One request/reply at a
//! time; request IDs are nonzero and strictly increase for the channel's lifetime. Exhaustion
//! requires a fresh session. The server must reject duplicate/stale IDs before any mutation.

use crate::{
    valid_buffer_bytes, BindFlags, Binding, DeviceCore, DeviceInfo, DeviceMemory, DeviceMmu,
    DeviceTexture, DeviceThreads, DeviceTiler, DeviceTopology, GroupReport, HeapPlacement,
    HeapSpec, Status, Submission, SyncState, Unbind,
};

pub const REQUEST_LEN: usize = 64;
pub const REPLY_LEN: usize = 48;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    GetInfo,
    GetTopology,
    GetMmuFeatures,
    GetTilerFeatures,
    GetTextureFeatures,
    GetCoreFeatures,
    GetThreadProperties,
    /// The memory-system words and the coherency protocol the driver programmed (G7o-a).
    /// Appended after the discovery opcodes, so earlier sessions are unchanged. — TIMBERDOODLE
    GetMemoryProperties,
    VmCreate,
    BufferCreate {
        bytes: u64,
    },
    /// Release one allocation this session owns. While the binding model is frozen (G8a,
    /// before GroupCreate), the server first unbinds every GPU VA mapping that names it.
    /// The reply is `BufferDestroyed`; a never-issued, foreign, or already-destroyed ID is
    /// `NotFound`. — TIMBERDOODLE
    BufferDestroy {
        buffer: u32,
    },
    VmBind(Binding),
    /// Remove exactly the live mappings covering one contiguous range. Allowed after
    /// GroupCreate: the driver orders same-address-space work and invalidates only the packed
    /// page-table region, without stopping any queue. The reply is `VmUnbound` echoing the
    /// range; an unknown, foreign or partially covered range is refused without mutation.
    /// — TIMBERDOODLE
    VmUnbind(Unbind),
    /// One normal-priority queue with a driver-owned 4 KiB ring and core masks.
    GroupCreate {
        vm: u32,
    },
    /// Obtain a READ-only CPU status VMO, which outlives terminal channel close. — MILVUS
    GroupWatch {
        group: u32,
    },
    Submit(Submission),
    /// Query is nonblocking; Pending never implies completion or permission to reuse backing.
    SyncRead {
        group: u32,
        point: u64,
    },
    /// Register a wait on the gpu-done timeline (DESIGN/023 G8c-a). The reply is `WaitDone`
    /// when the point is already complete, or `WaitArmed` carrying one transferred
    /// wake-channel read end; the driver writes one byte when the point completes. SyncRead
    /// remains the polling fallback. — TIMBERDOODLE
    FenceWait {
        group: u32,
        point: u64,
    },
    /// G7q-a: allocate, link and map one tiler heap in the driver's heap window. The reply is
    /// `HeapCreated` with the context and first-chunk addresses; no handle crosses, because the
    /// client never maps heap memory. — TIMBERDOODLE
    HeapCreate(HeapSpec),
    /// Unmap and release every page of one heap. The serve loop's ordering means no job is in
    /// flight; a client must not destroy a heap a later job will still name. — TIMBERDOODLE
    HeapDestroy {
        heap: u32,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Request {
    pub id: u32,
    pub command: Command,
}

impl Request {
    pub fn encode(self) -> [u8; REQUEST_LEN] {
        let (op, args) = match self.command {
            Command::GetInfo => (7, [0; 6]),
            Command::GetTopology => (8, [0; 6]),
            Command::GetMmuFeatures => (9, [0; 6]),
            Command::GetTilerFeatures => (10, [0; 6]),
            Command::GetTextureFeatures => (11, [0; 6]),
            Command::GetCoreFeatures => (12, [0; 6]),
            Command::GetThreadProperties => (13, [0; 6]),
            Command::GetMemoryProperties => (17, [0; 6]),
            Command::VmCreate => (1, [0; 6]),
            Command::BufferCreate { bytes } => (2, [bytes, 0, 0, 0, 0, 0]),
            Command::BufferDestroy { buffer } => (14, [u64::from(buffer), 0, 0, 0, 0, 0]),
            Command::VmBind(b) => (
                3,
                [
                    u64::from(b.vm),
                    u64::from(b.buffer),
                    b.va,
                    b.offset,
                    b.bytes,
                    u64::from(b.flags.0),
                ],
            ),
            Command::VmUnbind(u) => (15, [u64::from(u.vm), u.va, u.bytes, 0, 0, 0]),
            Command::GroupCreate { vm } => (4, [u64::from(vm), 0, 0, 0, 0, 0]),
            Command::GroupWatch { group } => (18, [u64::from(group), 0, 0, 0, 0, 0]),
            Command::Submit(s) => (
                5,
                [
                    u64::from(s.group),
                    s.stream_va,
                    u64::from(s.stream_bytes),
                    s.signal_point,
                    0,
                    0,
                ],
            ),
            Command::SyncRead { group, point } => (6, [u64::from(group), point, 0, 0, 0, 0]),
            Command::FenceWait { group, point } => (16, [u64::from(group), point, 0, 0, 0, 0]),
            Command::HeapCreate(h) => (
                19,
                [
                    u64::from(h.vm),
                    u64::from(h.chunk_bytes),
                    u64::from(h.initial_chunks),
                    u64::from(h.max_chunks),
                    u64::from(h.target_in_flight),
                    0,
                ],
            ),
            Command::HeapDestroy { heap } => (20, [u64::from(heap), 0, 0, 0, 0, 0]),
        };
        let mut bytes = [0; REQUEST_LEN];
        bytes[..4].copy_from_slice(b"KGQ1");
        bytes[4..8].copy_from_slice(&self.id.to_le_bytes());
        bytes[8..12].copy_from_slice(&u32::to_le_bytes(op));
        put_args(&mut bytes[16..], &args);
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != REQUEST_LEN || &bytes[..4] != b"KGQ1" {
            return None;
        }
        let id = word(bytes, 4);
        if id == 0 {
            return None;
        }
        let a = args::<6>(&bytes[16..]);
        let command = match word(bytes, 8) {
            7 => Command::GetInfo,
            8 => Command::GetTopology,
            9 => Command::GetMmuFeatures,
            10 => Command::GetTilerFeatures,
            11 => Command::GetTextureFeatures,
            12 => Command::GetCoreFeatures,
            13 => Command::GetThreadProperties,
            17 => Command::GetMemoryProperties,
            18 if a[0] != 0 => Command::GroupWatch { group: a[0] as u32 },
            1 => Command::VmCreate,
            2 if valid_buffer_bytes(a[0]) => Command::BufferCreate { bytes: a[0] },
            // Zero is never a buffer ID. The canonical re-encode below also refuses an ID
            // whose high bits would be lost by the u32 conversion and any reserved slot.
            14 if a[0] != 0 => Command::BufferDestroy {
                buffer: a[0] as u32,
            },
            3 => {
                let b = Binding {
                    vm: a[0] as u32,
                    buffer: a[1] as u32,
                    va: a[2],
                    offset: a[3],
                    bytes: a[4],
                    flags: BindFlags(a[5] as u32),
                };
                // Full buffer bounds and overlap checks happen after ID resolution.
                b.validate(crate::MAX_BUFFER_BYTES).ok()?;
                Command::VmBind(b)
            }
            15 if a[0] != 0 => {
                let u = Unbind {
                    vm: a[0] as u32,
                    va: a[1],
                    bytes: a[2],
                };
                if !u.valid_shape() {
                    return None;
                }
                Command::VmUnbind(u)
            }
            4 if a[0] != 0 => Command::GroupCreate { vm: a[0] as u32 },
            5 => {
                let s = Submission {
                    group: a[0] as u32,
                    stream_va: a[1],
                    stream_bytes: a[2] as u32,
                    signal_point: a[3],
                };
                if !s.valid_shape() {
                    return None;
                }
                Command::Submit(s)
            }
            6 if a[0] != 0 && a[1] != 0 => Command::SyncRead {
                group: a[0] as u32,
                point: a[1],
            },
            16 if a[0] != 0 && a[1] != 0 => Command::FenceWait {
                group: a[0] as u32,
                point: a[1],
            },
            19 => {
                let h = HeapSpec {
                    vm: a[0] as u32,
                    chunk_bytes: a[1] as u32,
                    initial_chunks: a[2] as u32,
                    max_chunks: a[3] as u32,
                    target_in_flight: a[4] as u32,
                };
                if !h.valid_shape() {
                    return None;
                }
                Command::HeapCreate(h)
            }
            20 if a[0] != 0 => Command::HeapDestroy { heap: a[0] as u32 },
            _ => return None,
        };
        let request = Self { id, command };
        // Canonical encoding also rejects high bits lost by narrow integer conversions.
        (request.encode() == bytes).then_some(request)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Response {
    Info(DeviceInfo),
    Topology(DeviceTopology),
    Mmu(DeviceMmu),
    Tiler(DeviceTiler),
    Texture(DeviceTexture),
    Core(DeviceCore),
    /// Three raw thread-property words in slots 0..2; slot 3 is zero. No word is
    /// special-cased here: all three may legitimately be zero when the group is absent. — TIMBERDOODLE
    Threads(DeviceThreads),
    /// Four raw words in slots 0..3: `L2_FEATURES`, `MEM_FEATURES`, `COHERENCY_FEATURES` and
    /// the programmed coherency protocol. All four slots are used, so there is no reserved
    /// slot to check; the re-encode check still refuses a word truncated to 32 bits.
    /// — TIMBERDOODLE
    Memory(DeviceMemory),
    VmCreated {
        vm: u32,
    },
    /// Exactly one transferred VMO accompanies a successful BufferCreated; no other reply
    /// carries a handle. `bytes` is the allocation's exact length, with no implicit rounding.
    BufferCreated {
        buffer: u32,
        bytes: u64,
    },
    /// The named allocation is gone: its live bindings were unbound and its backing released.
    /// The reply carries no handle; the caller's own granted handle is the caller's to close.
    /// — TIMBERDOODLE
    BufferDestroyed {
        buffer: u32,
    },
    VmBound,
    /// The named range is gone: every live binding it covered was unbound after the driver
    /// cleared and invalidated exactly that mapping. The range crosses verbatim as the echo a
    /// client correlates against its request; no handle accompanies it. — TIMBERDOODLE
    VmUnbound(Unbind),
    GroupCreated {
        group: u32,
    },
    GroupWatch {
        group: u32,
        report: GroupReport,
    },
    /// Accepted for execution, not completed. Poll SyncRead before reusing any backing.
    Submitted {
        point: u64,
    },
    Sync {
        point: u64,
        state: SyncState,
    },
    /// The point was already complete when the wait was registered; no handle accompanies.
    WaitDone {
        point: u64,
        gpu_status: u32,
    },
    /// Armed; exactly one transferred wake-channel read end accompanies this reply. The
    /// driver writes one byte to it and closes its end when the point completes.
    WaitArmed {
        point: u64,
    },
    HeapCreated {
        heap: u32,
        placement: HeapPlacement,
    },
    /// The heap is unmapped and its pages released; `bytes` is what returned to the budget.
    HeapDestroyed {
        heap: u32,
        bytes: u64,
    },
}

impl Response {
    pub const fn handle_count(self) -> usize {
        match self {
            Self::BufferCreated { .. } | Self::WaitArmed { .. } | Self::GroupWatch { .. } => 1,
            _ => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reply {
    /// Zero is permitted only for BadRequest when the request frame could not be decoded.
    pub id: u32,
    pub result: Result<Response, Status>,
}

impl Reply {
    pub fn encode(self) -> [u8; REPLY_LEN] {
        let mut bytes = [0; REPLY_LEN];
        bytes[..4].copy_from_slice(b"KGA1");
        bytes[4..8].copy_from_slice(&self.id.to_le_bytes());
        let (op, a) = match self.result {
            Ok(Response::Info(info)) => (
                7,
                [u64::from(info.gpu_id), u64::from(info.cs_features), 0, 0],
            ),
            Err(status) => {
                bytes[8..12].copy_from_slice(&(status as u32).to_le_bytes());
                (0, [0; 4])
            }
            Ok(Response::Topology(topology)) => (8, [topology.shader_present, 0, 0, 0]),
            Ok(Response::Mmu(mmu)) => (9, [u64::from(mmu.mmu_features), 0, 0, 0]),
            Ok(Response::Tiler(tiler)) => (10, [u64::from(tiler.tiler_features), 0, 0, 0]),
            Ok(Response::Texture(texture)) => (
                11,
                [
                    u64::from(texture.texture_features[0]),
                    u64::from(texture.texture_features[1]),
                    u64::from(texture.texture_features[2]),
                    u64::from(texture.texture_features[3]),
                ],
            ),
            Ok(Response::Core(core)) => (12, [u64::from(core.core_features), 0, 0, 0]),
            Ok(Response::Threads(threads)) => (
                13,
                [
                    u64::from(threads.max_threads),
                    u64::from(threads.max_workgroup_size),
                    u64::from(threads.thread_features),
                    0,
                ],
            ),
            Ok(Response::Memory(memory)) => (
                17,
                [
                    u64::from(memory.l2_features),
                    u64::from(memory.mem_features),
                    u64::from(memory.coherency_features),
                    u64::from(memory.coherency_protocol),
                ],
            ),
            Ok(Response::VmCreated { vm }) => (1, [u64::from(vm), 0, 0, 0]),
            Ok(Response::BufferCreated { buffer, bytes }) => (2, [u64::from(buffer), bytes, 0, 0]),
            Ok(Response::BufferDestroyed { buffer }) => (14, [u64::from(buffer), 0, 0, 0]),
            Ok(Response::VmBound) => (3, [0; 4]),
            Ok(Response::VmUnbound(u)) => (15, [u64::from(u.vm), u.va, u.bytes, 0]),
            Ok(Response::GroupCreated { group }) => (4, [u64::from(group), 0, 0, 0]),
            Ok(Response::GroupWatch { group, report }) => (
                18,
                [
                    u64::from(group),
                    report.state as u64,
                    u64::from(report.gpu_status),
                    0,
                ],
            ),
            Ok(Response::Submitted { point }) => (5, [point, 0, 0, 0]),
            Ok(Response::Sync { point, state }) => match state {
                SyncState::Pending => (6, [point, 0, 0, 0]),
                SyncState::Complete { gpu_status } => (6, [point, 1, u64::from(gpu_status), 0]),
            },
            Ok(Response::WaitDone { point, gpu_status }) => {
                (16, [point, 0, u64::from(gpu_status), 0])
            }
            Ok(Response::WaitArmed { point }) => (16, [point, 1, 0, 0]),
            Ok(Response::HeapCreated { heap, placement }) => (
                19,
                [
                    u64::from(heap),
                    placement.context_va,
                    placement.first_chunk_va,
                    0,
                ],
            ),
            Ok(Response::HeapDestroyed { heap, bytes }) => (20, [u64::from(heap), bytes, 0, 0]),
        };
        bytes[12..16].copy_from_slice(&u32::to_le_bytes(op));
        put_args(&mut bytes[16..], &a);
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != REPLY_LEN || &bytes[..4] != b"KGA1" {
            return None;
        }
        let id = word(bytes, 4);
        let status = word(bytes, 8);
        let op = word(bytes, 12);
        let a = args::<4>(&bytes[16..]);
        let result = if status != 0 {
            Err(match status {
                1 => Status::BadRequest,
                2 => Status::BadState,
                3 => Status::NotFound,
                4 => Status::Busy,
                5 => Status::NoMemory,
                6 => Status::DeviceLost,
                7 => Status::NotSupported,
                _ => return None,
            })
        } else {
            Ok(match op {
                7 => Response::Info(DeviceInfo {
                    gpu_id: a[0] as u32,
                    cs_features: a[1] as u32,
                }),
                8 if a[0] != 0 => Response::Topology(DeviceTopology {
                    shader_present: a[0],
                }),
                9 if a[0] != 0 => Response::Mmu(DeviceMmu {
                    mmu_features: a[0] as u32,
                }),
                10 if a[0] != 0 => Response::Tiler(DeviceTiler {
                    tiler_features: a[0] as u32,
                }),
                // Element 0 is the optional-absence gate, so a zero there is never a success
                // on this channel; the session maps it to NotSupported. Elements 1..3 are
                // unrestricted, zero included, and the re-encode round-trip below refuses
                // any high bits a 32-bit element cannot carry. — TIMBERDOODLE
                11 if a[0] != 0 => Response::Texture(DeviceTexture {
                    texture_features: [a[0] as u32, a[1] as u32, a[2] as u32, a[3] as u32],
                }),
                12 => Response::Core(DeviceCore {
                    core_features: a[0] as u32,
                }),
                // Unlike tiler and texture features there is no nonzero guard: absence is the
                // backend's decision, so the frame must round-trip zero words unchanged. The
                // re-encode check still refuses truncated words and a nonzero reserved slot.
                // — TIMBERDOODLE
                13 => Response::Threads(DeviceThreads {
                    max_threads: a[0] as u32,
                    max_workgroup_size: a[1] as u32,
                    thread_features: a[2] as u32,
                }),
                // Required words with no absence meaning: zero crosses verbatim, and the
                // consumer, not this codec, decides what a zero word implies. — TIMBERDOODLE
                17 => Response::Memory(DeviceMemory {
                    l2_features: a[0] as u32,
                    mem_features: a[1] as u32,
                    coherency_features: a[2] as u32,
                    coherency_protocol: a[3] as u32,
                }),
                1 if a[0] != 0 => Response::VmCreated { vm: a[0] as u32 },
                2 if a[0] != 0 && valid_buffer_bytes(a[1]) => Response::BufferCreated {
                    buffer: a[0] as u32,
                    bytes: a[1],
                },
                // Same zero/truncation rules as the request: canonical re-encoding rejects a
                // zero ID, lost high bits, and any nonzero reserved slot. — TIMBERDOODLE
                14 if a[0] != 0 => Response::BufferDestroyed {
                    buffer: a[0] as u32,
                },
                3 => Response::VmBound,
                15 if a[0] != 0 => {
                    let u = Unbind {
                        vm: a[0] as u32,
                        va: a[1],
                        bytes: a[2],
                    };
                    if !u.valid_shape() {
                        return None;
                    }
                    Response::VmUnbound(u)
                }
                4 if a[0] != 0 => Response::GroupCreated { group: a[0] as u32 },
                18 if a[0] != 0 => Response::GroupWatch {
                    group: a[0] as u32,
                    report: GroupReport::from_words(a[1] as u32, a[2] as u32)?,
                },
                5 if a[0] != 0 => Response::Submitted { point: a[0] },
                6 if a[0] != 0 => Response::Sync {
                    point: a[0],
                    state: match a[1] {
                        0 => SyncState::Pending,
                        1 => SyncState::Complete {
                            gpu_status: a[2] as u32,
                        },
                        _ => return None,
                    },
                },
                16 if a[0] != 0 && a[1] == 0 && a[3] == 0 => Response::WaitDone {
                    point: a[0],
                    gpu_status: a[2] as u32,
                },
                16 if a[0] != 0 && a[1] == 1 && a[2] == 0 && a[3] == 0 => {
                    Response::WaitArmed { point: a[0] }
                }
                19 if a[0] != 0 => {
                    let placement = HeapPlacement {
                        context_va: a[1],
                        first_chunk_va: a[2],
                    };
                    if !placement.valid() {
                        return None;
                    }
                    Response::HeapCreated {
                        heap: a[0] as u32,
                        placement,
                    }
                }
                20 if a[0] != 0 && a[1] != 0 && a[1] % crate::PAGE_BYTES == 0 => {
                    Response::HeapDestroyed {
                        heap: a[0] as u32,
                        bytes: a[1],
                    }
                }
                _ => return None,
            })
        };
        if id == 0 && result != Err(Status::BadRequest) {
            return None;
        }
        let reply = Self { id, result };
        (reply.encode() == bytes).then_some(reply)
    }
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn args<const N: usize>(bytes: &[u8]) -> [u64; N] {
    core::array::from_fn(|i| u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap()))
}

fn put_args(bytes: &mut [u8], args: &[u64]) {
    for (slot, value) in bytes.chunks_exact_mut(8).zip(args) {
        slot.copy_from_slice(&value.to_le_bytes());
    }
}
