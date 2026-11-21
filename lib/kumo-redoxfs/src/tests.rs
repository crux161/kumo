//j926
use super::*;
use drv_blk::{BlockDevice, CMD_FLUSH, CMD_READ, CMD_WRITE};
use redoxfs::{FileSystem, Node, TreePtr, BLOCK_SIZE, HEADER_RING};
use std::{vec, vec::Vec};
use syscall::error::ENOENT;

const SECTORS: u64 = 16 * 1024;
const UUID: [u8; 16] = *b"KUMO-SH8-a-test1";
type Fs = FileSystem<BlockDisk<Memory>>;

/// A driver with a volatile write cache. Only CMD_FLUSH copies bytes to stable storage;
/// power loss drops the cache and every filesystem/allocator object. — KESTREL
#[derive(Clone)]
struct Memory {
    stable: Vec<u8>,
    cache: Vec<u8>,
    requests: usize,
    writes: usize,
    flushes: usize,
    fail_flush: Option<usize>,
    writes_left: Option<usize>,
}

impl Memory {
    fn new() -> Self {
        Self {
            stable: vec![0; (SECTORS * SECTOR_SIZE) as usize],
            cache: vec![0; (SECTORS * SECTOR_SIZE) as usize],
            requests: 0,
            writes: 0,
            flushes: 0,
            fail_flush: None,
            writes_left: None,
        }
    }

    fn power_loss(mut self) -> BlockDisk<Self> {
        self.cache.clone_from(&self.stable);
        self.fail_flush = None;
        self.writes_left = None;
        BlockDisk::new(self, SECTORS).unwrap()
    }
}

impl BlockTransport for Memory {
    fn exchange(&mut self, request: &[u8], reply: &mut [u8]) -> Option<usize> {
        self.requests += 1;
        let req = Request::decode(request)?;
        let device = BlockDevice::new(SECTORS * SECTOR_SIZE);
        match req.cmd {
            CMD_READ => {
                let data = device.read_blocks(&self.cache, req.lba, req.count).ok()?;
                reply.get_mut(1..1 + data.len())?.copy_from_slice(data);
                reply[0] = STATUS_OK;
                Some(1 + data.len())
            }
            CMD_WRITE => {
                self.writes += 1;
                if let Some(left) = self.writes_left.as_mut() {
                    if *left == 0 {
                        reply[0] = drv_blk::STATUS_IO_ERROR;
                        return Some(1);
                    }
                    *left -= 1;
                }
                reply[0] = device.write_blocks(
                    &mut self.cache,
                    req.lba,
                    req.count,
                    &request[REQUEST_LEN..],
                );
                Some(1)
            }
            CMD_FLUSH => {
                self.flushes += 1;
                if self.fail_flush == Some(self.flushes) {
                    reply[0] = drv_blk::STATUS_IO_ERROR;
                } else {
                    self.stable.clone_from(&self.cache);
                    reply[0] = STATUS_OK;
                }
                Some(1)
            }
            _ => None,
        }
    }
}

fn format() -> Fs {
    let disk = BlockDisk::new(Memory::new(), SECTORS).unwrap();
    FileSystem::create_unencrypted(disk, UUID, 1, 0).unwrap()
}

fn remount(fs: Fs) -> Fs {
    let disk = fs.disk.into_transport().power_loss();
    FileSystem::open(disk, None, Some(0), false).unwrap()
}

fn add_file(fs: &mut Fs, name: &str, data: &[u8]) -> TreePtr<Node> {
    fs.tx(|tx| {
        let node = tx.create_node(TreePtr::root(), name, Node::MODE_FILE | 0o640, 1, 0)?;
        tx.write_node(node.ptr(), 0, data, 1, 0)?;
        Ok(node.ptr())
    })
    .unwrap()
}

fn read(fs: &mut Fs, parent: TreePtr<Node>, name: &str) -> Vec<u8> {
    fs.tx(|tx| {
        let node = tx.find_node(parent, name)?;
        let mut data = vec![0; node.data().size() as usize];
        let len = tx.read_node(node.ptr(), 0, &mut data, 0, 0)?;
        assert_eq!(len, data.len());
        Ok(data)
    })
    .unwrap()
}

#[test]
fn blocks_translate_to_sectors_without_crossing_the_volume() {
    let mut disk = BlockDisk::new(Memory::new(), SECTORS).unwrap();
    let data: Vec<_> = (0..(2 * BLOCK_SIZE)).map(|i| (i % 251) as u8).collect();
    // The last two filesystem blocks, with preceding sectors checked for spill.
    let start = SECTORS / 8 - 2;
    unsafe { disk.write_at(start, &data).unwrap() };
    let mut out = vec![0; data.len()];
    assert_eq!(unsafe { disk.read_at(start, &mut out) }, Ok(data.len()));
    assert_eq!(out, data);
    assert!(disk.transport.cache[..(start * BLOCK_SIZE) as usize]
        .iter()
        .all(|b| *b == 0));
    let requests = disk.transport.requests;
    for (block, len) in [(u64::MAX, 4096), (start + 1, 8192), (0, 513)] {
        assert!(unsafe { disk.write_at(block, &vec![0; len]) }.is_err());
        assert!(unsafe { disk.read_at(block, &mut vec![0; len]) }.is_err());
    }
    assert_eq!(disk.transport.requests, requests);
    assert!(BlockDisk::new(Memory::new(), 7).is_err());
    assert!(BlockDisk::new(Memory::new(), u64::MAX - 7).is_err());
}

#[test]
fn protocol_errors_and_short_replies_are_never_success() {
    struct Reply(Option<usize>, u8);
    impl BlockTransport for Reply {
        fn exchange(&mut self, _: &[u8], out: &mut [u8]) -> Option<usize> {
            out.fill(self.1);
            self.0
        }
    }
    for (len, status, errno) in [
        (None, 0, EIO),
        (Some(0), 0, EIO),
        (Some(2), 0, EIO),
        (Some(1), drv_blk::STATUS_READ_ONLY, EROFS),
        (Some(1), drv_blk::STATUS_UNSUPPORTED, EIO),
        (Some(1), drv_blk::STATUS_BAD_LBA, EIO),
        (Some(1), 0xFF, EIO),
    ] {
        let mut disk = BlockDisk::new(Reply(len, status), 8).unwrap();
        assert_eq!(
            unsafe { disk.write_at(0, &[0; 4096]) },
            Err(Error::new(errno))
        );
        assert_eq!(disk.sync(), Err(Error::new(errno)));
    }
    for len in [None, Some(0), Some(1), Some(512), Some(514)] {
        let mut disk = BlockDisk::new(Reply(len, 0), 8).unwrap();
        assert_eq!(
            unsafe { disk.read_at(0, &mut [0; 4096]) },
            Err(Error::new(EIO))
        );
    }
}

#[test]
fn directories_and_multirecord_file_survive_remount_then_delete() {
    let mut fs = format();
    let dir = fs
        .tx(|tx| tx.create_node(TreePtr::root(), "workspace", Node::MODE_DIR | 0o750, 1, 0))
        .unwrap()
        .ptr();
    let data: Vec<_> = (0..(redoxfs::RECORD_SIZE * 2 + 177))
        .map(|i| ((i * 17 + i / 251) % 256) as u8)
        .collect();
    fs.tx(|tx| {
        let file = tx.create_node(dir, "long-rust-source.rs", Node::MODE_FILE | 0o640, 1, 0)?;
        tx.write_node(file.ptr(), 0, &data, 1, 0)
    })
    .unwrap();
    let mut fs = remount(fs);
    assert_eq!(fs.header.uuid(), UUID);
    let dir = fs
        .tx(|tx| tx.find_node(TreePtr::root(), "workspace"))
        .unwrap()
        .ptr();
    assert_eq!(read(&mut fs, dir, "long-rust-source.rs"), data);
    fs.tx(|tx| tx.remove_node(dir, "long-rust-source.rs", Node::MODE_FILE))
        .unwrap();
    fs.tx(|tx| tx.remove_node(TreePtr::root(), "workspace", Node::MODE_DIR))
        .unwrap();
    let mut fs = remount(fs);
    assert_eq!(
        fs.tx(|tx| tx.find_node(TreePtr::root(), "workspace")).err(),
        Some(Error::new(ENOENT))
    );
}

#[test]
fn failed_flush_keeps_the_preceding_committed_tree() {
    let mut initial = format();
    let ptr = add_file(&mut initial, "committed", b"old durable bytes");
    let baseline = initial.disk.into_transport();
    for barrier in [1, 2] {
        let mut fs = FileSystem::open(baseline.clone().power_loss(), None, Some(0), false).unwrap();
        fs.disk.transport.fail_flush = Some(fs.disk.transport.flushes + barrier);
        assert_eq!(
            fs.tx(|tx| tx.write_node(ptr, 0, b"new durable bytes", 2, 0)),
            Err(Error::new(EIO))
        );
        let mut fs = remount(fs);
        assert_eq!(
            read(&mut fs, TreePtr::root(), "committed"),
            b"old durable bytes"
        );
    }
}

#[test]
fn interrupted_cow_header_write_keeps_the_preceding_tree() {
    let mut fs = format();
    let ptr = add_file(&mut fs, "committed", b"old durable bytes");
    // Count sector writes up to the first barrier, then interrupt each of the eight sectors
    // of the new root. These are device failures, not a graceful filesystem shutdown.
    let baseline = fs.disk.into_transport();
    let mut trial = FileSystem::open(baseline.clone().power_loss(), None, Some(0), false).unwrap();
    trial.disk.transport.fail_flush = Some(trial.disk.transport.flushes + 1);
    let start = trial.disk.transport.writes;
    assert!(trial
        .tx(|tx| tx.write_node(ptr, 0, b"new durable bytes", 2, 0))
        .is_err());
    let data_sectors = trial.disk.transport.writes - start;
    assert!(data_sectors > 0);
    for left in 0..=data_sectors + 8 {
        let mut fs = FileSystem::open(baseline.clone().power_loss(), None, Some(0), false).unwrap();
        fs.disk.transport.writes_left = Some(left);
        let result = fs.tx(|tx| tx.write_node(ptr, 0, b"new durable bytes", 2, 0));
        let committed = result.is_ok();
        let mut fs = remount(fs);
        let expected = if committed {
            b"new durable bytes"
        } else {
            b"old durable bytes"
        };
        let data = read(&mut fs, TreePtr::root(), "committed");
        assert_eq!(data, expected);
    }
}

#[test]
fn a_corrupt_newest_header_falls_back_to_the_last_committed_generation() {
    let mut fs = format();
    let ptr = add_file(&mut fs, "committed", b"old durable bytes");
    fs.tx(|tx| tx.write_node(ptr, 0, b"replacement bytes", 2, 0))
        .unwrap();
    let slot = fs.header.generation() % HEADER_RING;
    let mut memory = fs.disk.into_transport();
    memory.stable[(slot * BLOCK_SIZE) as usize + 32] ^= 0x80;
    let mut fs = FileSystem::open(memory.power_loss(), None, Some(0), false).unwrap();
    assert_eq!(
        read(&mut fs, TreePtr::root(), "committed"),
        b"old durable bytes"
    );
}

#[test]
fn unknown_format_and_truncated_disk_are_refused() {
    let mut fs = format();
    let mut bad = fs.header;
    bad.version = (redoxfs::VERSION + 1).into();
    bad.update(None);
    let mut memory = fs.disk.into_transport();
    // Every slot has a self-consistent checksum but a future format version.
    for chunk in memory.stable[..(HEADER_RING * BLOCK_SIZE) as usize].chunks_exact_mut(4096) {
        chunk.copy_from_slice(&bad);
    }
    assert!(FileSystem::open(memory.power_loss(), None, Some(0), false).is_err());
    fs = format();
    let mut memory = fs.disk.into_transport();
    memory.cache.truncate(4096);
    assert!(FileSystem::open(
        BlockDisk::new(memory, SECTORS).unwrap(),
        None,
        Some(0),
        false
    )
    .is_err());
}

#[test]
fn replay_refuses_corrupt_allocator_and_misplaced_header_generation() {
    let mut fs = format();
    add_file(&mut fs, "committed", b"durable bytes");
    let alloc_offset = fs.header.alloc.addr().index() * BLOCK_SIZE;
    let mut memory = fs.disk.into_transport();
    memory.stable[alloc_offset as usize] ^= 0x80;
    assert_eq!(
        FileSystem::open(memory.power_loss(), None, Some(0), false).err(),
        Some(Error::new(EIO))
    );

    let fs = format();
    let mut header = fs.header;
    header.generation = 6.into();
    header.update(None);
    let mut memory = fs.disk.into_transport();
    memory.stable[..4096].copy_from_slice(&header);
    assert_eq!(
        FileSystem::open(memory.power_loss(), None, Some(0), false).err(),
        Some(Error::new(EIO))
    );
}
