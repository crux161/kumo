use alloc::vec::Vec;

use kumo_abi::{Handle, KoId, ObjectKind, Rights, Signals};
use kumo_ipc::{Message, MessageHeader, MAX_INLINE_BYTES, MAX_MESSAGE_HANDLES};

use crate::object::{HandleEntry, KernelObject, ObjectError, ObjectManager};
use crate::task::Process;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpcError {
    BadHandle,
    WrongType,
    AccessDenied,
    TableFull,
    TooManyBytes,
    TooManyHandles,
    ShouldWait,
    PeerClosed,
    NotChannel,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IpcReport {
    pub channels: usize,
    pub ports: usize,
    pub calls: usize,
    pub bytes: usize,
    pub handle_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KernelMessage {
    pub header: MessageHeader,
    bytes: Vec<u8>,
    handles: Vec<Handle>,
    transfers: Vec<HandleEntry>,
}

impl KernelMessage {
    pub fn new(ordinal: u32, bytes: &[u8], handles: &[Handle]) -> Result<Self, IpcError> {
        Self::from_borrowed(Message::new(ordinal, bytes, handles).map_err(map_message_error)?)
    }

    pub fn call(ordinal: u32, bytes: &[u8], handles: &[Handle]) -> Result<Self, IpcError> {
        Self::from_borrowed(Message::call(ordinal, bytes, handles).map_err(map_message_error)?)
    }

    pub fn reply_to(request: &Self, bytes: &[u8], handles: &[Handle]) -> Result<Self, IpcError> {
        let mut reply = Self::new(request.header.ordinal, bytes, handles)?;
        reply.header = reply.header.is_reply();
        Ok(reply)
    }

    pub fn from_borrowed(message: Message<'_>) -> Result<Self, IpcError> {
        if message.bytes.len() > MAX_INLINE_BYTES {
            return Err(IpcError::TooManyBytes);
        }
        if message.handles.len() > MAX_MESSAGE_HANDLES {
            return Err(IpcError::TooManyHandles);
        }

        Ok(Self {
            header: message.header,
            bytes: message.bytes.to_vec(),
            handles: message.handles.to_vec(),
            transfers: Vec::new(),
        })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn handles(&self) -> &[Handle] {
        &self.handles
    }

    fn attach_transfers(&mut self, transfers: Vec<HandleEntry>) {
        self.transfers = transfers;
    }

    fn install_transfers(&mut self, process: &mut Process) -> Result<(), IpcError> {
        let mut installed = Vec::new();
        for entry in &self.transfers {
            let handle = process.handles_mut().insert_entry(*entry)?;
            installed.push(handle);
        }
        self.handles = installed;
        self.header.handles_len = self.handles.len() as u32;
        self.transfers.clear();
        Ok(())
    }
}

impl From<ObjectError> for IpcError {
    fn from(error: ObjectError) -> Self {
        match error {
            ObjectError::BadHandle => Self::BadHandle,
            ObjectError::WrongType => Self::WrongType,
            ObjectError::AccessDenied => Self::AccessDenied,
            ObjectError::TableFull => Self::TableFull,
        }
    }
}

fn map_message_error(error: kumo_ipc::MessageError) -> IpcError {
    match error {
        kumo_ipc::MessageError::TooManyBytes => IpcError::TooManyBytes,
        kumo_ipc::MessageError::TooManyHandles => IpcError::TooManyHandles,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelEnd {
    Left,
    Right,
}

#[derive(Clone, Debug)]
struct EndpointState {
    object: KernelObject,
    inbox: Vec<KernelMessage>,
    open: bool,
}

#[derive(Clone, Debug)]
pub struct ChannelPair {
    left: EndpointState,
    right: EndpointState,
}

#[derive(Clone, Debug)]
pub struct IpcRegistry {
    channels: Vec<ChannelPair>,
    ports: Vec<Port>,
}

impl IpcRegistry {
    pub const fn new() -> Self {
        Self {
            channels: Vec::new(),
            ports: Vec::new(),
        }
    }

    /// Create a root channel: one endpoint goes to `process` as a handle, the other
    /// is retained by the kernel. Returns `(process_handle, channel_index, kernel_end)`.
    /// The kernel reads/writes `kernel_end` via [`channel_pair_mut`](Self::channel_pair_mut).
    pub fn root_channel_create(
        &mut self,
        objects: &mut ObjectManager,
        process: &mut Process,
    ) -> Result<(Handle, usize, ChannelEnd), IpcError> {
        let channel = ChannelPair::new(objects);
        let right = process.handles_mut().insert(
            channel.object(ChannelEnd::Right),
            Rights::READ | Rights::WRITE | Rights::TRANSFER | Rights::DUPLICATE | Rights::WAIT,
        )?;
        let index = self.channels.len();
        self.channels.push(channel);
        Ok((right, index, ChannelEnd::Left))
    }

    pub fn channel_create(
        &mut self,
        objects: &mut ObjectManager,
        process: &mut Process,
    ) -> Result<(Handle, Handle), IpcError> {
        let channel = ChannelPair::new(objects);
        let left = process.handles_mut().insert(
            channel.object(ChannelEnd::Left),
            Rights::READ | Rights::WRITE | Rights::TRANSFER | Rights::DUPLICATE | Rights::WAIT,
        )?;
        let right = process.handles_mut().insert(
            channel.object(ChannelEnd::Right),
            Rights::READ | Rights::WRITE | Rights::TRANSFER | Rights::DUPLICATE | Rights::WAIT,
        )?;
        self.channels.push(channel);
        Ok((left, right))
    }

    pub fn port_create(
        &mut self,
        objects: &mut ObjectManager,
        process: &mut Process,
    ) -> Result<Handle, IpcError> {
        let port = Port::new(objects);
        let handle = process.handles_mut().insert(
            port.object(),
            Rights::WAIT | Rights::TRANSFER | Rights::DUPLICATE,
        )?;
        self.ports.push(port);
        Ok(handle)
    }

    pub fn port_wait(&mut self, process: &Process, port: Handle) -> Result<PortPacket, IpcError> {
        let port_entry = process
            .handles()
            .require(port, ObjectKind::Port, Rights::WAIT)?;
        self.port_mut_by_koid(port_entry.koid)?.wait()
    }

    pub fn port_queue_signal(
        &mut self,
        process: &Process,
        port: Handle,
        source: KoId,
        signals: Signals,
    ) -> Result<(), IpcError> {
        let port_entry = process
            .handles()
            .require(port, ObjectKind::Port, Rights::WAIT)?;
        self.port_mut_by_koid(port_entry.koid)?
            .queue_signal(source, signals);
        Ok(())
    }

    /// Signal a port directly by koid (no process handle lookup). Used by
    /// port-channel bindings when a message arrives on a bound channel.
    pub fn port_queue_signal_by_koid(&mut self, port_koid: KoId, source: KoId, signals: Signals) {
        if let Ok(port) = self.port_mut_by_koid(port_koid) {
            port.queue_signal(source, signals);
        }
    }

    pub fn channel_write(
        &mut self,
        process: &mut Process,
        channel: Handle,
        message: Message<'_>,
    ) -> Result<(), IpcError> {
        let channel_entry =
            process
                .handles()
                .require(channel, ObjectKind::Channel, Rights::WRITE)?;
        let end = self.channel_end_for(channel_entry.koid)?;
        let mut transfers = Vec::new();
        for handle in message.handles {
            let entry = process.handles().get(*handle)?;
            if !entry.rights.contains(Rights::TRANSFER) {
                return Err(IpcError::AccessDenied);
            }
            transfers.push(entry);
        }

        let mut message = KernelMessage::from_borrowed(message)?;
        message.attach_transfers(transfers.clone());
        let channel = self.channel_mut_by_koid(channel_entry.koid)?;
        channel.write(end, message)?;

        for entry in transfers {
            process.handles_mut().remove(entry.handle)?;
        }
        Ok(())
    }

    pub fn channel_read(
        &mut self,
        process: &mut Process,
        channel: Handle,
    ) -> Result<KernelMessage, IpcError> {
        let channel_entry =
            process
                .handles()
                .require(channel, ObjectKind::Channel, Rights::READ)?;
        let end = self.channel_end_for(channel_entry.koid)?;
        let channel = self.channel_mut_by_koid(channel_entry.koid)?;
        let mut message = channel.read(end)?;
        message.install_transfers(process)?;
        Ok(message)
    }

    pub fn channel_call<F>(
        &mut self,
        process: &mut Process,
        channel: Handle,
        request: Message<'_>,
        server: F,
    ) -> Result<KernelMessage, IpcError>
    where
        F: FnOnce(KernelMessage) -> Result<KernelMessage, IpcError>,
    {
        let channel_entry = process.handles().require(
            channel,
            ObjectKind::Channel,
            Rights::READ | Rights::WRITE,
        )?;
        let end = self.channel_end_for(channel_entry.koid)?;
        let request = KernelMessage::from_borrowed(request)?;
        let channel = self.channel_mut_by_koid(channel_entry.koid)?;
        channel.call(end, request, server)
    }

    fn channel_end_for(&self, koid: KoId) -> Result<ChannelEnd, IpcError> {
        for channel in &self.channels {
            if channel.object(ChannelEnd::Left).koid() == koid {
                return Ok(ChannelEnd::Left);
            }
            if channel.object(ChannelEnd::Right).koid() == koid {
                return Ok(ChannelEnd::Right);
            }
        }
        Err(IpcError::NotChannel)
    }

    pub fn peer_koid_for(&self, koid: KoId) -> Result<KoId, IpcError> {
        for channel in &self.channels {
            if channel.object(ChannelEnd::Left).koid() == koid {
                return Ok(channel.object(ChannelEnd::Right).koid());
            }
            if channel.object(ChannelEnd::Right).koid() == koid {
                return Ok(channel.object(ChannelEnd::Left).koid());
            }
        }
        Err(IpcError::NotChannel)
    }

    /// Access a [`ChannelPair`] by its index in the registry (returned by
    /// [`channel_create`](Self::channel_create)). Used by the kernel to read/write
    /// its own channel endpoint directly, without going through a handle table.
    pub fn channel_pair_mut(&mut self, index: usize) -> Option<&mut ChannelPair> {
        self.channels.get_mut(index)
    }

    fn channel_mut_by_koid(&mut self, koid: KoId) -> Result<&mut ChannelPair, IpcError> {
        for channel in &mut self.channels {
            if channel.object(ChannelEnd::Left).koid() == koid
                || channel.object(ChannelEnd::Right).koid() == koid
            {
                return Ok(channel);
            }
        }
        Err(IpcError::NotChannel)
    }

    fn port_mut_by_koid(&mut self, koid: KoId) -> Result<&mut Port, IpcError> {
        for port in &mut self.ports {
            if port.object().koid() == koid {
                return Ok(port);
            }
        }
        Err(IpcError::BadHandle)
    }

    pub fn close_by_koid(&mut self, koid: KoId) -> Result<Option<KoId>, IpcError> {
        let end = self.channel_end_for(koid)?;
        let channel = self.channel_mut_by_koid(koid)?;
        channel.close(end);
        let peer_end = peer(end);
        if channel.endpoint(peer_end).open {
            Ok(Some(channel.object(peer_end).koid()))
        } else {
            Ok(None)
        }
    }
}

impl Default for IpcRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ChannelPair {
    pub fn new(objects: &mut ObjectManager) -> Self {
        Self {
            left: EndpointState {
                object: objects.create(ObjectKind::Channel),
                inbox: Vec::new(),
                open: true,
            },
            right: EndpointState {
                object: objects.create(ObjectKind::Channel),
                inbox: Vec::new(),
                open: true,
            },
        }
    }

    pub fn object(&self, end: ChannelEnd) -> KernelObject {
        self.endpoint(end).object
    }

    pub fn peer_object(&self, end: ChannelEnd) -> KernelObject {
        self.endpoint(peer(end)).object
    }

    pub fn signals(&self, end: ChannelEnd) -> Signals {
        let endpoint = self.endpoint(end);
        let peer = self.endpoint(peer(end));
        let mut signals = Signals::empty();
        if !endpoint.inbox.is_empty() {
            signals |= Signals::READABLE;
        }
        if peer.open {
            signals |= Signals::WRITABLE;
        } else {
            signals |= Signals::PEER_CLOSED;
        }
        signals
    }

    pub fn write(&mut self, from: ChannelEnd, message: KernelMessage) -> Result<(), IpcError> {
        if !self.endpoint(peer(from)).open {
            return Err(IpcError::PeerClosed);
        }
        self.endpoint_mut(peer(from)).inbox.push(message);
        Ok(())
    }

    pub fn read(&mut self, end: ChannelEnd) -> Result<KernelMessage, IpcError> {
        let endpoint = self.endpoint_mut(end);
        if endpoint.inbox.is_empty() {
            Err(IpcError::ShouldWait)
        } else {
            Ok(endpoint.inbox.remove(0))
        }
    }

    pub fn call<F>(
        &mut self,
        from: ChannelEnd,
        request: KernelMessage,
        server: F,
    ) -> Result<KernelMessage, IpcError>
    where
        F: FnOnce(KernelMessage) -> Result<KernelMessage, IpcError>,
    {
        self.write(from, request)?;
        let request = self.read(peer(from))?;
        let mut reply = server(request)?;
        reply.header = reply.header.is_reply();
        self.write(peer(from), reply)?;
        self.read(from)
    }

    pub fn close(&mut self, end: ChannelEnd) {
        self.endpoint_mut(end).open = false;
    }

    fn endpoint(&self, end: ChannelEnd) -> &EndpointState {
        match end {
            ChannelEnd::Left => &self.left,
            ChannelEnd::Right => &self.right,
        }
    }

    fn endpoint_mut(&mut self, end: ChannelEnd) -> &mut EndpointState {
        match end {
            ChannelEnd::Left => &mut self.left,
            ChannelEnd::Right => &mut self.right,
        }
    }
}

fn peer(end: ChannelEnd) -> ChannelEnd {
    match end {
        ChannelEnd::Left => ChannelEnd::Right,
        ChannelEnd::Right => ChannelEnd::Left,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PortPacket {
    pub source: KoId,
    pub signals: Signals,
}

#[derive(Clone, Debug)]
pub struct Port {
    object: KernelObject,
    packets: Vec<PortPacket>,
}

impl Port {
    pub fn new(objects: &mut ObjectManager) -> Self {
        Self {
            object: objects.create(ObjectKind::Port),
            packets: Vec::new(),
        }
    }

    pub const fn object(&self) -> KernelObject {
        self.object
    }

    pub fn queue_signal(&mut self, source: KoId, signals: Signals) {
        self.packets.push(PortPacket { source, signals });
    }

    pub fn wait(&mut self) -> Result<PortPacket, IpcError> {
        if self.packets.is_empty() {
            Err(IpcError::ShouldWait)
        } else {
            Ok(self.packets.remove(0))
        }
    }

    pub fn signals(&self) -> Signals {
        if self.packets.is_empty() {
            Signals::empty()
        } else {
            Signals::READABLE
        }
    }
}

pub fn smoke() -> Result<IpcReport, IpcError> {
    let mut objects = ObjectManager::new();
    let mut channel = ChannelPair::new(&mut objects);
    let mut port = Port::new(&mut objects);

    let payload = b"sora?";
    let handles = [Handle(11), Handle(12)];
    channel.write(ChannelEnd::Left, KernelMessage::new(1, payload, &handles)?)?;
    let received = channel.read(ChannelEnd::Right)?;

    let request = KernelMessage::call(2, b"ping", &[])?;
    let reply = channel.call(ChannelEnd::Left, request, |request| {
        KernelMessage::reply_to(&request, b"pong", &[])
    })?;

    port.queue_signal(channel.object(ChannelEnd::Left).koid(), Signals::READABLE);
    let _packet = port.wait()?;

    Ok(IpcReport {
        channels: 1,
        ports: 1,
        calls: 1,
        bytes: received.bytes().len() + reply.bytes().len(),
        handle_count: received.handles().len() + reply.handles().len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kumo_abi::{Rights, INVALID_HANDLE};

    fn test_process(objects: &mut ObjectManager) -> Process {
        let job = crate::task::Job::root(objects);
        let vmar = crate::mm::Vmar::new(0xffff_0000_0000_0000, crate::mm::PAGE_SIZE * 16).unwrap();
        Process::new(objects, &job, vmar)
    }

    #[test]
    fn channel_moves_messages_to_the_peer_inbox() {
        let mut objects = ObjectManager::new();
        let mut channel = ChannelPair::new(&mut objects);
        let message = KernelMessage::new(7, b"ping", &[Handle(42)]).unwrap();

        assert!(channel
            .signals(ChannelEnd::Left)
            .contains(Signals::WRITABLE));
        assert_eq!(channel.read(ChannelEnd::Right), Err(IpcError::ShouldWait));

        channel.write(ChannelEnd::Left, message).unwrap();
        assert!(channel
            .signals(ChannelEnd::Right)
            .contains(Signals::READABLE));

        let received = channel.read(ChannelEnd::Right).unwrap();
        assert_eq!(received.header.ordinal, 7);
        assert_eq!(received.bytes(), b"ping");
        assert_eq!(received.handles(), &[Handle(42)]);
        assert!(!channel
            .signals(ChannelEnd::Right)
            .contains(Signals::READABLE));
    }

    #[test]
    fn channel_close_reports_peer_closed_and_rejects_writes() {
        let mut objects = ObjectManager::new();
        let mut channel = ChannelPair::new(&mut objects);

        channel.close(ChannelEnd::Right);

        assert!(channel
            .signals(ChannelEnd::Left)
            .contains(Signals::PEER_CLOSED));
        assert_eq!(
            channel.write(
                ChannelEnd::Left,
                KernelMessage::new(1, b"lost", &[]).unwrap()
            ),
            Err(IpcError::PeerClosed)
        );
    }

    #[test]
    fn channel_call_round_trips_a_reply() {
        let mut objects = ObjectManager::new();
        let mut channel = ChannelPair::new(&mut objects);
        let request = KernelMessage::call(9, b"hello", &[]).unwrap();

        let reply = channel
            .call(ChannelEnd::Left, request, |request| {
                assert_eq!(request.bytes(), b"hello");
                KernelMessage::reply_to(&request, b"world", &[])
            })
            .unwrap();

        assert_eq!(reply.header.ordinal, 9);
        assert!(reply.header.flags & MessageHeader::FLAG_IS_REPLY != 0);
        assert_eq!(reply.bytes(), b"world");
    }

    #[test]
    fn channel_objects_can_be_installed_as_capabilities() {
        let mut objects = ObjectManager::new();
        let channel = ChannelPair::new(&mut objects);
        let mut handles = crate::object::HandleTable::new();

        let left = handles
            .insert(
                channel.object(ChannelEnd::Left),
                Rights::READ | Rights::WRITE | Rights::TRANSFER,
            )
            .unwrap();
        let right = handles
            .insert(channel.peer_object(ChannelEnd::Left), Rights::READ)
            .unwrap();

        assert_ne!(left, INVALID_HANDLE);
        assert!(handles
            .require(left, ObjectKind::Channel, Rights::WRITE)
            .is_ok());
        assert_eq!(
            handles.require(right, ObjectKind::Channel, Rights::WRITE),
            Err(crate::object::ObjectError::AccessDenied)
        );
    }

    #[test]
    fn port_queues_signal_packets_fifo() {
        let mut objects = ObjectManager::new();
        let mut port = Port::new(&mut objects);
        let source = KoId(55);

        assert_eq!(port.wait(), Err(IpcError::ShouldWait));
        port.queue_signal(source, Signals::READABLE | Signals::PEER_CLOSED);
        assert!(port.signals().contains(Signals::READABLE));

        let packet = port.wait().unwrap();
        assert_eq!(packet.source, source);
        assert!(packet.signals.contains(Signals::READABLE));
        assert!(packet.signals.contains(Signals::PEER_CLOSED));
    }

    #[test]
    fn registry_creates_channel_endpoint_handles() {
        let mut objects = ObjectManager::new();
        let mut process = test_process(&mut objects);
        let mut ipc = IpcRegistry::new();

        let (left, right) = ipc.channel_create(&mut objects, &mut process).unwrap();

        assert!(process
            .handles()
            .require(left, ObjectKind::Channel, Rights::READ | Rights::WRITE)
            .is_ok());
        assert!(process
            .handles()
            .require(right, ObjectKind::Channel, Rights::TRANSFER)
            .is_ok());
        assert_eq!(process.handles().live_count(), 2);
    }

    #[test]
    fn registry_write_requires_channel_write_right() {
        let mut objects = ObjectManager::new();
        let mut process = test_process(&mut objects);
        let mut ipc = IpcRegistry::new();
        let (left, _right) = ipc.channel_create(&mut objects, &mut process).unwrap();
        let read_only = process.handles_mut().duplicate(left, Rights::READ).unwrap();
        let msg = Message::new(1, b"nope", &[]).unwrap();

        assert_eq!(
            ipc.channel_write(&mut process, read_only, msg),
            Err(IpcError::AccessDenied)
        );
    }

    #[test]
    fn registry_transfers_handles_by_consuming_sender_capabilities() {
        let mut objects = ObjectManager::new();
        let mut process = test_process(&mut objects);
        let mut ipc = IpcRegistry::new();
        let (left, right) = ipc.channel_create(&mut objects, &mut process).unwrap();
        let event = objects.create(ObjectKind::Event);
        let event_handle = process
            .handles_mut()
            .insert(event, Rights::WAIT | Rights::TRANSFER)
            .unwrap();
        let transfer_handles = [event_handle];
        let msg = Message::new(3, b"cap", &transfer_handles).unwrap();

        ipc.channel_write(&mut process, left, msg).unwrap();

        assert_eq!(
            process.handles().get(event_handle),
            Err(ObjectError::BadHandle)
        );
        let received = ipc.channel_read(&mut process, right).unwrap();
        assert_eq!(received.bytes(), b"cap");
        assert_eq!(received.handles().len(), 1);
        let received_handle = received.handles()[0];
        assert_ne!(received_handle, event_handle);
        assert!(process
            .handles()
            .require(received_handle, ObjectKind::Event, Rights::WAIT)
            .is_ok());
    }

    #[test]
    fn registry_rejects_transfer_without_transfer_right() {
        let mut objects = ObjectManager::new();
        let mut process = test_process(&mut objects);
        let mut ipc = IpcRegistry::new();
        let (left, _right) = ipc.channel_create(&mut objects, &mut process).unwrap();
        let event = objects.create(ObjectKind::Event);
        let event_handle = process.handles_mut().insert(event, Rights::WAIT).unwrap();
        let transfer_handles = [event_handle];
        let msg = Message::new(3, b"cap", &transfer_handles).unwrap();

        assert_eq!(
            ipc.channel_write(&mut process, left, msg),
            Err(IpcError::AccessDenied)
        );
        assert!(process.handles().get(event_handle).is_ok());
    }

    #[test]
    fn registry_channel_call_enforces_read_write_and_returns_reply() {
        let mut objects = ObjectManager::new();
        let mut process = test_process(&mut objects);
        let mut ipc = IpcRegistry::new();
        let (left, _right) = ipc.channel_create(&mut objects, &mut process).unwrap();
        let request = Message::call(99, b"ping", &[]).unwrap();

        let reply = ipc
            .channel_call(&mut process, left, request, |request| {
                assert_eq!(request.bytes(), b"ping");
                KernelMessage::reply_to(&request, b"pong", &[])
            })
            .unwrap();

        assert_eq!(reply.bytes(), b"pong");
        assert!(reply.header.flags & MessageHeader::FLAG_IS_REPLY != 0);
    }

    #[test]
    fn registry_creates_and_waits_on_ports() {
        let mut objects = ObjectManager::new();
        let mut process = test_process(&mut objects);
        let mut ipc = IpcRegistry::new();
        let port = ipc.port_create(&mut objects, &mut process).unwrap();
        let source = KoId(77);

        assert!(process
            .handles()
            .require(port, ObjectKind::Port, Rights::WAIT)
            .is_ok());
        assert_eq!(ipc.port_wait(&process, port), Err(IpcError::ShouldWait));

        ipc.port_queue_signal(&process, port, source, Signals::READABLE)
            .unwrap();
        let packet = ipc.port_wait(&process, port).unwrap();
        assert_eq!(packet.source, source);
        assert!(packet.signals.contains(Signals::READABLE));
    }

    #[test]
    fn registry_port_wait_requires_wait_right() {
        let mut objects = ObjectManager::new();
        let mut process = test_process(&mut objects);
        let mut ipc = IpcRegistry::new();
        let port = ipc.port_create(&mut objects, &mut process).unwrap();
        let transfer_only = process
            .handles_mut()
            .duplicate(port, Rights::TRANSFER)
            .unwrap();

        assert_eq!(
            ipc.port_wait(&process, transfer_only),
            Err(IpcError::AccessDenied)
        );
    }

    #[test]
    fn smoke_exercises_channel_call_and_port() {
        let report = smoke().unwrap();
        assert_eq!(report.channels, 1);
        assert_eq!(report.ports, 1);
        assert_eq!(report.calls, 1);
        assert_eq!(report.bytes, 9);
        assert_eq!(report.handle_count, 2);
    }
}
