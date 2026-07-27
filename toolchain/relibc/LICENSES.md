<!-- j505 -->
# relibc source and licence inventory

This inventory covers source redistributed by KUMO's I-a0 relibc scaffold.
The immutable revisions are recorded in `SOURCE.lock`; the corresponding
licence texts remain beside each source tree.

## Recursive upstream sources

| Source | Revision | Licence |
|---|---|---|
| relibc | `5f6afb52692e62dae79154f4dbec2d0e79a07602` | MIT (`vendor/relibc/LICENSE`) |
| dlmalloc-rs | `40c10f6182cb1d2a78b29f85492936dfbeded02e` | MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`) |
| openlibm | `f5ed774593458e116e69731a3ffa59e2a6408d4e` | Mixed MIT, ISC, BSD-2-Clause, public-domain/FDLIBM notices; LGPL-2.1-or-later applies to the upstream test files (`LICENSE.md`) |

openlibm is pinned because it is part of relibc's recursive source identity,
but I-a0 does not compile or redistribute an openlibm binary.

## Vendored Cargo build closure

The 53 directories under `toolchain/relibc/vendor/` are the packages selected
by the KUMO target's normal/build dependency graph. Each directory contains
Cargo's checksum manifest and its upstream licence text.

| Licence expression from package metadata | Packages |
|---|---|
| `MIT OR Apache-2.0` / `Apache-2.0 OR MIT` | argon2-0.5.3, arrayvec-0.7.8, autocfg-1.5.1, base64ct-1.8.3, bcrypt-pbkdf-0.10.0, bitflags-2.13.0, blake2-0.10.6, block-buffer-0.10.4, blowfish-0.9.1, cc-1.1.22, cfg-if-1.0.4, chrono-0.4.45, chrono-tz-0.10.4, cipher-0.4.4, cpufeatures-0.2.17, crypto-common-0.1.7, digest-0.10.7, hmac-0.12.1, inout-0.1.4, lock_api-0.4.14, log-0.4.33, md-5-0.10.6, num-traits-0.2.19, object-0.36.7, password-hash-0.5.0, pbkdf2-0.12.2, rand-0.10.2, rand_core-0.10.1, rand_core-0.6.4, rand_jitter-0.6.1, rand_xorshift-0.5.0, salsa20-0.10.2, sc-0.2.7, scopeguard-1.2.0, scrypt-0.11.0, sha-crypt-0.5.0, sha2-0.10.9, shlex-1.3.0, typenum-1.20.1, unicode-width-0.1.14 |
| `MIT/Apache-2.0` (upstream legacy spelling) | plain-0.2.3, siphasher-1.0.3, version_check-0.9.5 |
| `MIT` | cbitset-0.2.0, generic-array-0.14.7, libm-0.2.16, phf-0.12.1, phf_shared-0.12.1, posix-regex-0.1.4, spin-0.9.9 |
| `Unlicense OR MIT` | byteorder-1.5.0, memchr-2.8.3 |
| `BSD-3-Clause` | subtle-2.6.1 |

`rust-src` is not copied into KUMO. The check uses the source and dependency
vendor shipped by the exact rustup toolchain named in `rust-toolchain.toml`.
Those files remain covered by the Rust toolchain distribution's own notices.
