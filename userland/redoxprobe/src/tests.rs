//j928
//j929
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

// --- SH8-c (j929): the crash phase against the real handler, killed at sampled points ---

use kumo_vfs::reconnect::{LinkError, Reconnecting, SessionLink};
use redoxfsd::BulkWindow;
use std::cell::RefCell;

type Server = RedoxServer<CachedDisk<BlockDisk<Ram>>>;

struct World {
    server: Option<Server>,
    generation: u32,
    calls: u32,
    /// Kill the server before this call reaches it.
    kill_before: Option<u32>,
    /// Kill the server after this call committed, losing its reply.
    kill_after: Option<u32>,
    vmos: Vec<Vec<u8>>,
    kills: u32,
}

impl World {
    fn new() -> Self {
        let mut server = serve(Ram(vec![0; (SECTORS * 512) as usize]), true);
        server.set_access(0, SESSION_READ_WRITE);
        Self {
            server: Some(server),
            generation: 1,
            calls: 0,
            kill_before: None,
            kill_after: None,
            vmos: Vec::new(),
            kills: 0,
        }
    }

    /// The server dies; only the volume's bytes (the RAM drv-blk serves) survive, and a new
    /// instance mounts them (with cleanup, as redoxfsd does).
    fn kill(&mut self) {
        let ram = self
            .server
            .take()
            .unwrap()
            .fs
            .disk
            .into_inner()
            .into_transport();
        self.server = Some(serve(ram, false));
        self.generation += 1;
        self.kills += 1;
    }
}

struct VmoWindow<'a>(&'a mut Vec<Vec<u8>>);

impl BulkWindow for VmoWindow<'_> {
    fn fill(
        &mut self,
        vmo: u32,
        vmo_offset: u64,
        len: usize,
        fill: &mut dyn FnMut(&mut [u8]) -> usize,
    ) -> Option<usize> {
        let buf = self.0.get_mut((vmo as usize).checked_sub(1)?)?;
        let start = vmo_offset as usize;
        Some(fill(buf.get_mut(start..start + len)?))
    }
}

struct Link<'w> {
    world: &'w RefCell<World>,
    generation: u32,
}

impl SessionLink for Link<'_> {
    fn exchange(&mut self, request: &[u8], vmo: u32, reply: &mut [u8]) -> Result<usize, LinkError> {
        let mut w = self.world.borrow_mut();
        w.calls += 1;
        if w.kill_before == Some(w.calls) {
            w.kill();
        }
        if self.generation != w.generation {
            return Err(LinkError::Closed);
        }
        let World { server, vmos, .. } = &mut *w;
        let len = server
            .as_mut()
            .unwrap()
            .dispatch(0, request, vmo, &mut VmoWindow(vmos), reply);
        if w.kill_after == Some(w.calls) {
            w.kill();
            return Err(LinkError::Closed);
        }
        Ok(len)
    }

    fn reconnect(&mut self) -> bool {
        let mut w = self.world.borrow_mut();
        self.generation = w.generation;
        w.server.as_mut().unwrap().set_access(0, SESSION_READ_WRITE);
        true
    }
}

struct Host<'w>(&'w RefCell<World>);

impl CrashHost for Host<'_> {
    fn pause(&mut self) {}

    fn vmo_filled(&mut self, len: usize, fill: &dyn Fn(&mut [u8], u64)) -> u32 {
        let mut data = vec![0; len];
        for (i, piece) in data.chunks_mut(256).enumerate() {
            fill(piece, (i * 256) as u64);
        }
        let mut w = self.0.borrow_mut();
        w.vmos.push(data);
        w.vmos.len() as u32
    }

    fn release(&mut self, _vmo: u32) {}
}

fn run(world: &RefCell<World>) -> Result<u32, Fail> {
    let mut rc = Reconnecting::<Link<'_>, 8>::new(Link {
        world,
        generation: 1,
    });
    crash_phase(&mut rc, &mut Host(world))
}

/// A fresh client on a fresh server instance finds exactly the crash phase's file.
fn verify_after_restart(world: &RefCell<World>) -> Result<(), Fail> {
    world.borrow_mut().kill();
    let generation = world.borrow().generation;
    let mut rc = Reconnecting::<Link<'_>, 8>::new(Link { world, generation });
    rc.link.reconnect();
    verify_crash_phase(&mut rc)
}

#[test]
fn without_a_crash_the_phase_says_so() {
    let world = RefCell::new(World::new());
    assert_eq!(run(&world), Err("no crash seen"));
}

#[test]
fn the_server_killed_before_or_after_any_sampled_request_loses_nothing() {
    let probe = RefCell::new(World::new());
    assert_eq!(run(&probe), Err("no crash seen"));
    let total = probe.borrow().calls;
    let mut points: Vec<u32> = (1..total).step_by((total / 24).max(1) as usize).collect();
    // The edges: the first calls, the WriteVmo neighbourhood, the last ones.
    points.extend([1, 2, 3, 258, 259, 260, 261, 262, 263, total - 2, total - 1]);
    for at in points {
        for after in [false, true] {
            let world = RefCell::new(World::new());
            if after {
                world.borrow_mut().kill_after = Some(at);
            } else {
                world.borrow_mut().kill_before = Some(at);
            }
            let result = run(&world);
            assert!(
                matches!(result, Ok(n) if n >= 1),
                "kill {} call {at}: {result:?}",
                if after { "after" } else { "before" }
            );
            assert_eq!(world.borrow().kills, 1);
            assert_eq!(
                verify_after_restart(&world),
                Ok(()),
                "verify after call {at}"
            );
        }
    }
}
