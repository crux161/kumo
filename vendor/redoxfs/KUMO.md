# RedoxFS on KUMO
<!-- j926 -->

SH8-a / PLAN/008 S5-a. The upstream source is RedoxFS **0.9.1**, tag `0.9.1`,
commit `473b4baeb041ebe14504f30693393b1cae52558c` from
<https://github.com/redox-os/redoxfs> (mirror of
<https://gitlab.redox-os.org/redox-os/redoxfs>). The upstream MIT license is in `LICENSE`.
The on-disk format remains **8** (`src/lib.rs`); mount checks its signature, version,
and header checksum. No format conversion is attempted.

The SHA-256 of `git archive 473b4baeb041ebe14504f30693393b1cae52558c` is
`073f16ce6695737b0d510407d34b5ff35004d72deadf5a7a760e7041ae1b6c62`.
All upstream files are retained. `KUMO.patch` is the complete local delta to the
manifest and source files; apply it at the root of that archive to reproduce this port.
`KUMO.md` and `KUMO.patch` are KUMO additions.

The manifest packages only the `no_std` library. Its original host tools, FUSE and
Redox scheme adapters are retained as reference sources, but are not build targets.
The eight direct dependencies are exact pins from upstream's release lockfile;
the dependency closure is checked in under `vendor/cargo` with Cargo's checksums.
`redox_syscall` supplies errors and flag constants, with its userspace syscall
feature disabled. KUMO does not invoke Redox syscalls. The root workspace's
existing source replacement keeps all checks offline.

Local source changes:

- Import `alloc::vec::Vec` in the two places that relied on the std prelude.
- Add `Header::with_uuid` and `FileSystem::create_unencrypted`: UUID and timestamps
  come from the caller, with no ambient random source or clock. They share the
  existing formatter; encryption remains outside the KUMO gate.
- Add a `Disk::sync` seam. A changed transaction flushes the CoW blocks before
  publishing its new header, then flushes the header before reporting success.
  `lib/kumo-redoxfs` implements both barriers with the existing `drv-blk` flush
  command and propagates every error. The upstream trait's default is a no-op for
  compatibility with its memory adapters; durable KUMO mounts use `BlockDisk`.
- Refuse a checksummed header whose generation would underflow the ring origin.
- Silence the unused decompression error when upstream logging is disabled.

`kumo-redoxfs::BlockDisk` owns a volume transport and the granted 512-byte sector
count. The driver owns partition offsets and access policy. RedoxFS's 4096-byte
blocks map to eight sector requests. All spans are checked before I/O, read and
status replies must have exact lengths, read-only errors remain EROFS, and a failed
or unsupported flush cannot acknowledge a commit. After a write/flush error the
caller must discard the filesystem instance and remount before further mutations.

Run from the repository root:

```sh
cargo test -p redoxfs -p kumo-redoxfs --offline
cargo build -p kumo-redoxfs --target aarch64-unknown-none --release --offline
cargo build -p kumo-redoxfs --target x86_64-unknown-none --release --offline
```

The host proof uses the real upstream transactions over the real block wire codec,
served by a driver model with separate volatile and stable bytes. It proves directory
and multi-record-file remount, deletion, two failed commit barriers, every interrupted
sector write in one transaction, checksum fallback/replay refusal, format refusal,
truncation, overflow, volume bounds, and malformed replies. The 28 upstream core
unit tests run without its std feature, alongside eight KUMO tests.

This is the library/block-adapter gate, with no live filesystem server or root switch.
Before holding real data: bound and fuzz hostile mount/replay inputs, implement and
boot `fsd` through the existing VFS protocol, prove Sora kill/restart recovery on a
persistent backend, then repeat SH7's Cargo workspace from that volume. An ordinary
image boot still uses the existing FAT mechanics. — KESTREL 2026-11-20
