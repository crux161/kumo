//j930
//! `cargo xtask redoxfs-image <path> --size <MiB> [--uuid <text>] [--force]` (SH8-d, D-c/D4).
//!
//! Writes a fresh RedoxFS disk image with the SAME pinned, vendored RedoxFS and
//! `kumo-redoxfs::BlockDisk` KUMO mounts with, through the drv-blk volume protocol, so the format
//! on the disk is exactly the one redoxfsd reads. Sora never formats a disk it did not create;
//! this tool is how a persistent disk becomes a RedoxFS. Deterministic: the UUID comes from
//! `--uuid` or the path, and the creation time is 0. — TEAL

use kumo_redoxfs::redoxfs::FileSystem;
use kumo_redoxfs::{BlockDisk, BlockTransport};
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
        "usage: cargo xtask redoxfs-image <path> --size <MiB> [--uuid <text>] [--force]";
    let mut path = None;
    let mut size = None;
    let mut uuid_text = None;
    let mut force = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--size" => size = it.next().and_then(|v| v.parse::<u64>().ok()),
            "--uuid" => uuid_text = it.next().cloned(),
            "--force" => force = true,
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
    let image = format(mib, uuid)?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    fs::write(&path, &image).map_err(|e| format!("write {}: {e}", path.display()))?;
    println!(
        "redoxfs-image: {} ({mib} MiB RedoxFS, uuid {})",
        path.display(),
        uuid.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    Ok(())
}

/// A formatted image of `mib` MiB, verified by mounting it again.
pub fn format(mib: u64, uuid: [u8; 16]) -> Result<Vec<u8>, String> {
    let sectors = mib * 2048;
    let disk = BlockDisk::new(Ram(vec![0; (sectors * 512) as usize]), sectors)
        .map_err(|e| format!("volume: {e:?}"))?;
    let fs =
        FileSystem::create_unencrypted(disk, uuid, 0, 0).map_err(|e| format!("format: {e:?}"))?;
    let image = fs.disk.into_transport().0;
    let check = BlockDisk::new(Ram(image), sectors).map_err(|e| format!("volume: {e:?}"))?;
    let fs = FileSystem::open(check, None, Some(0), false).map_err(|e| format!("verify: {e:?}"))?;
    if fs.header.uuid() != uuid {
        return Err("verify: uuid mismatch".into());
    }
    Ok(fs.disk.into_transport().0)
}

#[cfg(test)]
mod tests {
    use super::*;

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
