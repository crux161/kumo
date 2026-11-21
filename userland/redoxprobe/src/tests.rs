//j928
//! The probe against redoxfsd's real handler, across a server replacement.
use super::*;
use redoxfsd::kumo_redoxfs::redoxfs::FileSystem;
use redoxfsd::kumo_redoxfs::{BlockDisk, BlockTransport};
use redoxfsd::{CachedDisk, NoWindow, RedoxServer, SESSION_READ_WRITE};
use std::{vec, vec::Vec};

const SECTORS: u64 = 16 * 1024;

struct Ram(Vec<u8>);

impl BlockTransport for Ram {
    fn exchange(&mut self, request: &[u8], reply: &mut [u8]) -> Option<usize> {
        Some(drv_blk::serve_device_frame(
            &mut self.0,
            request,
            false,
            reply,
        ))
    }
}

struct Local<'s>(&'s mut RedoxServer<CachedDisk<BlockDisk<Ram>>>);

impl Vfs for Local<'_> {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Option<usize> {
        Some(self.0.dispatch(0, request, 0, &mut NoWindow, reply))
    }
}

fn serve(ram: Ram, format: bool) -> RedoxServer<CachedDisk<BlockDisk<Ram>>> {
    let disk = CachedDisk::new(BlockDisk::new(ram, SECTORS).unwrap(), 64);
    let fs = if format {
        FileSystem::create_unencrypted(disk, *b"KUMO-WORK-probe1", 0, 0).unwrap()
    } else {
        FileSystem::open(disk, None, Some(0), true).unwrap()
    };
    let mut server = RedoxServer::new(fs, || (0, 0));
    server.set_access(0, SESSION_READ_WRITE);
    server
}

#[test]
fn the_probe_passes_and_its_tree_outlives_the_server() {
    let mut server = serve(Ram(vec![0; (SECTORS * 512) as usize]), true);
    assert_eq!(write_phase(&mut Local(&mut server)), Ok(()));
    assert_eq!(verify_phase(&mut Local(&mut server)), Ok(()));
    // Replace the server: only the volume's bytes survive.
    server.close_session(0);
    let ram = server.fs.disk.into_inner().into_transport();
    let mut server = serve(ram, false);
    assert_eq!(verify_phase(&mut Local(&mut server)), Ok(()));
    // A second write run cleans the first and passes again.
    assert_eq!(write_phase(&mut Local(&mut server)), Ok(()));
}

#[test]
fn verify_fails_on_a_volume_without_the_tree() {
    let mut server = serve(Ram(vec![0; (SECTORS * 512) as usize]), true);
    assert_eq!(verify_phase(&mut Local(&mut server)), Err("readdir"));
}
