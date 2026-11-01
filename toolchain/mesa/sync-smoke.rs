//j749
//! Native sync metadata over the actual codec/Session, with real kernel wake objects.
//! The GPU backend and completion producer are modeled; no Mali execution is claimed.
#![no_std]
#![no_main]
extern crate alloc;

use alloc::{rc::Rc, vec::Vec};
use core::cell::RefCell;
use kumo_abi::{Handle, Rights};
use kumo_gpu::{
    client::{Client, FenceStatus, SessionTransport, Transport, TransportError},
    session::{Backend, Session, SubmitError},
    sync::{Error, Point, Snapshot, SyncObjects},
    BindFlags, Binding, Command, DeviceInfo, Reply, Request, Response, Status, Submission,
    PAGE_BYTES, REPLY_LEN, REQUEST_LEN, VA_START,
};

kumo_rt::entry!(main);

fn handle(raw: u64) -> Handle {
    assert!(raw != 0 && raw <= u32::MAX as u64);
    Handle(raw as u32)
}
fn close(h: Handle) {
    assert_eq!(kumo_rt::handle_close(h), 0);
}
fn say(line: &[u8]) {
    let _ = kumo_rt::debug_write(line.as_ptr(), line.len());
}

#[derive(Default)]
struct Hardware {
    vmo: Option<Handle>,
    refuse: bool,
}
impl Backend for Hardware {
    fn info(&mut self) -> Result<DeviceInfo, Status> {
        Ok(DeviceInfo {
            gpu_id: 0xa8670005,
            cs_features: 0x7085f,
        })
    }
    fn create_vm(&mut self) -> Result<(), Status> {
        Ok(())
    }
    fn create_buffer(&mut self, _id: u32, bytes: u64) -> Result<(), Status> {
        assert_eq!(bytes, PAGE_BYTES);
        let mut pages = [0];
        self.vmo = Some(handle(kumo_rt::vmo_create_paged(bytes, &mut pages)));
        Ok(())
    }
    fn bind(&mut self, _binding: Binding) -> Result<(), Status> {
        Ok(())
    }
    fn unbind(&mut self, _bindings: &[Binding]) -> Result<(), Status> {
        Ok(())
    }
    fn destroy_buffer(&mut self, _id: u32, _bindings: &[Binding]) -> Result<(), Status> {
        close(self.vmo.take().unwrap());
        Ok(())
    }
    fn create_group(&mut self) -> Result<(), Status> {
        Ok(())
    }
    fn submit(&mut self, _job: Submission) -> Result<(), SubmitError> {
        if core::mem::take(&mut self.refuse) {
            Err(SubmitError::Unpublished(Status::NoMemory))
        } else {
            Ok(())
        }
    }
    fn retire(&mut self) {
        if let Some(vmo) = self.vmo.take() {
            close(vmo);
        }
    }
}

#[derive(Default)]
struct Server {
    session: Session,
    hardware: Hardware,
    calls: Vec<Command>,
    wakes: Vec<(u64, Handle)>,
    closed: bool,
}
struct Wire {
    server: Rc<RefCell<Server>>,
    reply: Option<Reply>,
    grant: Option<Handle>,
}
impl Transport for Wire {
    type Handle = Handle;
    fn send(&mut self, bytes: &[u8; REQUEST_LEN]) -> Result<(), TransportError> {
        let request = Request::decode(bytes).unwrap();
        let mut server = self.server.borrow_mut();
        assert!(!server.closed);
        server.calls.push(request.command);
        let Server {
            session, hardware, ..
        } = &mut *server;
        let result = session.dispatch(request, hardware);
        match result {
            Ok(Response::BufferCreated { .. }) => {
                self.grant = Some(handle(kumo_rt::handle_duplicate(
                    server.hardware.vmo.unwrap(),
                    Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER,
                )));
            }
            Ok(Response::WaitArmed { point }) => {
                let (read, write) = kumo_rt::channel_create_pair();
                self.grant = Some(handle(read));
                server.wakes.push((point, handle(write)));
            }
            _ => (),
        }
        self.reply = Some(Reply {
            id: request.id,
            result,
        });
        Ok(())
    }
    fn receive(
        &mut self,
        bytes: &mut [u8; REPLY_LEN + 1],
    ) -> Result<(usize, Option<Handle>), TransportError> {
        bytes[..REPLY_LEN].copy_from_slice(&self.reply.take().unwrap().encode());
        Ok((REPLY_LEN, self.grant.take()))
    }
    fn close_handle(&mut self, h: Handle) {
        close(h);
    }
}
impl SessionTransport for Wire {
    fn close_session(&mut self) {
        let mut server = self.server.borrow_mut();
        assert!(!server.closed);
        server.closed = true;
        // This backend owns no GPU; only its modeled quiescent branch may retire.
        // Actual reset-before-DMA-release remains the driver's obligation. — MILVUS
        let released = server.session.close();
        assert_eq!(released.buffers, 1);
        assert_eq!(released.bindings, 1);
        server.hardware.retire();
    }
}
fn complete(server: &Rc<RefCell<Server>>, point: u64, status: u32) {
    let wakes = {
        let mut server = server.borrow_mut();
        server.session.complete(point, status).unwrap();
        core::mem::take(&mut server.wakes)
    };
    for (value, write) in wakes {
        assert_eq!(value, point);
        if status == 0 {
            assert_eq!(kumo_rt::channel_write(write, [1u8].as_ptr(), 1), 0);
        }
        close(write);
    }
}
fn job(point: u64) -> Submission {
    Submission {
        group: 1,
        stream_va: VA_START,
        stream_bytes: 32,
        signal_point: point,
    }
}
fn point(sync: kumo_gpu::sync::SyncHandle, value: u64) -> Point {
    Point { sync, value }
}

fn wait(
    syncs: &mut SyncObjects<'_, Wire>,
    snapshot: Snapshot,
    server: &Rc<RefCell<Server>>,
    complete_now: bool,
) {
    let FenceStatus::Armed(wake) = syncs.arm(snapshot).unwrap() else {
        panic!()
    };
    let port = handle(kumo_rt::port_create());
    let timer = handle(kumo_rt::timer_create(if complete_now {
        5_000_000_000
    } else {
        2_000_000
    }));
    assert_eq!(kumo_rt::port_bind(port, wake), 0);
    assert_eq!(kumo_rt::port_bind(port, timer), 0);
    if complete_now {
        complete(server, 1, 0);
    }
    let source = kumo_rt::port_wait(port);
    if complete_now {
        assert_eq!(source, kumo_rt::handle_koid(wake));
        let mut byte = [0];
        assert_eq!(kumo_rt::channel_read(wake, byte.as_mut_ptr(), 1), 1);
        assert_eq!(byte, [1]);
    } else {
        assert_eq!(source, kumo_rt::handle_koid(timer));
    }
    assert_eq!(kumo_rt::port_unbind(port, wake), 0);
    assert_eq!(kumo_rt::port_unbind(port, timer), 0);
    close(timer);
    close(port);
    syncs.close_wait_handle(wake);
}

#[no_mangle]
extern "C" fn main() -> ! {
    say(b"G7p-a native sync begin\n");
    let server = Rc::new(RefCell::new(Server::default()));
    let mut client = Client::new(Wire {
        server: server.clone(),
        reply: None,
        grant: None,
    });
    assert_eq!(client.create_vm().unwrap(), 1);
    let buffer = client.create_buffer(PAGE_BYTES).unwrap();
    client
        .bind(Binding {
            vm: 1,
            buffer: buffer.id,
            va: VA_START,
            offset: 0,
            bytes: PAGE_BYTES,
            flags: BindFlags(BindFlags::READ_ONLY),
        })
        .unwrap();
    assert_eq!(client.create_group(1).unwrap(), 1);
    {
        let mut syncs = SyncObjects::new(&mut client).unwrap();
        let binary = point(syncs.create_binary(false).unwrap(), 0);
        let timeline = point(syncs.create_timeline(0).unwrap(), 42);
        assert!(!syncs.wait_pending(binary).unwrap());
        assert!(!syncs.wait_pending(timeline).unwrap());
        let first = syncs.submit(job(1), &[binary, timeline]).unwrap();
        let captured = syncs.snapshot(binary).unwrap().unwrap();
        assert!(syncs.wait_pending(binary).unwrap());
        assert!(syncs.wait_pending(point(timeline.sync, 41)).unwrap());
        assert!(!syncs.wait_pending(point(timeline.sync, 43)).unwrap());
        syncs.reset(binary.sync).unwrap();
        syncs.signal(binary).unwrap();
        let host = syncs.snapshot(binary).unwrap().unwrap();
        assert_eq!(syncs.signal(point(timeline.sync, 43)), Err(Error::Busy));
        wait(&mut syncs, captured, &server, true);
        assert_eq!(syncs.arm(first), Ok(FenceStatus::Done { gpu_status: 0 }));
        syncs.signal(point(timeline.sync, 43)).unwrap();
        say(b"G7p-a native sync: pending differs from done; reset preserves captured GPU fence; kernel wake and Port bound proved\n");

        server.borrow_mut().hardware.refuse = true;
        let output = point(timeline.sync, 100);
        assert!(syncs.submit(job(2), &[binary, output]).is_err());
        assert!(!syncs.wait_pending(output).unwrap());
        assert_eq!(syncs.arm(host), Ok(FenceStatus::Done { gpu_status: 0 }));
        let second = syncs.submit(job(2), &[binary, output]).unwrap();
        wait(&mut syncs, second, &server, false);
        assert_eq!(syncs.signal(point(timeline.sync, 101)), Err(Error::Busy));
        say(b"G7p-a native sync: refused submit preserves outputs; timer abandonment does not complete GPU work\n");
        complete(&server, 2, 0xbeef);
        syncs.destroy(binary.sync).unwrap();
        assert_eq!(syncs.arm(second), Err(Error::GpuFault(0xbeef)));
        let calls = server.borrow().calls.len();
        assert!(syncs.arm(host).is_err());
        assert!(syncs.submit(job(2), &[output]).is_err());
        assert!(syncs.wait_pending(output).is_err());
        assert_eq!(server.borrow().calls.len(), calls);
    }
    assert!(!server
        .borrow()
        .calls
        .iter()
        .any(|c| matches!(c, Command::SyncRead { .. })));
    assert_eq!(
        server
            .borrow()
            .calls
            .iter()
            .filter(|c| matches!(c, Command::Submit(_)))
            .count(),
        3
    );
    assert!(server.borrow().wakes.is_empty());
    client.close_buffer_handle(buffer.handle);
    client.close_session();
    client.close_session();
    assert!(server.borrow().closed);
    assert!(server.borrow().hardware.vmo.is_none());
    say(b"G7p-a native sync green: binary, timeline, accepted signals, snapshots, wait-pending, kernel wake/timeout, terminal loss; zero SyncRead\n");
    kumo_rt::process_exit(0)
}
