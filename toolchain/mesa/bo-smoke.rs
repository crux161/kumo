//j744
//! The C BO bridge over the real codec/Session, modeled GPU service and real kernel VMOs.
#![no_std]
#![no_main]
extern crate alloc;
use alloc::{rc::Rc, vec, vec::Vec};
use core::{
    cell::{Cell, RefCell},
    ptr::NonNull,
};
use kumo_abi::{Handle, Rights};
use kumo_gpu::{
    bo::{Buffers, MapError, Mapper},
    c::{with_buffer_api, Api},
    client::{Client, Transport, TransportError},
    session::{Backend, Session, SubmitError},
    Binding, DeviceInfo, Reply, Request, Response, Status, Submission, PAGE_BYTES, REPLY_LEN,
    REQUEST_LEN,
};
mod native;
kumo_rt::entry!(main);
extern "C" {
    fn kumo_mesa_bo_proof(api: *const Api) -> u32;
}

fn handle(raw: u64) -> Handle {
    assert!(raw != 0 && raw <= u32::MAX as u64);
    Handle(raw as u32)
}
#[derive(Default)]
struct Stats {
    calls: Cell<u32>,
    closed: Cell<u32>,
    maps: Cell<u32>,
    unmaps: Cell<u32>,
}
struct Hardware {
    buffers: Vec<(u32, Handle)>,
}
impl Backend for Hardware {
    fn info(&mut self) -> Result<DeviceInfo, Status> {
        Err(Status::NotSupported)
    }
    fn create_vm(&mut self) -> Result<(), Status> {
        Ok(())
    }
    fn create_buffer(&mut self, id: u32, bytes: u64) -> Result<(), Status> {
        let mut pages = vec![0; (bytes / PAGE_BYTES) as usize];
        let vmo = handle(kumo_rt::vmo_create_paged(bytes, &mut pages));
        self.buffers.push((id, vmo));
        Ok(())
    }
    fn destroy_buffer(&mut self, id: u32, bindings: &[Binding]) -> Result<(), Status> {
        assert!(bindings.is_empty());
        let index = self
            .buffers
            .iter()
            .position(|record| record.0 == id)
            .ok_or(Status::NotFound)?;
        let (_, vmo) = self.buffers.swap_remove(index);
        assert_eq!(kumo_rt::handle_close(vmo), 0);
        Ok(())
    }
    fn bind(&mut self, _: Binding) -> Result<(), Status> {
        Err(Status::NotSupported)
    }
    fn unbind(&mut self, _: &[Binding]) -> Result<(), Status> {
        Err(Status::NotSupported)
    }
    fn create_group(&mut self) -> Result<(), Status> {
        Err(Status::NotSupported)
    }
    fn submit(&mut self, _: Submission) -> Result<(), SubmitError> {
        Err(SubmitError::Unpublished(Status::NotSupported))
    }
}
struct Server {
    session: Session,
    hardware: Hardware,
}
struct Wire {
    server: Rc<RefCell<Server>>,
    stats: Rc<Stats>,
    reply: Option<Reply>,
    grant: Option<Handle>,
}
impl Transport for Wire {
    type Handle = Handle;
    fn send(&mut self, bytes: &[u8; REQUEST_LEN]) -> Result<(), TransportError> {
        let request = Request::decode(bytes).unwrap();
        self.stats.calls.set(self.stats.calls.get() + 1);
        let mut server = self.server.borrow_mut();
        let Server { session, hardware } = &mut *server;
        let result = session.dispatch(request, hardware);
        if let Ok(Response::BufferCreated { buffer, .. }) = result {
            let vmo = hardware
                .buffers
                .iter()
                .find(|record| record.0 == buffer)
                .unwrap()
                .1;
            self.grant = Some(handle(kumo_rt::handle_duplicate(
                vmo,
                Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER,
            )));
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
    fn close_handle(&mut self, vmo: Handle) {
        assert_eq!(kumo_rt::handle_close(vmo), 0);
        self.stats.closed.set(self.stats.closed.get() + 1);
    }
}
struct Memory(Rc<Stats>);
impl Mapper<Handle> for Memory {
    fn map(&mut self, vmo: &Handle, bytes: u64) -> Result<NonNull<u8>, MapError> {
        let address = native::Memory.map(vmo, bytes)?;
        self.0.maps.set(self.0.maps.get() + 1);
        Ok(address)
    }
    fn unmap(&mut self, address: NonNull<u8>, bytes: u64) -> Result<(), MapError> {
        native::Memory.unmap(address, bytes)?;
        self.0.unmaps.set(self.0.unmaps.get() + 1);
        Ok(())
    }
}
fn say(text: &[u8]) {
    let _ = kumo_rt::debug_write(text.as_ptr(), text.len());
}
#[no_mangle]
extern "C" fn main() -> ! {
    say(b"G7o-c native BOs begin: modeled GPU service, real paged VMOs\n");
    let server = Rc::new(RefCell::new(Server {
        session: Session::default(),
        hardware: Hardware {
            buffers: Vec::new(),
        },
    }));
    let stats = Rc::new(Stats::default());
    let mut client = Client::new(Wire {
        server: server.clone(),
        stats: stats.clone(),
        reply: None,
        grant: None,
    });
    client.create_vm().unwrap();
    let mut memory = Memory(stats.clone());
    {
        let mut buffers = Buffers::new(&mut client, &mut memory);
        let status = with_buffer_api(&mut buffers, |api| unsafe { kumo_mesa_bo_proof(api) });
        if status != 0 {
            say(b"G7o-c native BOs FAILED\n");
        }
        assert_eq!(status, 0, "C BO proof");
        assert_eq!(buffers.live_count(), 0);
    }
    assert!(!client.is_lost());
    assert_eq!(stats.calls.get(), 19); // one VM, nine creates and nine destroys; no rejected I/O
    assert_eq!(stats.closed.get(), 9);
    assert_eq!(stats.maps.get(), 10);
    assert_eq!(stats.unmaps.get(), 10);
    let mut server = server.borrow_mut();
    assert!(server.hardware.buffers.is_empty());
    assert_eq!(server.session.close().bytes, 0);
    say(b"G7o-c native BOs green: all flags, rounding, lazy NC mmap, remap, destroy, stale and foreign refusal; real VMOs\n");
    kumo_rt::process_exit(0)
}
