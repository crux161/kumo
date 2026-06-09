use kumo_abi::{Errno, Handle, Rights, Status, Syscall};
use kumo_ipc::Message;

use crate::ipc::{IpcError, IpcRegistry, KernelMessage, PortPacket};
use crate::object::{ObjectError, ObjectManager};
use crate::task::Process;

#[derive(Clone, Copy, Debug)]
pub enum KernelCall<'a> {
    HandleClose {
        handle: Handle,
    },
    HandleDuplicate {
        handle: Handle,
        rights: Rights,
    },
    ChannelCreate,
    ChannelWrite {
        channel: Handle,
        message: Message<'a>,
    },
    ChannelRead {
        channel: Handle,
    },
    PortCreate,
    PortWait {
        port: Handle,
    },
    Unsupported(Syscall),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KernelCallResult {
    Status(Status),
    Handles { first: Handle, second: Handle },
    Handle(Handle),
    Message(KernelMessage),
    PortPacket(PortPacket),
}

#[derive(Clone, Debug, Default)]
pub struct SyscallEngine {
    objects: ObjectManager,
    ipc: IpcRegistry,
}

impl SyscallEngine {
    pub fn new() -> Self {
        Self {
            objects: ObjectManager::new(),
            ipc: IpcRegistry::new(),
        }
    }

    pub fn objects_mut(&mut self) -> &mut ObjectManager {
        &mut self.objects
    }

    pub fn dispatch(&mut self, process: &mut Process, call: KernelCall<'_>) -> KernelCallResult {
        match call {
            KernelCall::HandleClose { handle } => {
                let status = match process.handles_mut().close(handle) {
                    Ok(()) => Errno::Ok.status(),
                    Err(error) => errno_from_object(error).status(),
                };
                KernelCallResult::Status(status)
            }
            KernelCall::HandleDuplicate { handle, rights } => {
                match process.handles_mut().duplicate(handle, rights) {
                    Ok(handle) => KernelCallResult::Handle(handle),
                    Err(error) => KernelCallResult::Status(errno_from_object(error).status()),
                }
            }
            KernelCall::ChannelCreate => {
                match self.ipc.channel_create(&mut self.objects, process) {
                    Ok((first, second)) => KernelCallResult::Handles { first, second },
                    Err(error) => KernelCallResult::Status(errno_from_ipc(error).status()),
                }
            }
            KernelCall::ChannelWrite { channel, message } => {
                let status = match self.ipc.channel_write(process, channel, message) {
                    Ok(()) => Errno::Ok.status(),
                    Err(error) => errno_from_ipc(error).status(),
                };
                KernelCallResult::Status(status)
            }
            KernelCall::ChannelRead { channel } => match self.ipc.channel_read(process, channel) {
                Ok(message) => KernelCallResult::Message(message),
                Err(error) => KernelCallResult::Status(errno_from_ipc(error).status()),
            },
            KernelCall::PortCreate => match self.ipc.port_create(&mut self.objects, process) {
                Ok(handle) => KernelCallResult::Handle(handle),
                Err(error) => KernelCallResult::Status(errno_from_ipc(error).status()),
            },
            KernelCall::PortWait { port } => match self.ipc.port_wait(process, port) {
                Ok(packet) => KernelCallResult::PortPacket(packet),
                Err(error) => KernelCallResult::Status(errno_from_ipc(error).status()),
            },
            KernelCall::Unsupported(_) => KernelCallResult::Status(Errno::NotSupported.status()),
        }
    }

    pub fn port_queue_signal(
        &mut self,
        process: &Process,
        port: Handle,
        source: kumo_abi::KoId,
        signals: kumo_abi::Signals,
    ) -> Status {
        match self.ipc.port_queue_signal(process, port, source, signals) {
            Ok(()) => Errno::Ok.status(),
            Err(error) => errno_from_ipc(error).status(),
        }
    }

    pub fn channel_call<F>(
        &mut self,
        process: &mut Process,
        channel: Handle,
        request: Message<'_>,
        server: F,
    ) -> Result<KernelMessage, Errno>
    where
        F: FnOnce(KernelMessage) -> Result<KernelMessage, IpcError>,
    {
        self.ipc
            .channel_call(process, channel, request, server)
            .map_err(errno_from_ipc)
    }
}

pub const fn errno_from_ipc(error: IpcError) -> Errno {
    match error {
        IpcError::BadHandle | IpcError::NotChannel => Errno::BadHandle,
        IpcError::WrongType => Errno::WrongType,
        IpcError::AccessDenied => Errno::AccessDenied,
        IpcError::TableFull => Errno::NoMemory,
        IpcError::TooManyBytes | IpcError::TooManyHandles => Errno::InvalidArgs,
        IpcError::ShouldWait => Errno::ShouldWait,
        IpcError::PeerClosed => Errno::PeerClosed,
    }
}

pub const fn errno_from_object(error: ObjectError) -> Errno {
    match error {
        ObjectError::BadHandle => Errno::BadHandle,
        ObjectError::WrongType => Errno::WrongType,
        ObjectError::AccessDenied => Errno::AccessDenied,
        ObjectError::TableFull => Errno::NoMemory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kumo_abi::{ObjectKind, Rights};

    fn test_process(engine: &mut SyscallEngine) -> Process {
        let job = crate::task::Job::root(engine.objects_mut());
        let vmar = crate::mm::Vmar::new(0xffff_0000_0000_0000, crate::mm::PAGE_SIZE * 16).unwrap();
        Process::new(engine.objects_mut(), &job, vmar)
    }

    fn create_channel(engine: &mut SyscallEngine, process: &mut Process) -> (Handle, Handle) {
        match engine.dispatch(process, KernelCall::ChannelCreate) {
            KernelCallResult::Handles { first, second } => (first, second),
            other => panic!("expected channel handles, got {other:?}"),
        }
    }

    fn create_port(engine: &mut SyscallEngine, process: &mut Process) -> Handle {
        match engine.dispatch(process, KernelCall::PortCreate) {
            KernelCallResult::Handle(handle) => handle,
            other => panic!("expected port handle, got {other:?}"),
        }
    }

    #[test]
    fn maps_ipc_errors_to_abi_errno() {
        assert_eq!(errno_from_ipc(IpcError::BadHandle), Errno::BadHandle);
        assert_eq!(errno_from_ipc(IpcError::WrongType), Errno::WrongType);
        assert_eq!(errno_from_ipc(IpcError::AccessDenied), Errno::AccessDenied);
        assert_eq!(errno_from_ipc(IpcError::TableFull), Errno::NoMemory);
        assert_eq!(errno_from_ipc(IpcError::TooManyBytes), Errno::InvalidArgs);
        assert_eq!(errno_from_ipc(IpcError::TooManyHandles), Errno::InvalidArgs);
        assert_eq!(errno_from_ipc(IpcError::ShouldWait), Errno::ShouldWait);
        assert_eq!(errno_from_ipc(IpcError::PeerClosed), Errno::PeerClosed);
        assert_eq!(errno_from_ipc(IpcError::NotChannel), Errno::BadHandle);
    }

    #[test]
    fn maps_object_errors_to_abi_errno() {
        assert_eq!(errno_from_object(ObjectError::BadHandle), Errno::BadHandle);
        assert_eq!(errno_from_object(ObjectError::WrongType), Errno::WrongType);
        assert_eq!(
            errno_from_object(ObjectError::AccessDenied),
            Errno::AccessDenied
        );
        assert_eq!(errno_from_object(ObjectError::TableFull), Errno::NoMemory);
    }

    #[test]
    fn dispatches_handle_duplicate_and_close() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let (left, _right) = create_channel(&mut engine, &mut process);

        let duplicated = engine.dispatch(
            &mut process,
            KernelCall::HandleDuplicate {
                handle: left,
                rights: Rights::READ,
            },
        );
        let KernelCallResult::Handle(read_only) = duplicated else {
            panic!("expected duplicated handle");
        };
        assert!(process
            .handles()
            .require(read_only, ObjectKind::Channel, Rights::READ)
            .is_ok());
        assert_eq!(
            process
                .handles()
                .require(read_only, ObjectKind::Channel, Rights::WRITE),
            Err(ObjectError::AccessDenied)
        );

        let closed = engine.dispatch(&mut process, KernelCall::HandleClose { handle: read_only });
        assert_eq!(closed, KernelCallResult::Status(Errno::Ok.status()));
        assert_eq!(
            process.handles().get(read_only),
            Err(ObjectError::BadHandle)
        );
    }

    #[test]
    fn duplicate_cannot_widen_rights() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let (left, _right) = create_channel(&mut engine, &mut process);
        let read_only = process.handles_mut().duplicate(left, Rights::READ).unwrap();

        let widened = engine.dispatch(
            &mut process,
            KernelCall::HandleDuplicate {
                handle: read_only,
                rights: Rights::READ | Rights::WRITE,
            },
        );

        assert_eq!(
            widened,
            KernelCallResult::Status(Errno::AccessDenied.status())
        );
    }

    #[test]
    fn dispatches_channel_create_write_and_read() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let (left, right) = create_channel(&mut engine, &mut process);

        let write = engine.dispatch(
            &mut process,
            KernelCall::ChannelWrite {
                channel: left,
                message: Message::new(4, b"hello", &[]).unwrap(),
            },
        );
        assert_eq!(write, KernelCallResult::Status(Errno::Ok.status()));

        let read = engine.dispatch(&mut process, KernelCall::ChannelRead { channel: right });
        let KernelCallResult::Message(message) = read else {
            panic!("expected message");
        };
        assert_eq!(message.header.ordinal, 4);
        assert_eq!(message.bytes(), b"hello");
    }

    #[test]
    fn dispatch_reports_should_wait_on_empty_channel() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let (_left, right) = create_channel(&mut engine, &mut process);

        let read = engine.dispatch(&mut process, KernelCall::ChannelRead { channel: right });

        assert_eq!(read, KernelCallResult::Status(Errno::ShouldWait.status()));
    }

    #[test]
    fn dispatch_preserves_transfer_semantics() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let (left, right) = create_channel(&mut engine, &mut process);
        let event = engine.objects_mut().create(ObjectKind::Event);
        let event_handle = process
            .handles_mut()
            .insert(event, Rights::WAIT | Rights::TRANSFER)
            .unwrap();
        let transfers = [event_handle];

        let write = engine.dispatch(
            &mut process,
            KernelCall::ChannelWrite {
                channel: left,
                message: Message::new(5, b"event", &transfers).unwrap(),
            },
        );
        assert_eq!(write, KernelCallResult::Status(Errno::Ok.status()));
        assert_eq!(
            process.handles().get(event_handle),
            Err(crate::object::ObjectError::BadHandle)
        );

        let read = engine.dispatch(&mut process, KernelCall::ChannelRead { channel: right });
        let KernelCallResult::Message(message) = read else {
            panic!("expected message with transferred handle");
        };
        let received_handle = message.handles()[0];
        assert_ne!(received_handle, event_handle);
        assert!(process
            .handles()
            .require(received_handle, ObjectKind::Event, Rights::WAIT)
            .is_ok());
    }

    #[test]
    fn channel_call_facade_returns_server_reply() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let (left, _right) = create_channel(&mut engine, &mut process);

        let reply = engine
            .channel_call(
                &mut process,
                left,
                Message::call(7, b"ping", &[]).unwrap(),
                |request| {
                    assert_eq!(request.bytes(), b"ping");
                    KernelMessage::reply_to(&request, b"pong", &[])
                },
            )
            .unwrap();

        assert_eq!(reply.bytes(), b"pong");
    }

    #[test]
    fn dispatches_port_create_and_wait() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);
        let port = create_port(&mut engine, &mut process);

        assert_eq!(
            engine.dispatch(&mut process, KernelCall::PortWait { port }),
            KernelCallResult::Status(Errno::ShouldWait.status())
        );

        let source = kumo_abi::KoId(100);
        let status = engine.port_queue_signal(&process, port, source, kumo_abi::Signals::READABLE);
        assert_eq!(status, Errno::Ok.status());

        let result = engine.dispatch(&mut process, KernelCall::PortWait { port });
        let KernelCallResult::PortPacket(packet) = result else {
            panic!("expected port packet");
        };
        assert_eq!(packet.source, source);
        assert!(packet.signals.contains(kumo_abi::Signals::READABLE));
    }

    #[test]
    fn unsupported_syscalls_return_not_supported() {
        let mut engine = SyscallEngine::new();
        let mut process = test_process(&mut engine);

        let result = engine.dispatch(
            &mut process,
            KernelCall::Unsupported(Syscall::ProcessCreate),
        );

        assert_eq!(
            result,
            KernelCallResult::Status(Errno::NotSupported.status())
        );
    }
}
