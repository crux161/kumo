#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]
//j563

//! The resident `identityd`: the serve loop in this crate's library over a channel.
//!
//! Bootstrap mirrors `fatfs`'s. One channel carries a first message of `[bundle_len: u64 LE]`
//! with a VMO holding the identity bundle (`kumo_identity::protocol::bundle_files`), and then
//! carries requests. The service owns no device and no other authority: it can read the three
//! files it was handed and answer the peer that holds the other end.

use identityd::{blocks_to_map, self_test, serve, Transport};
use kumo_abi::sys::log_level;
use kumo_abi::{Handle, VmarFlags};
use kumo_identity::protocol::{bundle_files, BUNDLE_MAX};
use kumo_identity::{lines, Account, Block, Identity};
use kumo_rt::{channel_read, channel_read_with_handle, channel_write, vmar_map_anywhere};

kumo_rt::entry!(main);

fn log(level: u8, message: &[u8]) {
    let _ = kumo_rt::log_write(level, message);
}

fn fail(message: &[u8]) -> ! {
    log(log_level::ERR, message);
    kumo_rt::process_exit(1)
}

struct ChannelTransport {
    channel: Handle,
    port: Handle,
}

impl Transport for ChannelTransport {
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        if kumo_rt::port_wait(self.port) == 0 {
            return None;
        }
        let n = channel_read(self.channel, buf.as_mut_ptr(), buf.len());
        if n == 0 || n == u64::MAX {
            None
        } else {
            Some(n as usize)
        }
    }

    fn send(&mut self, frame: &[u8]) {
        let _ = channel_write(self.channel, frame.as_ptr(), frame.len());
    }
}

/// Map `len` bytes of `vmo` wherever the kernel places them.
fn map(vmo: Handle, len: usize, flags: VmarFlags) -> Option<*mut u8> {
    let len = (len as u64).checked_add(0xFFF)? & !0xFFF;
    let (status, addr) = vmar_map_anywhere(vmo, len, flags.0);
    (status == 0 && addr != 0).then_some(addr as *mut u8)
}

#[no_mangle]
extern "C" fn main(
    _arg0: u64,
    bootstrap_channel: u64,
    _arg2: u64,
    _arg3: u64,
    _arg4: u64,
    _arg5: u64,
    _arg6: u64,
    _arg7: u64,
) -> ! {
    let channel = Handle(bootstrap_channel as u32);
    let mut header = [0u8; 8];
    let (n, bundle_vmo) = channel_read_with_handle(channel, header.as_mut_ptr(), header.len());
    if bundle_vmo == 0 || n != header.len() {
        fail(b"identityd: bootstrap missing the identity bundle\n");
    }
    let len = u64::from_le_bytes(header) as usize;
    if len == 0 || len > BUNDLE_MAX {
        fail(b"identityd: identity bundle out of range\n");
    }
    let Some(base) = map(Handle(bundle_vmo as u32), len, VmarFlags::READ) else {
        fail(b"identityd: identity bundle map failed\n");
    };
    // SAFETY: the kernel mapped at least `len` readable bytes at `base`, and nothing unmaps them
    // for the life of this process.
    let bundle = unsafe { core::slice::from_raw_parts(base as *const u8, len) };
    let Some([passwd, group, shadow]) = bundle_files(bundle) else {
        fail(b"identityd: identity bundle malformed\n");
    };
    let identity = Identity {
        passwd,
        group,
        shadow,
    };

    let blocks = blocks_to_map(&identity);
    let bytes = blocks * Block::SIZE;
    let memory_vmo = kumo_rt::vmo_create(bytes as u64);
    if memory_vmo == 0 || memory_vmo == u64::MAX {
        fail(b"identityd: argon2 memory unavailable\n");
    }
    let Some(memory) = map(
        Handle(memory_vmo as u32),
        bytes,
        VmarFlags::READ | VmarFlags::WRITE,
    ) else {
        fail(b"identityd: argon2 memory map failed\n");
    };
    // SAFETY: `bytes` of fresh zeroed, writable memory at a page-aligned `memory`, which meets
    // `Block`'s 64-byte alignment; this process holds the only mapping.
    let memory = unsafe { core::slice::from_raw_parts_mut(memory.cast::<Block>(), blocks) };
    if !self_test(memory) {
        fail(b"identityd: argon2id self-test failed\n");
    }

    let port = kumo_rt::port_create();
    if port == u64::MAX || kumo_rt::port_bind(Handle(port as u32), channel) != 0 {
        fail(b"identityd: port unavailable\n");
    }
    let accounts = lines(passwd).filter_map(Account::parse).count();
    let mut line = [0u8; 64];
    let mut at = 0;
    for piece in [
        &b"identityd: online, "[..],
        decimal(accounts as u64, &mut [0u8; 20]),
        b" accounts\n",
    ] {
        let end = (at + piece.len()).min(line.len());
        line[at..end].copy_from_slice(&piece[..end - at]);
        at = end;
    }
    log(log_level::NOTICE, &line[..at]);

    serve(
        &identity,
        memory,
        &mut ChannelTransport {
            channel,
            port: Handle(port as u32),
        },
    );
    // Sora closed its end: there is no one left to answer.
    kumo_rt::process_exit(0)
}

fn decimal(mut value: u64, buf: &mut [u8; 20]) -> &[u8] {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    &buf[at..]
}
