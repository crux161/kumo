//j641
//j644

//! G7a's reusable native client boundary. No allocation, OS handles, MMIO or scheduling
//! policy lives here. One mutable client performs one request/reply at a time. A transport
//! or protocol failure ends this logical session: reconnect through the broker with a fresh
//! capability, recreate objects, and never replay an uncertain Submit. This is not hardware
//! reset, DMA reclamation, a GPU fence or a Mesa/DRM implementation. — KESTREL

use crate::{
    Binding, Command, DeviceInfo, DeviceTopology, Reply, Request, Response, Status, Submission,
    SyncState, REPLY_LEN, REQUEST_LEN,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportError {
    Closed,
    Failed,
}

/// The adapter owns its endpoint. A successful receive transfers at most one handle to
/// this client, including on malformed frames. On receive error, the adapter must close
/// any received handle itself. The extra byte prevents oversized-prefix acceptance.
pub trait Transport {
    type Handle;
    fn send(&mut self, bytes: &[u8; REQUEST_LEN]) -> Result<(), TransportError>;
    fn receive(
        &mut self,
        bytes: &mut [u8; REPLY_LEN + 1],
    ) -> Result<(usize, Option<Self::Handle>), TransportError>;
    fn close_handle(&mut self, handle: Self::Handle);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidRequest,
    RequestIdsExhausted,
    Transport(TransportError),
    Protocol,
    Server(Status),
    DeviceLost,
}

/// A successful allocation transfers its handle to the caller, which owns mapping/closing
/// it. The ID remains local to the current session; it is not a physical address.
#[derive(Debug, Eq, PartialEq)]
pub struct Buffer<H> {
    pub id: u32,
    pub bytes: u64,
    pub handle: H,
}

pub struct Client<T: Transport> {
    transport: T,
    id: u32,
    lost: bool,
}

impl<T: Transport> Client<T> {
    /// Bind once to a fresh brokered session. Constructing a new client over an old
    /// endpoint does not reset its server IDs or make replay safe. — KESTREL
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            id: 0,
            lost: false,
        }
    }
    pub fn is_lost(&self) -> bool {
        self.lost
    }

    pub fn info(&mut self) -> Result<DeviceInfo, Error> {
        let (Response::Info(info), _) = self.call(Command::GetInfo)? else {
            unreachable!()
        };
        Ok(info)
    }
    pub fn topology(&mut self) -> Result<DeviceTopology, Error> {
        let (Response::Topology(topology), _) = self.call(Command::GetTopology)? else {
            unreachable!()
        };
        Ok(topology)
    }
    pub fn create_vm(&mut self) -> Result<u32, Error> {
        let (Response::VmCreated { vm }, _) = self.call(Command::VmCreate)? else {
            unreachable!()
        };
        Ok(vm)
    }
    pub fn create_buffer(&mut self, bytes: u64) -> Result<Buffer<T::Handle>, Error> {
        let (Response::BufferCreated { buffer, bytes }, Some(handle)) =
            self.call(Command::BufferCreate { bytes })?
        else {
            unreachable!()
        };
        Ok(Buffer {
            id: buffer,
            bytes,
            handle,
        })
    }
    pub fn bind(&mut self, binding: Binding) -> Result<(), Error> {
        self.call(Command::VmBind(binding))?;
        Ok(())
    }
    pub fn create_group(&mut self, vm: u32) -> Result<u32, Error> {
        let (Response::GroupCreated { group }, _) = self.call(Command::GroupCreate { vm })? else {
            unreachable!()
        };
        Ok(group)
    }
    /// Acceptance only. A successful return does not permit buffer reuse.
    pub fn submit(&mut self, submission: Submission) -> Result<(), Error> {
        self.call(Command::Submit(submission))?;
        Ok(())
    }
    /// Preserve the observed GPU status. A failed completion is returned once and latches
    /// loss; subsequent calls perform no transport I/O. Pending never releases an image.
    pub fn sync_read(&mut self, group: u32, point: u64) -> Result<SyncState, Error> {
        let (Response::Sync { state, .. }, _) = self.call(Command::SyncRead { group, point })?
        else {
            unreachable!()
        };
        Ok(state)
    }

    fn fail(&mut self, error: Error) -> Error {
        self.lost = true;
        error
    }

    fn call(&mut self, command: Command) -> Result<(Response, Option<T::Handle>), Error> {
        if self.lost {
            return Err(Error::DeviceLost);
        }
        let id = self
            .id
            .checked_add(1)
            .ok_or_else(|| self.fail(Error::RequestIdsExhausted))?;
        let request = Request { id, command };
        let bytes = request.encode();
        if Request::decode(&bytes) != Some(request) {
            return Err(Error::InvalidRequest);
        }
        // Consume before sending; even a transport error must never reuse this request ID.
        // No operation, including Submit, is retried here. — KESTREL
        self.id = id;
        self.transport
            .send(&bytes)
            .map_err(|e| self.fail(Error::Transport(e)))?;
        let mut bytes = [0; REPLY_LEN + 1];
        let (len, handle) = self
            .transport
            .receive(&mut bytes)
            .map_err(|e| self.fail(Error::Transport(e)))?;
        let reply = bytes
            .get(..len)
            .and_then(Reply::decode)
            .filter(|r| r.id == id);
        let valid = reply.is_some_and(|r| match r.result {
            Ok(response) => {
                matches_response(command, response)
                    && response.handle_count() == usize::from(handle.is_some())
            }
            Err(_) => handle.is_none(),
        });
        if !valid {
            if let Some(handle) = handle {
                self.transport.close_handle(handle);
            }
            return Err(self.fail(Error::Protocol));
        }
        match reply.expect("validated reply").result {
            Err(status) => {
                if status == Status::DeviceLost {
                    self.lost = true;
                }
                Err(Error::Server(status))
            }
            Ok(response) => {
                if matches!(response, Response::Sync { state: SyncState::Complete { gpu_status }, .. } if gpu_status != 0)
                {
                    self.lost = true;
                }
                Ok((response, handle))
            }
        }
    }
}

fn matches_response(command: Command, response: Response) -> bool {
    match (command, response) {
        (Command::GetInfo, Response::Info(_))
        | (Command::GetTopology, Response::Topology(_))
        | (Command::VmCreate, Response::VmCreated { .. })
        | (Command::VmBind(_), Response::VmBound)
        | (Command::GroupCreate { .. }, Response::GroupCreated { .. }) => true,
        (
            Command::BufferCreate { bytes },
            Response::BufferCreated {
                bytes: received, ..
            },
        ) => bytes == received,
        (Command::Submit(job), Response::Submitted { point }) => job.signal_point == point,
        (
            Command::SyncRead { point, .. },
            Response::Sync {
                point: received, ..
            },
        ) => point == received,
        _ => false,
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
