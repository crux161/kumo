//j930
//j931
//j932
//! `cargo xtask redoxfs-image <path> --size <MiB> [--uuid <text>] [--force] [--persona-cargo] [--sdk-source <path>] [--root]` (SH8-d, D-c/D4).
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

/// The volume as RAM, served by the same frame logic drv-blk's device mode runs.
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

const MIN_MIB: u64 = 4;
const MAX_MIB: u64 = 64 * 1024;

pub fn run(args: &[String]) -> Result<(), String> {
    const USAGE: &str =
        "usage: cargo xtask redoxfs-image <path> --size <MiB> [--uuid <text>] [--force] [--persona-cargo] [--sdk-source <path>] [--root]";
    let mut path = None;
    let mut size = None;
    let mut uuid_text = None;
    let mut force = false;
    let mut cargo_tree = false;
    let mut sdk_source: Option<PathBuf> = None;
    let mut root_tree = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--size" => size = it.next().and_then(|v| v.parse::<u64>().ok()),
            "--uuid" => uuid_text = it.next().cloned(),
            "--force" => force = true,
            "--persona-cargo" => cargo_tree = true,
            "--sdk-source" => sdk_source = it.next().map(PathBuf::from),
            "--root" => root_tree = true,
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
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&kumo_sha256::digest(seed.as_bytes())[..16]);
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
    let image = populated_tree(mib, uuid, dirs, &files)?;
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
fn populated_tree(
    mib: u64,
    uuid: [u8; 16],
    dirs: &[&str],
    files: &[(&str, Vec<u8>)],
) -> Result<Vec<u8>, String> {
    let sectors = mib * 2048;
    let disk = BlockDisk::new(Ram(vec![0; (sectors * 512) as usize]), sectors)
        .map_err(|e| format!("volume: {e:?}"))?;
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
    let image = fs.disk.into_transport().0;
    let check = BlockDisk::new(Ram(image), sectors).map_err(|e| format!("volume: {e:?}"))?;
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
    Ok(fs.disk.into_transport().0)
}

#[cfg(test)]
mod tests {
    use super::*;

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
