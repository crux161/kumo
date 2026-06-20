#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use kumo_abi::{Errno, Handle, Status};
use kumo_ipc::{Message, MessageError};

pub const SORA_NAME: &str = "Sora";
pub const ROOT_SERVER_ORDINAL_ECHO: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartPolicy {
    Never,
    OnFailure,
    Always,
}

impl RestartPolicy {
    /// Decide whether a terminated instance should be reconstructed. A persistent
    /// service disappearing outside an intentional shutdown is a failure.
    pub const fn should_restart(self, failed: bool) -> bool {
        match self {
            Self::Never => false,
            Self::OnFailure => failed,
            Self::Always => true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServerRecipe<'a> {
    pub name: &'a str,
    pub image_path: &'a str,
    pub restart: RestartPolicy,
}

/// The capabilities Sora retains for one running supervised server.
/// The bootstrap endpoint is moved into the child during `ProcessRun`, so it is
/// deliberately absent here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupervisedServer {
    pub process: Handle,
    pub client: Handle,
}

/// One persistent supervisor record: the construction policy needed to rebuild a
/// service and the capabilities for its current live instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupervisedService<'a> {
    pub recipe: ServerRecipe<'a>,
    pub instance: SupervisedServer,
}

pub struct Sora;

impl Sora {
    pub const fn new() -> Self {
        Self
    }

    pub fn echo<'a>(&self, request: Message<'a>) -> Result<Message<'a>, MessageError> {
        Message::new(ROOT_SERVER_ORDINAL_ECHO, request.bytes, request.handles)
    }
}

impl kumo_rt::Server for Sora {
    fn name(&self) -> &'static str {
        SORA_NAME
    }

    fn dispatch(&mut self, _channel: Handle, _message: &[u8]) -> Status {
        Errno::Ok.status()
    }
}

/// Close every present handle, continuing after an error so one failed close
/// cannot strand the remaining authority. Returns whether every close succeeded.
///
/// The closer is injected to keep the ownership policy host-testable; freestanding
/// Sora passes `kumo_rt::handle_close`.
pub fn close_handles(handles: &[Option<Handle>], mut close: impl FnMut(Handle) -> Status) -> bool {
    close_handles_except(handles, &[], &mut close)
}

/// Close every present handle except those explicitly transferred to the caller.
/// This makes a constructor's ownership handoff visible: failed construction keeps
/// nothing, while success preserves only the returned handles.
pub fn close_handles_except(
    handles: &[Option<Handle>],
    keep: &[Handle],
    mut close: impl FnMut(Handle) -> Status,
) -> bool {
    let mut all_closed = true;
    for handle in handles.iter().flatten() {
        if keep.contains(handle) {
            continue;
        }
        if close(*handle) != Errno::Ok.status() {
            all_closed = false;
        }
    }
    all_closed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_logic_is_host_testable() {
        let sora = Sora::new();
        let request = Message::call(ROOT_SERVER_ORDINAL_ECHO, b"ping", &[]).unwrap();
        let reply = sora.echo(request).unwrap();
        assert_eq!(reply.bytes, b"ping");
        assert_eq!(reply.header.ordinal, ROOT_SERVER_ORDINAL_ECHO);

        let supervised = SupervisedServer {
            process: Handle(3),
            client: Handle(5),
        };
        assert_eq!(supervised.process, Handle(3));
        assert_eq!(supervised.client, Handle(5));

        let service = SupervisedService {
            recipe: ServerRecipe {
                name: "ttyd",
                image_path: "bin/ttyd",
                restart: RestartPolicy::OnFailure,
            },
            instance: supervised,
        };
        assert_eq!(service.recipe.image_path, "bin/ttyd");
        assert_eq!(service.recipe.restart, RestartPolicy::OnFailure);
        assert_eq!(service.instance.process, Handle(3));
        assert_eq!(service.instance.client, Handle(5));
        assert!(!RestartPolicy::Never.should_restart(false));
        assert!(!RestartPolicy::Never.should_restart(true));
        assert!(!RestartPolicy::OnFailure.should_restart(false));
        assert!(RestartPolicy::OnFailure.should_restart(true));
        assert!(RestartPolicy::Always.should_restart(false));
        assert!(RestartPolicy::Always.should_restart(true));

        let handles = [Some(Handle(7)), None, Some(Handle(11))];
        let mut seen = [Handle(0); 2];
        let mut count = 0;
        let all_closed = close_handles(&handles, |handle| {
            seen[count] = handle;
            count += 1;
            if handle == Handle(7) {
                Errno::BadHandle.status()
            } else {
                Errno::Ok.status()
            }
        });
        assert!(!all_closed);
        assert_eq!(seen, [Handle(7), Handle(11)]);

        let mut closed = Handle(0);
        assert!(close_handles_except(&handles, &[Handle(7)], |handle| {
            closed = handle;
            Errno::Ok.status()
        }));
        assert_eq!(closed, Handle(11));
    }
}
