<!-- j511 -->
# KUMO cbindgen host-tool licence inventory

51 crates, vendored under `toolchain/cbindgen/vendor/`. This closure is a
**host build tool** — it produces KUMO's C headers and is never compiled into KUMO or
shipped in an image. Each crate keeps its upstream `.cargo-checksum.json` and any
licence files it publishes; this table is the index, not a substitute for them.

## The two worth reading twice

- **`cbindgen` itself is `MPL-2.0`** — file-level copyleft. It is a build tool executed to
  produce headers; no part of it is linked into KUMO or shipped in an image, so MPL's
  reciprocity attaches to cbindgen's own files, which are vendored here unmodified. If a
  future slice ever *modifies* a cbindgen source file, that file's changes must be offered
  back under MPL-2.0.
- **`r-efi` offers `LGPL-2.1-or-later` among its options** — KUMO takes the `MIT OR
  Apache-2.0` arm, as the disjunction permits. Nothing here relies on the LGPL arm.

Everything else is permissive (MIT/Apache-2.0 style). No crate in this closure has an
undeclared licence.

## Licences in use

- `MIT OR Apache-2.0` — 37 crate(s)
- `Apache-2.0 OR MIT` — 4 crate(s)
- `MIT` — 4 crate(s)
- `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` — 2 crate(s)
- `(MIT OR Apache-2.0) AND Unicode-3.0` — 1 crate(s)
- `MIT OR Apache-2.0 OR LGPL-2.1-or-later` — 1 crate(s)
- `MPL-2.0` — 1 crate(s)
- `Unlicense OR MIT` — 1 crate(s)

## Crates

| crate | version | licence |
|---|---|---|
| `anstream` | 1.0.0 | `MIT OR Apache-2.0` |
| `anstyle` | 1.0.14 | `MIT OR Apache-2.0` |
| `anstyle-parse` | 1.0.0 | `MIT OR Apache-2.0` |
| `anstyle-query` | 1.1.5 | `MIT OR Apache-2.0` |
| `anstyle-wincon` | 3.0.11 | `MIT OR Apache-2.0` |
| `bitflags` | 2.13.1 | `MIT OR Apache-2.0` |
| `cbindgen` | 0.29.2 | `MPL-2.0` |
| `cfg-if` | 1.0.4 | `MIT OR Apache-2.0` |
| `clap` | 4.6.4 | `MIT OR Apache-2.0` |
| `clap_builder` | 4.6.2 | `MIT OR Apache-2.0` |
| `clap_lex` | 1.1.0 | `MIT OR Apache-2.0` |
| `colorchoice` | 1.0.5 | `MIT OR Apache-2.0` |
| `equivalent` | 1.0.2 | `Apache-2.0 OR MIT` |
| `errno` | 0.3.14 | `MIT OR Apache-2.0` |
| `fastrand` | 2.5.0 | `Apache-2.0 OR MIT` |
| `getrandom` | 0.4.3 | `MIT OR Apache-2.0` |
| `hashbrown` | 0.17.1 | `MIT OR Apache-2.0` |
| `heck` | 0.5.0 | `MIT OR Apache-2.0` |
| `indexmap` | 2.14.0 | `Apache-2.0 OR MIT` |
| `is_terminal_polyfill` | 1.70.2 | `MIT OR Apache-2.0` |
| `itoa` | 1.0.18 | `MIT OR Apache-2.0` |
| `libc` | 0.2.189 | `MIT OR Apache-2.0` |
| `linux-raw-sys` | 0.12.1 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` |
| `log` | 0.4.33 | `MIT OR Apache-2.0` |
| `memchr` | 2.8.3 | `Unlicense OR MIT` |
| `once_cell` | 1.21.4 | `MIT OR Apache-2.0` |
| `once_cell_polyfill` | 1.70.2 | `MIT OR Apache-2.0` |
| `proc-macro2` | 1.0.107 | `MIT OR Apache-2.0` |
| `quote` | 1.0.47 | `MIT OR Apache-2.0` |
| `r-efi` | 6.0.0 | `MIT OR Apache-2.0 OR LGPL-2.1-or-later` |
| `rustix` | 1.1.4 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` |
| `serde` | 1.0.229 | `MIT OR Apache-2.0` |
| `serde_core` | 1.0.229 | `MIT OR Apache-2.0` |
| `serde_derive` | 1.0.229 | `MIT OR Apache-2.0` |
| `serde_json` | 1.0.151 | `MIT OR Apache-2.0` |
| `serde_spanned` | 1.1.1 | `MIT OR Apache-2.0` |
| `strsim` | 0.11.1 | `MIT` |
| `syn` | 3.0.3 | `MIT OR Apache-2.0` |
| `syn` | 2.0.119 | `MIT OR Apache-2.0` |
| `tempfile` | 3.27.0 | `MIT OR Apache-2.0` |
| `toml` | 0.9.12+spec-1.1.0 | `MIT OR Apache-2.0` |
| `toml_datetime` | 0.7.5+spec-1.1.0 | `MIT OR Apache-2.0` |
| `toml_parser` | 1.1.3+spec-1.1.0 | `MIT OR Apache-2.0` |
| `toml_writer` | 1.1.2+spec-1.1.0 | `MIT OR Apache-2.0` |
| `unicode-ident` | 1.0.24 | `(MIT OR Apache-2.0) AND Unicode-3.0` |
| `utf8parse` | 0.2.2 | `Apache-2.0 OR MIT` |
| `windows-link` | 0.2.1 | `MIT OR Apache-2.0` |
| `windows-sys` | 0.61.2 | `MIT OR Apache-2.0` |
| `winnow` | 1.0.4 | `MIT` |
| `winnow` | 0.7.15 | `MIT` |
| `zmij` | 1.0.23 | `MIT` |
