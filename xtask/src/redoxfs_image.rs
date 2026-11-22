//j930
//j931
//j932
//j933
//! `cargo xtask redoxfs-image <path> --size <MiB> [--uuid <text>] [--force] [--persona-cargo] [--sdk-source <path>] [--root] [--sparse]` (SH8-d, D-c/D4;
//! j933: `--sparse` writes the SH8-g extent file instead of a dense image).
//!
//! Writes a fresh RedoxFS disk image with the SAME pinned, vendored RedoxFS and
//! `kumo-redoxfs::BlockDisk` KUMO mounts with, through the drv-blk volume protocol, so the format
//! on the disk is exactly the one redoxfsd reads. Sora never formats a disk it did not create;
//! this tool is how a persistent disk becomes a RedoxFS. Deterministic: the UUID comes from
//! `--uuid` or the path, and the creation time is 0. — TEAL

use kumo_redoxfs::redoxfs::{FileSystem, Node, TreePtr};
use kumo_redoxfs::{BlockDisk, BlockTransport};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// The volume as RAM, served by the same frame logic drv-blk's device mode runs (j933: kept as
/// the reference the sparse volume is tested against).
#[cfg(test)]
struct Ram(Vec<u8>);

#[cfg(test)]
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

/// j933 (SH8-g): the volume as a sparse map of written sectors, so a 64 GiB partition costs
/// only what is written. Reads of unwritten sectors return zeros; the frame checks are
/// drv-blk's own `volume_request`, exactly as `serve_device_frame` applies them. — TEAL
struct Sparse {
    sectors: u64,
    written: BTreeMap<u64, Box<[u8; 512]>>,
}

impl BlockTransport for Sparse {
    fn exchange(&mut self, request: &[u8], reply: &mut [u8]) -> Option<usize> {
        let status = match drv_blk::volume_request(request, 0, self.sectors, false) {
            Ok(drv_blk::VolumeRequest::Read(lba)) => {
                let out = reply.get_mut(..513)?;
                match self.written.get(&lba) {
                    Some(sector) => out[1..].copy_from_slice(&sector[..]),
                    None => out[1..].fill(0),
                }
                out[0] = drv_blk::STATUS_OK;
                return Some(513);
            }
            Ok(drv_blk::VolumeRequest::Write(lba, data)) => {
                let mut sector = Box::new([0u8; 512]);
                sector.copy_from_slice(data);
                self.written.insert(lba, sector);
                drv_blk::STATUS_OK
            }
            Ok(drv_blk::VolumeRequest::Flush) => drv_blk::STATUS_OK,
            Ok(_) => drv_blk::STATUS_UNSUPPORTED,
            Err(status) => status,
        };
        *reply.first_mut()? = status;
        Some(1)
    }
}

impl Sparse {
    fn dense(&self) -> Vec<u8> {
        let mut image = vec![0; (self.sectors * 512) as usize];
        for (lba, sector) in &self.written {
            let at = *lba as usize * 512;
            image[at..at + 512].copy_from_slice(&sector[..]);
        }
        image
    }

    /// The extent file `nvme-prepare-root` writes (SH8-g G-a): every RedoxFS header-ring block
    /// (zeros included, so no stale bytes on the partition can look like a header) and every
    /// other 4 KiB block the builder wrote any sector of, zeros included. The target partition
    /// is NOT zeroed (it holds whatever the SSD held before), so a written zero block that the
    /// file omitted would read back as stale bytes and fail its record hash; the first metal
    /// run lost a file to exactly that (j933).
    ///
    /// Layout, little-endian: `KUMOEXT1`, the RedoxFS UUID (16), the volume's 512-byte sector
    /// count (u64), the record count (u64), then per record its 4 KiB block index from the volume
    /// start (u64) and the block's 4096 bytes; last, the SHA-256 of everything before it.
    fn extents(&self, uuid: [u8; 16]) -> Vec<u8> {
        const SECTORS_PER_BLOCK: u64 = 8;
        let mut blocks: std::collections::BTreeSet<u64> =
            (0..kumo_redoxfs::redoxfs::HEADER_RING).collect();
        for lba in self.written.keys() {
            blocks.insert(lba / SECTORS_PER_BLOCK);
        }
        let mut out = Vec::with_capacity(48 + blocks.len() * 4104 + 32);
        out.extend_from_slice(EXTENT_MAGIC);
        out.extend_from_slice(&uuid);
        out.extend_from_slice(&self.sectors.to_le_bytes());
        out.extend_from_slice(&(blocks.len() as u64).to_le_bytes());
        for block in blocks {
            out.extend_from_slice(&block.to_le_bytes());
            for lba in block * SECTORS_PER_BLOCK..(block + 1) * SECTORS_PER_BLOCK {
                match self.written.get(&lba) {
                    Some(sector) => out.extend_from_slice(&sector[..]),
                    None => out.extend_from_slice(&[0; 512]),
                }
            }
        }
        let digest = kumo_sha256::digest(&out);
        out.extend_from_slice(&digest);
        out
    }
}

/// j933: the extent file's magic (see `Sparse::extents`).
pub const EXTENT_MAGIC: &[u8; 8] = b"KUMOEXT1";

/// j933: replay an extent file onto a dense volume whose every byte starts as `background`
/// (host tests: the real partition is not zeroed).
#[cfg(test)]
pub fn replay_extents(file: &[u8], background: u8) -> Result<([u8; 16], Vec<u8>), String> {
    if file.len() < 48 + 32 || &file[..8] != EXTENT_MAGIC {
        return Err("not a KUMOEXT1 file".into());
    }
    let (body, digest) = file.split_at(file.len() - 32);
    if kumo_sha256::digest(body)[..] != digest[..] {
        return Err("extent digest mismatch".into());
    }
    let uuid: [u8; 16] = body[8..24].try_into().unwrap();
    let sectors = u64::from_le_bytes(body[24..32].try_into().unwrap());
    let count = u64::from_le_bytes(body[32..40].try_into().unwrap()) as usize;
    if body.len() != 40 + count * 4104 {
        return Err("extent length mismatch".into());
    }
    let mut image = vec![background; (sectors * 512) as usize];
    for record in body[40..].chunks_exact(4104) {
        let block = u64::from_le_bytes(record[..8].try_into().unwrap()) as usize;
        let at = block * 4096;
        image
            .get_mut(at..at + 4096)
            .ok_or("extent past the volume")?
            .copy_from_slice(&record[8..]);
    }
    Ok((uuid, image))
}

const MIN_MIB: u64 = 4;
const MAX_MIB: u64 = 64 * 1024;

pub fn run(args: &[String]) -> Result<(), String> {
    const USAGE: &str =
        "usage: cargo xtask redoxfs-image <path> --size <MiB> [--uuid <text>] [--force] [--persona-cargo] [--sdk-source <path>] [--root] [--sparse]";
    let mut path = None;
    let mut size = None;
    let mut uuid_text = None;
    let mut force = false;
    let mut cargo_tree = false;
    let mut sdk_source: Option<PathBuf> = None;
    let mut root_tree = false;
    let mut sparse = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--size" => size = it.next().and_then(|v| v.parse::<u64>().ok()),
            "--uuid" => uuid_text = it.next().cloned(),
            "--force" => force = true,
            "--persona-cargo" => cargo_tree = true,
            "--sdk-source" => sdk_source = it.next().map(PathBuf::from),
            "--root" => root_tree = true,
            "--sparse" => sparse = true,
            other if path.is_none() && !other.starts_with("--") => {
                path = Some(PathBuf::from(other))
            }
            _ => return Err(USAGE.into()),
        }
    }
    let (Some(path), Some(mib)) = (path, size) else {
        return Err(USAGE.into());
    };
    if !(MIN_MIB..=MAX_MIB).contains(&mib) {
        return Err(format!("--size must be {MIN_MIB}..={MAX_MIB} MiB"));
    }
    if !force && fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
        return Err(format!(
            "{} exists and is not empty; pass --force to replace it",
            path.display()
        ));
    }
    let seed = uuid_text.unwrap_or_else(|| path.display().to_string());
    let uuid = uuid_from_seed(&seed);
    if cargo_tree && root_tree {
        return Err("--root and --persona-cargo are separate disks".into());
    }
    let files = if root_tree {
        // j932 (SH8-f): a root disk carries the pinned Linux probes at their FHS paths; the
        // skeleton directories come from ROOT_SKELETON.
        let root = super::workspace_root()?;
        super::pinned_files(&root, &super::PERSONA_PAYLOADS, "persona payloads")?
    } else if cargo_tree {
        let root = super::workspace_root()?;
        let mut files = super::pinned_files(&root, &super::PERSONA_PAYLOADS, "persona payloads")?;
        files.extend(super::pinned_files(
            &root,
            &super::PERSONA_RUSTC,
            "scripts/persona-rustc.sh",
        )?);
        let link_sources: Vec<_> = super::PERSONA_LINK
            .iter()
            .map(|(source, target, hash)| {
                let source = match (
                    sdk_source.as_ref(),
                    source.strip_prefix("build/sysroot/aarch64-unknown-kumo/"),
                ) {
                    (Some(sdk), Some(relative)) => sdk.join(relative),
                    _ => root.join(source),
                };
                (source, *target, *hash)
            })
            .collect();
        let link: Vec<_> = link_sources
            .iter()
            .map(|(path, target, hash)| {
                (path.to_str().expect("SDK source is UTF-8"), *target, *hash)
            })
            .collect();
        files.extend(super::pinned_files(
            &root,
            &link,
            "use --sdk-source pointing at SH7-g's pinned SDK (hashes are checked)",
        )?);
        files.extend(super::persona_cargo_files(&root)?);
        files
    } else {
        Vec::new()
    };
    let dirs: &[&str] = if root_tree { &ROOT_SKELETON } else { &[] };
    let volume = populated_sparse(mib, uuid, dirs, &files)?;
    // j933: an extent file for nvme-prepare-root, or the dense image QEMU attaches.
    let image = if sparse {
        volume.extents(uuid)
    } else {
        volume.dense()
    };
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    fs::write(&path, &image).map_err(|e| format!("write {}: {e}", path.display()))?;
    println!(
        "redoxfs-image: {} ({mib} MiB RedoxFS, uuid {})",
        path.display(),
        uuid.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    if root_tree {
        // The line a boot manifest needs to make this disk the root (DESIGN/010 §7, D2).
        println!(
            "redoxfs-image: root = uuid:{}",
            uuid.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
    }
    Ok(())
}

/// A RedoxFS UUID from its seed text: the first 16 bytes of its SHA-256.
fn uuid_from_seed(seed: &str) -> [u8; 16] {
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&kumo_sha256::digest(seed.as_bytes())[..16]);
    uuid
}

/// j933 (SH8-g): where the root-preparation image carries the extent file (its ESP path).
pub const ROOT_EXTENT_ESP_PATH: &str = "EFI/KUMO/root-nvme.ext";

/// j933 (SH8-g, G-a): the opi5 NVMe root, pinned: the partition's size, the FHS skeleton, the
/// pinned Linux probes, and the UUID from `kumo_gpt::sh8g::ROOT_UUID_SEED`. Returns the
/// extent file and the UUID the boot manifest's `root =` line names.
pub fn nvme_root_extents(root: &std::path::Path) -> Result<(Vec<u8>, [u8; 16]), String> {
    let mib = kumo_gpt::sh8g::ROOT_BLOCKS / 2048;
    let uuid = uuid_from_seed(kumo_gpt::sh8g::ROOT_UUID_SEED);
    let files = super::pinned_files(root, &super::PERSONA_PAYLOADS, "persona payloads")?;
    let volume = populated_sparse(mib, uuid, &ROOT_SKELETON, &files)?;
    Ok((volume.extents(uuid), uuid))
}

/// j932 (SH8-f): the FHS 3.0 merged-`/usr` skeleton of a root disk (DESIGN/012 §2), minus the
/// trees that are synthetic (`dev`, `proc`, `sys`) or the ESP (`boot`). `bin`, `sbin` and `lib`
/// are real directories until RedoxFS links are part of the record. — TEAL
const ROOT_SKELETON: [&str; 13] = [
    "/etc",
    "/home",
    "/root",
    "/tmp",
    "/var/log",
    "/var/run",
    "/var/lib",
    "/usr/bin",
    "/usr/lib",
    "/usr/libexec/kumo",
    "/opt",
    "/media",
    "/mnt",
];

/// A formatted image of `mib` MiB, verified by mounting it again.
#[cfg(test)]
pub fn format(mib: u64, uuid: [u8; 16]) -> Result<Vec<u8>, String> {
    populated(mib, uuid, &[])
}

#[cfg(test)]
fn populated(mib: u64, uuid: [u8; 16], files: &[(&str, Vec<u8>)]) -> Result<Vec<u8>, String> {
    populated_tree(mib, uuid, &[], files)
}

/// `populated`, plus empty directories `dirs` (each created with its parents) before the files.
#[cfg(test)]
fn populated_tree(
    mib: u64,
    uuid: [u8; 16],
    dirs: &[&str],
    files: &[(&str, Vec<u8>)],
) -> Result<Vec<u8>, String> {
    Ok(populated_sparse(mib, uuid, dirs, files)?.dense())
}

/// `populated_tree` on a sparse volume (j933): the same writes, so `dense()` is byte-identical.
fn populated_sparse(
    mib: u64,
    uuid: [u8; 16],
    dirs: &[&str],
    files: &[(&str, Vec<u8>)],
) -> Result<Sparse, String> {
    populated_on::<Sparse>(mib, uuid, dirs, files)
}

/// A blank volume the builder can format (j933): the dense RAM one or the sparse map.
trait Volume: BlockTransport + Sized {
    fn blank(sectors: u64) -> Self;
}

#[cfg(test)]
impl Volume for Ram {
    fn blank(sectors: u64) -> Self {
        Ram(vec![0; (sectors * 512) as usize])
    }
}

impl Volume for Sparse {
    fn blank(sectors: u64) -> Self {
        Sparse {
            sectors,
            written: BTreeMap::new(),
        }
    }
}

fn populated_on<V: Volume>(
    mib: u64,
    uuid: [u8; 16],
    dirs: &[&str],
    files: &[(&str, Vec<u8>)],
) -> Result<V, String> {
    let sectors = mib * 2048;
    let disk = BlockDisk::new(V::blank(sectors), sectors).map_err(|e| format!("volume: {e:?}"))?;
    let mut fs =
        FileSystem::create_unencrypted(disk, uuid, 0, 0).map_err(|e| format!("format: {e:?}"))?;
    let mut directories = BTreeMap::from([(String::new(), TreePtr::root())]);
    let as_dirs: Vec<(&str, Option<&Vec<u8>>)> = dirs.iter().map(|d| (*d, None)).collect();
    let as_files: Vec<(&str, Option<&Vec<u8>>)> =
        files.iter().map(|(p, d)| (*p, Some(d))).collect();
    for (path, data) in as_dirs.into_iter().chain(as_files) {
        let parts: Vec<_> = path.trim_start_matches('/').split('/').collect();
        if !path.starts_with('/')
            || parts
                .iter()
                .any(|p| p.is_empty() || *p == "." || *p == "..")
        {
            return Err(format!("invalid seed path: {path}"));
        }
        let mut parent = TreePtr::root();
        let mut directory = String::new();
        // A directory entry creates every component; a file every component but its name.
        let parents = if data.is_none() {
            parts.len()
        } else {
            parts.len() - 1
        };
        for part in &parts[..parents] {
            directory.push('/');
            directory.push_str(part);
            parent = if let Some(ptr) = directories.get(&directory) {
                *ptr
            } else {
                let node = fs
                    .tx(|tx| tx.create_node(parent, part, Node::MODE_DIR | 0o755, 0, 0))
                    .map_err(|e| format!("mkdir {directory}: {e:?}"))?
                    .ptr();
                directories.insert(directory.clone(), node);
                node
            };
        }
        let Some(data) = data else {
            continue;
        };
        let name = parts[parts.len() - 1];
        let node = fs
            .tx(|tx| tx.create_node(parent, name, Node::MODE_FILE | 0o755, 0, 0))
            .map_err(|e| format!("create {path}: {e:?}"))?
            .ptr();
        for (index, chunk) in data.chunks(1024 * 1024).enumerate() {
            let n = fs
                .tx(|tx| tx.write_node(node, (index * 1024 * 1024) as u64, chunk, 0, 0))
                .map_err(|e| format!("write {path}: {e:?}"))?;
            if n != chunk.len() {
                return Err(format!("short seed write: {path}"));
            }
        }
    }
    let image = fs.disk.into_transport();
    let check = BlockDisk::new(image, sectors).map_err(|e| format!("volume: {e:?}"))?;
    let mut fs =
        FileSystem::open(check, None, Some(0), false).map_err(|e| format!("verify: {e:?}"))?;
    if fs.header.uuid() != uuid {
        return Err("verify: uuid mismatch".into());
    }
    let mut buffer = vec![0; 1024 * 1024];
    for (path, bytes) in files {
        let node = fs
            .tx(|tx| {
                let mut node = tx.read_tree(TreePtr::root())?;
                for part in path.trim_start_matches('/').split('/') {
                    node = tx.find_node(node.ptr(), part)?;
                }
                Ok(node)
            })
            .map_err(|e| format!("lookup after remount {path}: {e:?}"))?;
        if node.data().size() != bytes.len() as u64 {
            return Err(format!("size after remount differs: {path}"));
        }
        for (index, chunk) in bytes.chunks(buffer.len()).enumerate() {
            let n = fs
                .tx(|tx| {
                    tx.read_node(
                        node.ptr(),
                        (index * 1024 * 1024) as u64,
                        &mut buffer[..chunk.len()],
                        0,
                        0,
                    )
                })
                .map_err(|e| format!("verify {path}: {e:?}"))?;
            if n != chunk.len() || &buffer[..n] != chunk {
                return Err(format!("seed differs after remount: {path}"));
            }
        }
    }
    for path in dirs {
        let node = fs
            .tx(|tx| {
                let mut node = tx.read_tree(TreePtr::root())?;
                for part in path.trim_start_matches('/').split('/') {
                    node = tx.find_node(node.ptr(), part)?;
                }
                Ok(node)
            })
            .map_err(|e| format!("lookup after remount {path}: {e:?}"))?;
        if !node.data().is_dir() {
            return Err(format!("not a directory after remount: {path}"));
        }
    }
    if !files.is_empty() {
        println!(
            "redoxfs-image: verified {} files after remount",
            files.len()
        );
    }
    Ok(fs.disk.into_transport())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_root_payloads_replay_on_dirty_media_and_omitted_zeros_break_musl() {
        struct Dirty(Sparse);
        impl BlockTransport for Dirty {
            fn exchange(&mut self, request: &[u8], reply: &mut [u8]) -> Option<usize> {
                if let Ok(drv_blk::VolumeRequest::Read(lba)) =
                    drv_blk::volume_request(request, 0, self.0.sectors, false)
                {
                    if !self.0.written.contains_key(&lba) {
                        reply.get_mut(..513)?.fill(0xA5);
                        reply[0] = drv_blk::STATUS_OK;
                        return Some(513);
                    }
                }
                self.0.exchange(request, reply)
            }
        }
        let root = super::super::workspace_root().unwrap();
        let files =
            super::super::pinned_files(&root, &super::super::PERSONA_PAYLOADS, "persona payloads")
                .unwrap();
        let sectors = kumo_gpt::sh8g::ROOT_BLOCKS;
        let uuid = uuid_from_seed(kumo_gpt::sh8g::ROOT_UUID_SEED);
        let image = populated_sparse(sectors / 2048, uuid, &ROOT_SKELETON, &files)
            .unwrap()
            .extents(uuid);
        for omit_zeros in [false, true] {
            let mut replay = Sparse {
                sectors,
                written: BTreeMap::new(),
            };
            for record in image[40..image.len() - 32].chunks_exact(4104) {
                let block = u64::from_le_bytes(record[..8].try_into().unwrap());
                let data = &record[8..];
                if omit_zeros
                    && block >= kumo_redoxfs::redoxfs::HEADER_RING
                    && !data.iter().any(|b| *b != 0)
                {
                    continue;
                }
                for (i, sector) in data.chunks_exact(512).enumerate() {
                    let mut bytes = Box::new([0; 512]);
                    bytes.copy_from_slice(sector);
                    replay.written.insert(block * 8 + i as u64, bytes);
                }
            }
            let disk = BlockDisk::new(Dirty(replay), sectors).unwrap();
            let mut fs = FileSystem::open(disk, None, Some(0), false).unwrap();
            for (path, expected) in &files {
                let node = fs
                    .tx(|tx| {
                        let mut node = tx.read_tree(TreePtr::root())?;
                        for part in path.trim_start_matches('/').split('/') {
                            node = tx.find_node(node.ptr(), part)?;
                        }
                        Ok(node)
                    })
                    .unwrap();
                let mut out = vec![0; expected.len()];
                let read = fs.tx(|tx| tx.read_node(node.ptr(), 0, &mut out, 0, 0));
                if omit_zeros && *path == "/lib/ld-musl-aarch64.so.1" {
                    assert!(
                        read.is_err(),
                        "old sparse export reproduces the metal loader failure"
                    );
                } else if !omit_zeros {
                    assert_eq!(read.unwrap(), expected.len(), "{path}");
                    assert_eq!(&out, expected, "{path}");
                }
            }
        }
    }

    #[test]
    fn the_sparse_volume_is_byte_identical_to_the_ram_one_and_its_extents_replay() {
        let data: Vec<_> = (0..300_000u32).map(|i| (i * 7 + 3) as u8).collect();
        let files = [("/w/a.bin", data), ("/bin/probe", b"elf".to_vec())];
        let ram: Ram = populated_on(16, *b"KUMO-SH8-g-dense", &ROOT_SKELETON, &files).unwrap();
        let sparse: Sparse =
            populated_on(16, *b"KUMO-SH8-g-dense", &ROOT_SKELETON, &files).unwrap();
        assert!(ram.0 == sparse.dense(), "same writes, same bytes");
        let extents = sparse.extents(*b"KUMO-SH8-g-dense");
        let (uuid, replayed) = replay_extents(&extents, 0).unwrap();
        assert_eq!(&uuid, b"KUMO-SH8-g-dense");
        assert!(replayed == ram.0, "the extent file is the whole volume");
        // The partition on the SSD is not zeroed: over stale bytes the replayed volume must
        // still mount and every file must read back exactly (j933's first metal run did not).
        let (_, over_garbage) = replay_extents(&extents, 0xA5).unwrap();
        let sectors = over_garbage.len() as u64 / 512;
        let disk = BlockDisk::new(Ram(over_garbage), sectors).unwrap();
        let mut fs = FileSystem::open(disk, None, Some(0), false).unwrap();
        for (path, bytes) in &files {
            let node = fs
                .tx(|tx| {
                    let mut node = tx.read_tree(TreePtr::root())?;
                    for part in path.trim_start_matches('/').split('/') {
                        node = tx.find_node(node.ptr(), part)?;
                    }
                    Ok(node)
                })
                .unwrap();
            let mut out = vec![0; bytes.len()];
            let n = fs
                .tx(|tx| tx.read_node(node.ptr(), 0, &mut out, 0, 0))
                .unwrap();
            assert_eq!(n, bytes.len());
            assert!(out == *bytes, "{path} reads back over stale bytes");
        }
        assert!(
            extents.len() < ram.0.len() / 4,
            "a mostly empty volume ships mostly nothing"
        );
        let mut torn = extents.clone();
        torn[100] ^= 1;
        assert!(
            replay_extents(&torn, 0).is_err(),
            "the digest covers every byte"
        );
    }

    #[test]
    fn a_root_disk_carries_the_whole_skeleton_beside_its_files() {
        let image = populated_tree(
            8,
            *b"KUMO-SH8-f-root!",
            &ROOT_SKELETON,
            &[("/bin/probe", b"elf".to_vec())],
        )
        .unwrap();
        assert_eq!(&image[..8], b"RedoxFS\0");
        assert_eq!(
            &image[16..32],
            b"KUMO-SH8-f-root!",
            "Sora reads the UUID here"
        );
        assert!(populated_tree(8, *b"KUMO-SH8-f-root!", &["/a/../b"], &[]).is_err());
    }

    #[test]
    fn nested_seed_files_cross_records_and_transactions_and_remount_by_name() {
        let data: Vec<_> = (0..1024 * 1024 + 177)
            .map(|i| (i * 131 + 7) as u8)
            .collect();
        populated(
            8,
            *b"KUMO-SH8-e-image",
            &[
                ("/w/message/src/lib.rs", data),
                ("/w/src/main.rs", b"fn main() {}".to_vec()),
                ("/tmp/.keep", Vec::new()),
            ],
        )
        .unwrap();
        assert!(populated(8, *b"KUMO-SH8-e-image", &[("/../bad", vec![1])]).is_err());
        let mut state = 1u64;
        let noise: Vec<_> = (0..4 << 20)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        assert!(populated(4, *b"KUMO-SH8-e-image", &[("/large", noise)]).is_err());
    }

    #[test]
    fn an_image_carries_the_signature_sora_detects_and_mounts_again() {
        let image = format(8, *b"KUMO-SH8-d-image").unwrap();
        assert_eq!(image.len(), 8 << 20);
        assert_eq!(&image[..8], b"RedoxFS\0");
        assert_eq!(
            image,
            format(8, *b"KUMO-SH8-d-image").unwrap(),
            "deterministic"
        );
    }
}
