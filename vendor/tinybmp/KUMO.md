<!-- j648 -->
# tinybmp source snapshot

Copied from the supplied `resources/tinybmp`, whose manifest identifies version 0.7.0.
All Rust source files and both licenses are unchanged. The local Cargo manifest keeps
only the library dependency, pinned to embedded-graphics 0.8.2. Its eight transitive
registry packages are vendored under `vendor/cargo` with Cargo's checksum manifests.

The bundled `userland/bmp-view/assets/sample.bmp` is copied unchanged from
`resources/tinybmp/tests/logo-rgb888-24bpp.bmp` and shares tinybmp's MIT OR Apache-2.0 license.
Its SHA-256 is `995f7d9fd85d9f904d648685aafdf15c9eef9ef033d75b6e67325fbd6c8ec8a0`.

Source SHA-256 checksums:

```text
7023647d5e70bdd591d9cb7044efdde145695ad661ac41b99f05f106e7f6924a  src/color_table.rs
34201f581643881ae27c054ae4507fb562bfaa51cfc1737edfd19642027a1bdf  src/header/dib_header.rs
254f961044b0f8ab42ca407c233fb10d18580335fed2d6c27ba449065489f223  src/header/mod.rs
5d9c626685f574fa10a8137cbd52de4c285bb9a3e4784e6318118cb92734f0e9  src/iter.rs
0888b4df7749a1b8bee18bed732f717b317e03d72f5bfde0f593da62439506b8  src/lib.rs
27fba84c6326359b1fa9e21f52b817f0aec8e8fe2e946b18bb758278bdda97f0  src/parser.rs
6cf6bd912e056a1e8e600e8cae51550ff01433e194a20b50c8e1840826cafdba  src/raw_bmp.rs
cc407d26b803114aae048b159de15149896653cd8c39055d43a179d98a9b2b05  src/raw_iter.rs
```
