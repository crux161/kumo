//j749
//! Session-local native sync objects, without DRM handles or sync files. Calls are serial.
//! Pending means an accepted job owns a signal; only FenceWait Done proves completion.
//! This first lane publishes signals, not submit dependencies or a blocking OS wait.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::client::{Client, Error as ClientError, FenceStatus, Transport};
use crate::{Fence, Submission};

pub const MAX_SYNCS: usize = 64;
pub const MAX_TIMELINE_POINTS: usize = 256;

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidRequest,
    NotFound,
    NoMemory,
    Busy,
    Client(ClientError),
    GpuFault(u32),
}
impl From<ClientError> for Error {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

/// Opaque local identity, qualified by a never-reused store generation. Not a capability
/// or a wire handle; the store borrows the actual session capability. — MILVUS
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncHandle {
    owner: u64,
    id: u32,
}

/// Binary objects use value 0. Timeline waits for 0 are always satisfied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Point {
    pub sync: SyncHandle,
    pub value: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Payload {
    Signaled,
    Fence(Fence),
}

/// A captured payload, independent of subsequent reset, replacement or object destruction.
/// It can be armed only on its original store/session. No raw fence is accepted from callers.
/// This is the snapshot Gallium needs in place of exporting a sync file. — MILVUS
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Snapshot {
    owner: u64,
    payload: Payload,
}

enum Object {
    Binary(Option<Payload>),
    Timeline {
        completed: u64,
        // Logical values are increasing; the fences are increasing points on one queue.
        pending: Vec<(u64, Fence)>,
    },
}

struct Record {
    id: u32,
    object: Object,
}

/// Owns only sync metadata and an exclusive borrow of one native client. Dropping an object
/// never cancels GPU work or releases DMA backing. The enclosing device owns containment.
/// All published GPU signals belong to one group; multi-queue dependency ordering is later
/// work. At most 64 objects and 256 unobserved timeline points per object. — MILVUS
pub struct SyncObjects<'a, T: Transport> {
    client: &'a mut Client<T>,
    owner: u64,
    next_id: u32,
    group: Option<u32>,
    objects: Vec<Record>,
}

impl<'a, T: Transport> SyncObjects<'a, T> {
    pub fn new(client: &'a mut Client<T>) -> Result<Self, Error> {
        let owner = NEXT_OWNER
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::NoMemory)?;
        if client.is_lost() {
            return Err(ClientError::DeviceLost.into());
        }
        Ok(Self {
            client,
            owner,
            next_id: 0,
            group: None,
            objects: Vec::new(),
        })
    }

    fn ready(&self) -> Result<(), Error> {
        if self.client.is_lost() {
            Err(ClientError::DeviceLost.into())
        } else {
            Ok(())
        }
    }

    fn index(&self, handle: SyncHandle) -> Result<usize, Error> {
        self.ready()?;
        if handle.owner != self.owner {
            return Err(Error::NotFound);
        }
        self.objects
            .iter()
            .position(|record| record.id == handle.id)
            .ok_or(Error::NotFound)
    }

    fn create(&mut self, object: Object) -> Result<SyncHandle, Error> {
        self.ready()?;
        if self.objects.len() == MAX_SYNCS {
            return Err(Error::NoMemory);
        }
        let id = self.next_id.checked_add(1).ok_or(Error::NoMemory)?;
        self.objects.try_reserve(1).map_err(|_| Error::NoMemory)?;
        self.objects.push(Record { id, object });
        self.next_id = id;
        Ok(SyncHandle {
            owner: self.owner,
            id,
        })
    }

    pub fn create_binary(&mut self, signaled: bool) -> Result<SyncHandle, Error> {
        self.create(Object::Binary(signaled.then_some(Payload::Signaled)))
    }

    pub fn create_timeline(&mut self, initial_value: u64) -> Result<SyncHandle, Error> {
        self.create(Object::Timeline {
            completed: initial_value,
            pending: Vec::new(),
        })
    }

    pub fn destroy(&mut self, handle: SyncHandle) -> Result<(), Error> {
        let index = self.index(handle)?;
        self.objects.swap_remove(index);
        Ok(())
    }

    pub fn reset(&mut self, handle: SyncHandle) -> Result<(), Error> {
        let index = self.index(handle)?;
        let Object::Binary(payload) = &mut self.objects[index].object else {
            return Err(Error::InvalidRequest);
        };
        *payload = None;
        Ok(())
    }

    pub fn signal(&mut self, point: Point) -> Result<(), Error> {
        let index = self.index(point.sync)?;
        match &mut self.objects[index].object {
            Object::Binary(payload) if point.value == 0 => *payload = Some(Payload::Signaled),
            Object::Timeline { completed, pending } => {
                // Do not let a CPU signal bypass unresolved GPU work. A later native queue
                // may relax this only with an explicit ordering proof. — MILVUS
                if !pending.is_empty() {
                    return Err(Error::Busy);
                }
                if point.value <= *completed {
                    return Err(Error::InvalidRequest);
                }
                *completed = point.value;
            }
            _ => return Err(Error::InvalidRequest),
        }
        Ok(())
    }

    /// None means no signal has been submitted for this value yet. Callers may check again
    /// after a producer publishes; this function neither blocks nor invents a wake channel.
    pub fn snapshot(&self, point: Point) -> Result<Option<Snapshot>, Error> {
        let index = self.index(point.sync)?;
        let payload = match &self.objects[index].object {
            Object::Binary(payload) if point.value == 0 => *payload,
            Object::Timeline { completed, pending } => {
                if point.value <= *completed {
                    Some(Payload::Signaled)
                } else {
                    pending
                        .iter()
                        .find(|(value, _)| *value >= point.value)
                        .map(|(_, fence)| Payload::Fence(*fence))
                }
            }
            _ => return Err(Error::InvalidRequest),
        };
        Ok(payload.map(|payload| Snapshot {
            owner: self.owner,
            payload,
        }))
    }

    /// Mesa WAIT_PENDING: available for signaling, which does not imply GPU completion.
    pub fn wait_pending(&self, point: Point) -> Result<bool, Error> {
        Ok(self.snapshot(point)?.is_some())
    }

    /// Publish all output signals only after Submit is acknowledged. Validation and every
    /// fallible reservation precede transport I/O; a refused Submit leaves payloads intact.
    /// No submit is retried after transport/protocol uncertainty. — MILVUS
    pub fn submit(&mut self, submission: Submission, signals: &[Point]) -> Result<Snapshot, Error> {
        self.ready()?;
        if !submission.valid_shape()
            || self.group.is_some_and(|group| group != submission.group)
            || signals.len() > MAX_SYNCS
        {
            return Err(Error::InvalidRequest);
        }
        for (n, point) in signals.iter().enumerate() {
            let index = self.index(point.sync)?;
            if signals[..n].iter().any(|other| other.sync == point.sync) {
                return Err(Error::InvalidRequest);
            }
            match &mut self.objects[index].object {
                Object::Binary(_) if point.value == 0 => (),
                Object::Timeline { completed, pending } => {
                    let last = pending.last().map_or(*completed, |(value, _)| *value);
                    if point.value <= last {
                        return Err(Error::InvalidRequest);
                    }
                    if pending.len() == MAX_TIMELINE_POINTS {
                        return Err(Error::NoMemory);
                    }
                    pending.try_reserve(1).map_err(|_| Error::NoMemory)?;
                }
                _ => return Err(Error::InvalidRequest),
            }
        }
        let fence = self.client.submit_fence(submission)?;
        self.group = Some(fence.group);
        for point in signals {
            // Every index was checked above; exclusive client access prevents reentry or
            // concurrent destruction between acceptance and metadata publication. — MILVUS
            let index = self.index(point.sync).expect("validated signal");
            match &mut self.objects[index].object {
                Object::Binary(payload) => *payload = Some(Payload::Fence(fence)),
                Object::Timeline { pending, .. } => pending.push((point.value, fence)),
            }
        }
        Ok(Snapshot {
            owner: self.owner,
            payload: Payload::Fence(fence),
        })
    }

    /// Register on G8c's gpu-done channel. Armed transfers its read end to the caller, who
    /// must use a bounded Port/timer wait and close it on every path. A wake or peer close
    /// alone is not completion: re-arm this snapshot and require Done with status zero.
    /// No SyncRead polling, OS wait policy, or implicit retry lives here. — MILVUS
    pub fn arm(&mut self, snapshot: Snapshot) -> Result<FenceStatus<T::Handle>, Error> {
        self.ready()?;
        if snapshot.owner != self.owner {
            return Err(Error::NotFound);
        }
        let Payload::Fence(fence) = snapshot.payload else {
            return Ok(FenceStatus::Done { gpu_status: 0 });
        };
        let state = self.client.fence_wait(fence)?;
        if let FenceStatus::Done { gpu_status } = state {
            if gpu_status != 0 {
                return Err(Error::GpuFault(gpu_status));
            }
            self.completed(fence);
        }
        Ok(state)
    }

    pub fn close_wait_handle(&mut self, handle: T::Handle) {
        // Both VMO grants and wake channels use Transport's one owned-handle close hook.
        self.client.close_buffer_handle(handle);
    }

    fn completed(&mut self, fence: Fence) {
        for record in &mut self.objects {
            match &mut record.object {
                Object::Binary(payload @ Some(Payload::Fence(_))) => {
                    if let Some(Payload::Fence(current)) = *payload {
                        if current.group == fence.group && current.point <= fence.point {
                            *payload = Some(Payload::Signaled);
                        }
                    }
                }
                Object::Timeline { completed, pending } => {
                    let count = pending
                        .iter()
                        .take_while(|(_, current)| {
                            current.group == fence.group && current.point <= fence.point
                        })
                        .count();
                    if count != 0 {
                        *completed = pending[count - 1].0;
                        pending.drain(..count);
                    }
                }
                _ => (),
            }
        }
    }
}

#[cfg(test)]
#[path = "sync_tests.rs"]
mod tests;
