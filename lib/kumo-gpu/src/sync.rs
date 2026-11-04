//j749
//j750
//j751
//j779
//! Session-local native sync objects, without DRM handles or sync files. Calls are serial.
//! Pending means an accepted job owns a signal; only FenceWait Done proves completion.
//! Captured dependencies gate publication; the caller owns bounded OS waits.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::client::{Client, Error as ClientError, FenceStatus, Transport};
use crate::{Fence, MultiSubmission, Submission};

/// VK2-a: what one intended submission publishes: one stream (Submit), or up to three
/// subqueue streams under one point (SubmitStreams, VK1 D1). Waits and signals treat both
/// alike: the point is the one fence the signals carry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Work {
    Single(Submission),
    Streams(MultiSubmission),
}

impl Work {
    pub fn group(self) -> u32 {
        match self {
            Self::Single(s) => s.group,
            Self::Streams(m) => m.group,
        }
    }

    pub fn valid_shape(self) -> bool {
        match self {
            Self::Single(s) => s.valid_shape(),
            Self::Streams(m) => m.valid_shape(),
        }
    }
}

pub const MAX_SYNCS: usize = 64;
pub const MAX_TIMELINE_POINTS: usize = 256;
pub const MAX_SUBMIT_WAITS: usize = 64;

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidRequest,
    NotFound,
    NoMemory,
    Busy,
    Unsubmitted { wait_index: u32 },
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

/// Captured input payloads and validated output points for one intended submission.
/// Not cloneable: after acknowledgement this token cannot publish the job a second time.
/// Dropping it cancels only the intent, never the GPU jobs its dependencies name. — MILVUS
#[derive(Debug)]
pub struct PreparedSubmit {
    owner: u64,
    submission: Work,
    waits: Vec<Snapshot>,
    signals: Vec<Point>,
    cursor: usize,
    accepted: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub enum SubmitProgress<H> {
    /// No Submit was sent. The caller owns this wake and must close it on every path.
    /// Wake, timeout and peer close all require another step; none advances the cursor.
    Blocked {
        dependency: Snapshot,
        wake: H,
    },
    Submitted(Snapshot),
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
    state: State,
}

// The BO/device owner retains this metadata between serial callback borrows. No second
// Client, endpoint or request-ID sequence is created when lending the facade. — MILVUS
pub(crate) struct State {
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
            state: State {
                owner,
                next_id: 0,
                group: None,
                objects: Vec::new(),
            },
        })
    }

    pub(crate) fn with_state<R>(
        client: &'a mut Client<T>,
        state: &mut Option<State>,
        f: impl FnOnce(&mut Self) -> Result<R, Error>,
    ) -> Result<R, Error> {
        let mut facade = if let Some(state) = state.take() {
            Self { client, state }
        } else {
            Self::new(client)?
        };
        let result = f(&mut facade);
        *state = Some(facade.state);
        result
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
        if handle.owner != self.state.owner {
            return Err(Error::NotFound);
        }
        self.state
            .objects
            .iter()
            .position(|record| record.id == handle.id)
            .ok_or(Error::NotFound)
    }

    fn create(&mut self, object: Object) -> Result<SyncHandle, Error> {
        self.ready()?;
        if self.state.objects.len() == MAX_SYNCS {
            return Err(Error::NoMemory);
        }
        let id = self.state.next_id.checked_add(1).ok_or(Error::NoMemory)?;
        self.state
            .objects
            .try_reserve(1)
            .map_err(|_| Error::NoMemory)?;
        self.state.objects.push(Record { id, object });
        self.state.next_id = id;
        Ok(SyncHandle {
            owner: self.state.owner,
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
        self.state.objects.swap_remove(index);
        Ok(())
    }

    pub fn reset(&mut self, handle: SyncHandle) -> Result<(), Error> {
        let index = self.index(handle)?;
        let Object::Binary(payload) = &mut self.state.objects[index].object else {
            return Err(Error::InvalidRequest);
        };
        *payload = None;
        Ok(())
    }

    pub fn signal(&mut self, point: Point) -> Result<(), Error> {
        let index = self.index(point.sync)?;
        match &mut self.state.objects[index].object {
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
        let payload = match &self.state.objects[index].object {
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
            owner: self.state.owner,
            payload,
        }))
    }

    /// Mesa WAIT_PENDING: available for signaling, which does not imply GPU completion.
    pub fn wait_pending(&self, point: Point) -> Result<bool, Error> {
        Ok(self.snapshot(point)?.is_some())
    }

    fn reserve_signals(&mut self, submission: Work, signals: &[Point]) -> Result<(), Error> {
        self.ready()?;
        if !submission.valid_shape()
            || self
                .state
                .group
                .is_some_and(|group| group != submission.group())
            || signals.len() > MAX_SYNCS
        {
            return Err(Error::InvalidRequest);
        }
        for (n, point) in signals.iter().enumerate() {
            let index = self.index(point.sync)?;
            if signals[..n].iter().any(|other| other.sync == point.sync) {
                return Err(Error::InvalidRequest);
            }
            match &mut self.state.objects[index].object {
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
        Ok(())
    }

    /// Capture every input fence before returning, so resetting/replacing an input object
    /// while the caller waits cannot retarget the intended dependency. Unsubmitted values
    /// refuse locally; this lane has no deferred job queue or future-value wake service.
    /// Validation and allocation do not publish work or change output payloads. — MILVUS
    pub fn prepare(
        &mut self,
        submission: Submission,
        waits: &[Point],
        signals: &[Point],
    ) -> Result<PreparedSubmit, Error> {
        self.prepare_work(Work::Single(submission), waits, signals)
    }

    /// VK2-a: [`Self::prepare`] for several subqueue streams under one point.
    pub fn prepare_streams(
        &mut self,
        submission: MultiSubmission,
        waits: &[Point],
        signals: &[Point],
    ) -> Result<PreparedSubmit, Error> {
        self.prepare_work(Work::Streams(submission), waits, signals)
    }

    fn prepare_work(
        &mut self,
        submission: Work,
        waits: &[Point],
        signals: &[Point],
    ) -> Result<PreparedSubmit, Error> {
        self.ready()?;
        if waits.len() > MAX_SUBMIT_WAITS {
            return Err(Error::InvalidRequest);
        }
        self.reserve_signals(submission, signals)?;
        let mut captured = Vec::new();
        captured
            .try_reserve(waits.len())
            .map_err(|_| Error::NoMemory)?;
        for (index, point) in waits.iter().enumerate() {
            let snapshot = self.snapshot(*point)?.ok_or(Error::Unsubmitted {
                wait_index: index as u32,
            })?;
            if !captured.contains(&snapshot) {
                captured.push(snapshot);
            }
        }
        let mut outputs = Vec::new();
        outputs
            .try_reserve(signals.len())
            .map_err(|_| Error::NoMemory)?;
        outputs.extend_from_slice(signals);
        Ok(PreparedSubmit {
            owner: self.state.owner,
            submission,
            waits: captured,
            signals: outputs,
            cursor: 0,
            accepted: false,
        })
    }

    /// Advance only on authoritative Done/status 0 for each captured dependency, then
    /// publish one Submit and all its signals. A recoverable server refusal can be stepped
    /// again explicitly; acceptance consumes the intent and uncertainty latches Client loss.
    /// No OS wait, SyncRead spin or implicit Submit retry occurs here. — MILVUS
    pub fn submit_prepared(
        &mut self,
        prepared: &mut PreparedSubmit,
    ) -> Result<SubmitProgress<T::Handle>, Error> {
        self.ready()?;
        if prepared.owner != self.state.owner {
            return Err(Error::NotFound);
        }
        if prepared.accepted {
            return Err(Error::InvalidRequest);
        }
        // Other serial calls may have destroyed or advanced an output while the caller
        // waited. Revalidate before arming any dependency, and again before Submit.
        // — MILVUS
        self.reserve_signals(prepared.submission, &prepared.signals)?;
        while let Some(dependency) = prepared.waits.get(prepared.cursor).copied() {
            match self.arm(dependency)? {
                FenceStatus::Done { gpu_status: 0 } => prepared.cursor += 1,
                FenceStatus::Armed(wake) => {
                    return Ok(SubmitProgress::Blocked { dependency, wake });
                }
                FenceStatus::Done { .. } => unreachable!("arm rejects failed completion"),
            }
        }
        let snapshot = self.submit_work(prepared.submission, &prepared.signals)?;
        prepared.accepted = true;
        Ok(SubmitProgress::Submitted(snapshot))
    }

    /// Publish all output signals only after Submit is acknowledged. Validation and every
    /// fallible reservation precede transport I/O; a refused Submit leaves payloads intact.
    /// No submit is retried after transport/protocol uncertainty. — MILVUS
    pub fn submit(&mut self, submission: Submission, signals: &[Point]) -> Result<Snapshot, Error> {
        self.submit_work(Work::Single(submission), signals)
    }

    fn submit_work(&mut self, submission: Work, signals: &[Point]) -> Result<Snapshot, Error> {
        self.reserve_signals(submission, signals)?;
        let fence = match submission {
            Work::Single(s) => self.client.submit_fence(s)?,
            Work::Streams(m) => self.client.submit_streams_fence(m)?,
        };
        self.state.group = Some(fence.group);
        for point in signals {
            // Every index was checked above; exclusive client access prevents reentry or
            // concurrent destruction between acceptance and metadata publication. — MILVUS
            let index = self.index(point.sync).expect("validated signal");
            match &mut self.state.objects[index].object {
                Object::Binary(payload) => *payload = Some(Payload::Fence(fence)),
                Object::Timeline { pending, .. } => pending.push((point.value, fence)),
            }
        }
        Ok(Snapshot {
            owner: self.state.owner,
            payload: Payload::Fence(fence),
        })
    }

    /// Register on G8c's gpu-done channel. Armed transfers its read end to the caller, who
    /// must use a bounded Port/timer wait and close it on every path. A wake or peer close
    /// alone is not completion: re-arm this snapshot and require Done with status zero.
    /// No SyncRead polling, OS wait policy, or implicit retry lives here. — MILVUS
    pub fn arm(&mut self, snapshot: Snapshot) -> Result<FenceStatus<T::Handle>, Error> {
        self.ready()?;
        if snapshot.owner != self.state.owner {
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
        for record in &mut self.state.objects {
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
