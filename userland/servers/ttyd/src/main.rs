#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

use kumo_abi::Handle;
use ttyd::{Transport, TtyServer};

kumo_rt::entry!(ttyd_main);

struct ChannelTransport {
    chan: Handle,
    port: Handle,
}

impl Transport for ChannelTransport {
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        if kumo_rt::port_wait(self.port) == 0 {
            return None;
        }
        let n = kumo_rt::channel_read(self.chan, buf.as_mut_ptr(), buf.len()) as usize;
        if n == 0 {
            None
        } else {
            Some(n)
        }
    }

    fn send(&mut self, frame: &[u8]) {
        let _ = kumo_rt::channel_write(self.chan, frame.as_ptr(), frame.len());
    }
}

#[no_mangle]
extern "C" fn ttyd_main(request_channel: u64) -> ! {
    let chan = Handle(request_channel as u32);
    let port_h = kumo_rt::port_create();
    if port_h == u64::MAX {
        kumo_rt::process_exit(1);
    }
    let port = Handle(port_h as u32);
    if kumo_rt::port_bind(port, chan) != 0 {
        kumo_rt::process_exit(1);
    }

    let mut server = TtyServer::new();
    server.serve(&mut ChannelTransport { chan, port });
    kumo_rt::process_exit(0);
}
