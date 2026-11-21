//j930
//! SH8-d (j930, PLAN/008 S5): RedoxFS's mount and replay path against hostile disks.
//!
//! Random corruption alone is the easy case. Every RedoxFS block pointer carries the target's
//! seahash, and the header hashes itself, so a flipped byte reads back as a clean `EIO`. A hostile
//! disk is a *forgery*. seahash is not secret, so a forger rewrites a block, fixes the hash in the
//! pointer that names it, and re-hashes the header. Merkle hashing rules out cycles. It does not
//! constrain the counts, indexes, levels and ids *inside* a correctly hashed block, and those are
//! what the mount, `cleanup`, walk and commit paths trust.
//!
//! Each case forges one structure on a valid image, then (on a thread with a deadline, catching
//! panics) mounts with cleanup, walks the whole tree, reads every file within a byte budget, commits
//! one write and mounts again. An error anywhere is a pass; a panic, a hang or a runaway
//! allocation is a bug. Seeded and reproducible: `KUMO_FUZZ_SEED` and `KUMO_FUZZ_ITERS` override.
//! — TEAL

use super::*;
use redoxfs::{FileSystem, Node, Transaction, TreePtr, BLOCK_SIZE};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::string::String;
use std::sync::mpsc;
use std::time::Duration;
use std::vec;
use std::vec::Vec;

const SECTORS: u64 = 8 * 1024; // 4 MiB
const BLOCK: usize = BLOCK_SIZE as usize;

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

type Fs = FileSystem<BlockDisk<Ram>>;

fn mount(image: Vec<u8>) -> syscall::error::Result<Fs> {
    FileSystem::open(BlockDisk::new(Ram(image), SECTORS)?, None, Some(0), true)
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }
}

fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed | 1);
    (0..n).map(|_| rng.next() as u8).collect()
}

/// Valid images to forge from: empty, a populated tree with history, and one whose release
/// list holds a node that was open when power failed.
fn seeds() -> Vec<Vec<u8>> {
    let make = || {
        FileSystem::create_unencrypted(
            BlockDisk::new(Ram(vec![0; (SECTORS * 512) as usize]), SECTORS).unwrap(),
            *b"KUMO-SH8-d-fuzz!",
            1,
            0,
        )
        .unwrap()
    };
    let empty = make().disk.into_transport().0;

    let mut fs = make();
    fs.tx(|tx| {
        let a = tx.create_node(TreePtr::root(), "a", Node::MODE_DIR | 0o755, 1, 0)?;
        let b = tx.create_node(a.ptr(), "b", Node::MODE_DIR | 0o755, 1, 0)?;
        for (i, len) in [16usize, 5000, 300_000].iter().enumerate() {
            let f = tx.create_node(
                b.ptr(),
                &std::format!("f{i}"),
                Node::MODE_FILE | 0o644,
                1,
                0,
            )?;
            tx.write_node(f.ptr(), 0, &noise(*len, i as u64 + 7), 1, 0)?;
        }
        for i in 0..60 {
            tx.create_node(
                a.ptr(),
                &std::format!("entry-{i:03}"),
                Node::MODE_FILE | 0o644,
                1,
                0,
            )?;
        }
        Ok(())
    })
    .unwrap();
    for generation in 0..12u64 {
        fs.tx(|tx| {
            let a = tx.find_node(TreePtr::root(), "a")?;
            let b = tx.find_node(a.ptr(), "b")?;
            let f = tx.find_node(b.ptr(), "f1")?;
            tx.write_node(f.ptr(), generation * 300, &noise(700, generation), 2, 0)
        })
        .unwrap();
    }
    fs.tx(|tx| {
        let a = tx.find_node(TreePtr::root(), "a")?;
        tx.remove_node(a.ptr(), "entry-007", Node::MODE_FILE)
            .map(|_| ())
    })
    .unwrap();
    let populated = fs.disk.into_transport().0;

    let mut fs = make();
    let held = fs
        .tx(|tx| {
            let f = tx.create_node(TreePtr::root(), "held", Node::MODE_FILE | 0o644, 1, 0)?;
            tx.write_node(f.ptr(), 0, &noise(9000, 3), 1, 0)?;
            Ok(f.ptr())
        })
        .unwrap();
    fs.tx(|tx| tx.on_open_node(held)).unwrap();
    fs.tx(|tx| {
        tx.remove_node(TreePtr::root(), "held", Node::MODE_FILE)
            .map(|_| ())
    })
    .unwrap();
    let released = fs.disk.into_transport().0; // power fails with the node released-but-open
    vec![empty, populated, released]
}

// --- forging ---

const H_SIZE: usize = 32;
const H_GENERATION: usize = 40;
const H_TREE: usize = 48;
const H_ALLOC: usize = 64;
const H_RELEASE: usize = 80 + 64 * 48;
const H_HASHED: usize = BLOCK - 24;
const H_ENC_HASH: usize = BLOCK - 24;
const H_HASH: usize = BLOCK - 8;

fn u64_at(image: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(image[at..at + 8].try_into().unwrap())
}

fn put_u64(image: &mut [u8], at: usize, value: u64) {
    image[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// The newest valid header's ring block.
fn head(image: &[u8]) -> usize {
    (0..redoxfs::HEADER_RING as usize)
        .filter(|&b| &image[b * BLOCK..b * BLOCK + 8] == redoxfs::SIGNATURE)
        .max_by_key(|&b| u64_at(image, b * BLOCK + H_GENERATION))
        .unwrap_or(0)
}

fn rehash_header(image: &mut [u8], block: usize) {
    let at = block * BLOCK;
    let hash = seahash::hash(&image[at..at + H_HASHED]);
    image[at + H_ENC_HASH..at + H_ENC_HASH + 16].fill(0);
    image[at + H_ENC_HASH..at + H_ENC_HASH + 8].copy_from_slice(&hash.to_le_bytes());
    put_u64(image, at + H_HASH, hash);
}

/// The level-0 block a pointer at `ptr_at` names, if it is one inside the image.
fn target(image: &[u8], ptr_at: usize) -> Option<usize> {
    let addr = u64_at(image, ptr_at);
    let index = (addr >> 8) as usize;
    (addr & 0xF == 0 && index != 0 && (index + 1) * BLOCK <= image.len()).then_some(index)
}

/// Rewrite block `index` with `edit`, then fix the hash in the pointer at `ptr_at`.
fn forge(image: &mut [u8], index: usize, ptr_at: usize, edit: impl FnOnce(&mut [u8])) {
    edit(&mut image[index * BLOCK..(index + 1) * BLOCK]);
    let hash = seahash::hash(&image[index * BLOCK..(index + 1) * BLOCK]);
    put_u64(image, ptr_at + 8, hash);
}

const EXTREME_I64: [i64; 8] = [
    i64::MIN,
    i64::MAX,
    -1_000_000_000,
    1_000_000_000,
    -1,
    1,
    0,
    -4096,
];
const EXTREME_U64: [u64; 7] = [0, 1, u64::MAX, u64::MAX >> 8, 1 << 55, 1 << 56, 0x7FFF];

fn mutate(image: &mut Vec<u8>, rng: &mut Rng) -> String {
    let h = head(image);
    let hb = h * BLOCK;
    match rng.below(7) {
        0 => {
            let (field, name) = rng.pick(&[(H_SIZE, "size"), (H_GENERATION, "generation")]);
            let value = rng.pick(&EXTREME_U64);
            put_u64(image, hb + field, value);
            rehash_header(image, h);
            std::format!("header {name}={value:#x}")
        }
        1 => {
            // A header pointer aimed at another block, with that block's correct hash, so the
            // read succeeds and the bytes are the wrong type (or the wrong level).
            let (field, name) =
                rng.pick(&[(H_TREE, "tree"), (H_ALLOC, "alloc"), (H_RELEASE, "release")]);
            let blocks = image.len() / BLOCK;
            let index = 1 + rng.below(blocks as u64 - 1) as usize;
            let level = rng.pick(&[0u64, 0, 0, 3, 5, 15]);
            put_u64(image, hb + field, (index as u64) << 8 | level);
            let len = (BLOCK << level).min(image.len() - index * BLOCK);
            let hash = seahash::hash(&image[index * BLOCK..index * BLOCK + len]);
            put_u64(image, hb + field + 8, hash);
            rehash_header(image, h);
            std::format!("header {name} -> block {index} level {level}")
        }
        2 => {
            let Some(alloc) = target(image, hb + H_ALLOC) else {
                return "no alloc block".into();
            };
            let entries = (BLOCK - 16) / 16;
            let k = rng.below(entries as u64) as usize;
            let index = rng.pick(&EXTREME_U64);
            let count = rng.pick(&EXTREME_I64);
            forge(image, alloc, hb + H_ALLOC, |b| {
                put_u64(b, 16 + k * 16, index);
                b[24 + k * 16..32 + k * 16].copy_from_slice(&count.to_le_bytes());
            });
            rehash_header(image, h);
            std::format!("alloc entry {k} = ({index:#x}, {count})")
        }
        3 => {
            let Some(alloc) = target(image, hb + H_ALLOC) else {
                return "no alloc block".into();
            };
            // Duplicate the first entry over others: the same blocks allocated twice.
            forge(image, alloc, hb + H_ALLOC, |b| {
                let first: [u8; 16] = b[16..32].try_into().unwrap();
                for k in 1..4 {
                    b[16 + k * 16..32 + k * 16].copy_from_slice(&first);
                }
            });
            rehash_header(image, h);
            "alloc entries duplicated".into()
        }
        4 => {
            let Some(release) = target(image, hb + H_RELEASE) else {
                // No release list: make one out of the alloc block's bytes.
                let at = hb + H_RELEASE;
                image.copy_within(hb + H_ALLOC..hb + H_ALLOC + 16, at);
                rehash_header(image, h);
                return "release list := alloc block".into();
            };
            let k = rng.below(((BLOCK - 16) / 4) as u64) as usize;
            let id = rng.pick(&[0u32, 1, 2, u32::MAX, 0x00FF_FFFF, 77]);
            forge(image, release, hb + H_RELEASE, |b| {
                b[16 + k * 4..20 + k * 4].copy_from_slice(&id.to_le_bytes());
            });
            rehash_header(image, h);
            std::format!("release entry {k} = {id:#x}")
        }
        5 => {
            let Some(tree) = target(image, hb + H_TREE) else {
                return "no tree block".into();
            };
            let entries = BLOCK / 16;
            let k = rng.below(entries as u64) as usize;
            let addr = rng.pick(&EXTREME_U64);
            forge(image, tree, hb + H_TREE, |b| put_u64(b, k * 16, addr));
            rehash_header(image, h);
            std::format!("tree entry {k} addr {addr:#x}")
        }
        _ => {
            let at = rng.below(image.len() as u64) as usize;
            image[at] ^= 1 << rng.below(8);
            std::format!("bit flip at {at:#x}")
        }
    }
}

// --- exercising ---

/// Walk every reachable node (bounded), read files within a byte budget.
fn walk(tx: &mut Transaction<'_, BlockDisk<Ram>>) -> syscall::error::Result<()> {
    let mut stack = vec![(TreePtr::<Node>::root(), 0usize)];
    let mut nodes = 0;
    let mut budget: u64 = 8 << 20;
    let mut buf = vec![0u8; 64 * 1024];
    while let Some((ptr, depth)) = stack.pop() {
        nodes += 1;
        if nodes > 4000 || depth > 32 {
            break;
        }
        let node = tx.read_tree(ptr)?;
        if node.data().is_dir() {
            let mut children = Vec::new();
            tx.child_nodes(ptr, &mut children)?;
            for child in children.iter().take(512) {
                stack.push((child.node_ptr(), depth + 1));
            }
        } else {
            let mut offset = 0;
            while budget > 0 && offset < node.data().size() {
                let n = tx.read_node(ptr, offset, &mut buf, 0, 0)?;
                if n == 0 {
                    break;
                }
                offset += n as u64;
                budget = budget.saturating_sub(n as u64);
            }
        }
    }
    Ok(())
}

/// How far a case got: 0 refused at mount, 1 mounted, 2 walked, 3 committed and remounted.
fn exercise(image: Vec<u8>, depth: &std::sync::atomic::AtomicU8) -> syscall::error::Result<()> {
    use std::sync::atomic::Ordering::Relaxed;
    let mut fs = mount(image)?;
    depth.store(1, Relaxed);
    fs.tx(|tx| walk(tx))?;
    depth.store(2, Relaxed);
    fs.tx(|tx| {
        let f = tx.create_node(
            TreePtr::root(),
            "fuzz-written",
            Node::MODE_FILE | 0o644,
            3,
            0,
        )?;
        tx.write_node(f.ptr(), 0, &noise(20_000, 99), 3, 0)
            .map(|_| ())
    })?;
    let mut fs = mount(fs.disk.into_transport().0)?;
    fs.tx(|tx| walk(tx))?;
    depth.store(3, Relaxed);
    Ok(())
}

/// Run one case: `Ok(depth reached)` for any error or success, `Err(why)` for a panic or a hang.
fn case(image: Vec<u8>) -> Result<u8, String> {
    let (send, recv) = mpsc::channel();
    std::thread::spawn(move || {
        let depth = std::sync::atomic::AtomicU8::new(0);
        let outcome = catch_unwind(AssertUnwindSafe(|| exercise(image, &depth)));
        let reached = depth.load(std::sync::atomic::Ordering::Relaxed);
        let _ = send.send(outcome.map(|_| reached).map_err(|payload| {
            payload
                .downcast_ref::<&str>()
                .map(|s| String::from(*s))
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic".into())
        }));
    });
    match recv.recv_timeout(Duration::from_secs(20)) {
        Ok(outcome) => outcome,
        Err(_) => Err("hang (no result in 20 s)".into()),
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
fn forged_and_corrupted_disks_never_panic_hang_or_run_away() {
    let seed = env_u64("KUMO_FUZZ_SEED", 0x5348_382D_6430_3930);
    let iters = env_u64("KUMO_FUZZ_ITERS", 240);
    let images = seeds();
    for image in &images {
        assert_eq!(
            case(image.clone()),
            Ok(3),
            "a valid seed image must pass every stage"
        );
    }
    let quiet = std::panic::take_hook();
    std::panic::set_hook(std::boxed::Box::new(|_| {}));
    let mut failures = Vec::new();
    let mut reached = [0u64; 4];
    let mut rng = Rng(seed);
    for i in 0..iters {
        let mut image = images[(i % images.len() as u64) as usize].clone();
        let mut what = Vec::new();
        for _ in 0..1 + rng.below(2) {
            what.push(mutate(&mut image, &mut rng));
        }
        match case(image) {
            Ok(depth) => reached[depth as usize] += 1,
            Err(why) => failures.push(std::format!("case {i} [{}]: {why}", what.join("; "))),
        }
    }
    std::panic::set_hook(quiet);
    std::println!(
        "fuzz seed {seed:#x}: {iters} cases; refused at mount {}, mounted {}, walked {}, committed+remounted {}",
        reached[0], reached[1], reached[2], reached[3]
    );
    // The forgeries must exercise the deep paths too, not only the mount's refusals.
    assert!(
        reached[2] + reached[3] >= iters / 5,
        "too few cases got past the walk: {reached:?}"
    );
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The bug classes the fuzzer found (j930), each pinned to a hand-made forgery.
#[test]
fn each_found_forgery_is_refused_cleanly() {
    let images = seeds();
    let populated = &images[1];
    let h = head(populated) * BLOCK;
    let alloc = target(populated, h + H_ALLOC).expect("populated image has an alloc block");
    let forged_alloc = |entry: usize, index: u64, count: i64| {
        let mut image = populated.clone();
        forge(&mut image, alloc, h + H_ALLOC, |b| {
            put_u64(b, 16 + entry * 16, index);
            b[24 + entry * 16..32 + entry * 16].copy_from_slice(&count.to_le_bytes());
        });
        rehash_header(&mut image, h / BLOCK);
        image
    };
    let refused = |image: Vec<u8>, what: &str| {
        assert_eq!(
            case(image.clone()),
            Ok(0),
            "{what}: refused at mount, cleanly"
        );
        assert!(mount(image).is_err(), "{what}");
    };
    // reset_allocator: a 2^63-step loop, a negation overflow, a block index past 2^56.
    refused(
        forged_alloc(3, 0, 1_000_000_000),
        "a free run of 10^9 blocks",
    );
    refused(forged_alloc(3, 1, i64::MIN), "count i64::MIN");
    refused(
        forged_alloc(3, 1 << 56, -1),
        "an index BlockAddr cannot hold",
    );
    // reset_allocator: the same blocks allocated twice (was an assert).
    let mut image = populated.clone();
    forge(&mut image, alloc, h + H_ALLOC, |b| {
        let first: [u8; 16] = b[16..32].try_into().unwrap();
        b[32..48].copy_from_slice(&first);
    });
    rehash_header(&mut image, h / BLOCK);
    refused(image, "a duplicated allocation");
    // A generation the next commit would overflow.
    let mut image = populated.clone();
    put_u64(&mut image, h + H_GENERATION, u64::MAX);
    rehash_header(&mut image, h / BLOCK);
    // The forged header is skipped; the mount falls back to the last good generation, which
    // commits normally (it was an overflow panic at the next commit).
    assert_eq!(
        case(image),
        Ok(3),
        "generation u64::MAX is ignored, not trusted"
    );
    // A volume larger than the disk.
    let mut image = populated.clone();
    put_u64(&mut image, h + H_SIZE, u64::MAX & !(BLOCK as u64 - 1));
    rehash_header(&mut image, h / BLOCK);
    refused(image, "a volume past the disk");
    // A release-list id whose tree index is past a TreeList's 254 entries (was a panic).
    let released = &images[2];
    let rh = head(released) * BLOCK;
    let release = target(released, rh + H_RELEASE).expect("the release image has a list");
    let mut image = released.clone();
    forge(&mut image, release, rh + H_RELEASE, |b| {
        b[16..20].copy_from_slice(&0x00FF_FFFFu32.to_le_bytes());
    });
    rehash_header(&mut image, rh / BLOCK);
    assert!(
        case(image).is_ok(),
        "an out-of-range node id is an error, not a panic"
    );
}
