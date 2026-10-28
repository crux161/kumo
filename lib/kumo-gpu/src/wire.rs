//j645
//j646
//j647
//j650
//j651

//! Exact, versioned little-endian channel frames. All unused fields must be zero. No repr(C),
//! pointers, native padding or process-local handles cross the wire. One request/reply at a
//! time; request IDs are nonzero and strictly increase for the channel's lifetime. Exhaustion
//! requires a fresh session. The server must reject duplicate/stale IDs before any mutation.

use crate::{
    valid_buffer_bytes, BindFlags, Binding, DeviceCore, DeviceInfo, DeviceMmu, DeviceTexture,
    DeviceThreads, DeviceTiler, DeviceTopology, Status, Submission, SyncState,
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
    VmCreate,
    BufferCreate {
        bytes: u64,
    },
    VmBind(Binding),
    /// One normal-priority queue with a driver-owned 4 KiB ring and core masks.
    GroupCreate {
        vm: u32,
    },
    Submit(Submission),
    /// Query is nonblocking; Pending never implies completion or permission to reuse backing.
    SyncRead {
        group: u32,
        point: u64,
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
            Command::VmCreate => (1, [0; 6]),
            Command::BufferCreate { bytes } => (2, [bytes, 0, 0, 0, 0, 0]),
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
            Command::GroupCreate { vm } => (4, [u64::from(vm), 0, 0, 0, 0, 0]),
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
            1 => Command::VmCreate,
            2 if valid_buffer_bytes(a[0]) => Command::BufferCreate { bytes: a[0] },
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
    VmCreated {
        vm: u32,
    },
    /// Exactly one transferred VMO accompanies a successful BufferCreated; no other reply
    /// carries a handle. `bytes` is the allocation's exact length, with no implicit rounding.
    BufferCreated {
        buffer: u32,
        bytes: u64,
    },
    VmBound,
    GroupCreated {
        group: u32,
    },
    /// Accepted for execution, not completed. Poll SyncRead before reusing any backing.
    Submitted {
        point: u64,
    },
    Sync {
        point: u64,
        state: SyncState,
    },
}

impl Response {
    pub const fn handle_count(self) -> usize {
        match self {
            Self::BufferCreated { .. } => 1,
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
            Ok(Response::Texture(texture)) => {
                (11, [u64::from(texture.texture_features_0), 0, 0, 0])
            }
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
            Ok(Response::VmCreated { vm }) => (1, [u64::from(vm), 0, 0, 0]),
            Ok(Response::BufferCreated { buffer, bytes }) => (2, [u64::from(buffer), bytes, 0, 0]),
            Ok(Response::VmBound) => (3, [0; 4]),
            Ok(Response::GroupCreated { group }) => (4, [u64::from(group), 0, 0, 0]),
            Ok(Response::Submitted { point }) => (5, [point, 0, 0, 0]),
            Ok(Response::Sync { point, state }) => match state {
                SyncState::Pending => (6, [point, 0, 0, 0]),
                SyncState::Complete { gpu_status } => (6, [point, 1, u64::from(gpu_status), 0]),
            },
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
                11 if a[0] != 0 => Response::Texture(DeviceTexture {
                    texture_features_0: a[0] as u32,
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
                1 if a[0] != 0 => Response::VmCreated { vm: a[0] as u32 },
                2 if a[0] != 0 && valid_buffer_bytes(a[1]) => Response::BufferCreated {
                    buffer: a[0] as u32,
                    bytes: a[1],
                },
                3 => Response::VmBound,
                4 if a[0] != 0 => Response::GroupCreated { group: a[0] as u32 },
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
