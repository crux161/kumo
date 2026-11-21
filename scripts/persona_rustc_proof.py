#j920
#j924
#j925
"""The SH7-e3 boot commands and Linux oracle shared by the QEMU and metal suites."""

BOOT = (
    b"echo SH7-e3 begins\n"
    b"linux /opt/rust/bin/rustc -Vv\n"
    b"linux /opt/rust/bin/rustc --edition 2021 --crate-type lib --target aarch64-unknown-none "
    b"-C opt-level=2 --emit=obj -o /w/add.o /w/add.rs\n"
    b"linux /bin/objprobe /w/add.o\n"
    b"echo SH7-e3 ends\n"
)

VERSION = (
    "rustc 1.98.0-nightly (91fe22da8 2026-06-21)",
    "binary: rustc",
    "commit-hash: 91fe22da8084a1c9e993d78d4a56f22ab8396236",
    "commit-date: 2026-06-21",
    "host: aarch64-unknown-linux-musl",
    "release: 1.98.0-nightly",
    "LLVM version: 22.1.7",
)
# j924 (SH7-f): the Linux rustc links a native KUMO executable on KUMO (it spawns rust-lld
# through musl's posix_spawn), a second process hashes it, and Sora runs it.
LINK_BOOT = (
    b"echo SH7-f begins\n"
    b"linux /opt/rust/bin/rustc @/w/hello.args\n"
    b"linux /bin/objprobe /w/hello\n"
    b"run /w/hello\n"
    b"echo SH7-f ends\n"
)
# The pinned rustc and rust-lld, the same arguments and sysroot, in the pinned Alpine image
# (build/verification/sh7f/oracle).
LINKED = "objprobe: /w/hello 9032 bytes sha256 2a6e456ecbf632f91edc296e3a27be25cab26f74f1784e456b4a11874970b2eb"
HELLO = "SH7-f hello from KUMO"


def validate_link(text):
    """rustc exits 0, the hash matches Linux's, then the executable runs on KUMO, in order."""
    begin, end = "SH7-f begins", "SH7-f ends"
    if text.count(begin) != 1 or text.count(end) != 1:
        raise RuntimeError("missing or duplicated SH7-f boot markers")
    window = text[text.index(begin) + len(begin):text.index(end)]
    expected = ("linux: /opt/rust/bin/rustc exited 0", LINKED, "linux: /bin/objprobe exited 0", HELLO)
    lines = [line.partition("] ")[2] if line.startswith("[") else line for line in window.splitlines()]
    if [line for line in lines if line in expected] != list(expected):
        raise RuntimeError("SH7-f link, digest or native run differs from Linux")


# j925 (SH7-g): Cargo builds the oracle's two-crate workspace on KUMO: a host proc macro (built,
# linked and loaded by rustc) and a no_std binary that uses it, both through rust-lld. A second
# process hashes the binary and Sora runs it. The environment and working directory are the
# Linux oracle's (build/verification/sh7g/lld-oracle/run.sh).
CARGO_ENV = (
    b"PWD=/w PATH=/opt/rust/bin:/usr/local/bin:/usr/bin:/bin:/sbin CARGO_TARGET_DIR=/out/target "
    b"CARGO_HOME=/out/cargo-home CARGO_INCREMENTAL=0 TMPDIR=/tmp OPENSSL_armcap=0"
)
CARGO_BIN = b"/out/target/aarch64-unknown-kumo/release/sh7-hello"
CARGO_BOOT = (
    b"echo SH7-g begins\n"
    b"env " + CARGO_ENV + b" linux /opt/rust/bin/cargo -Zhost-config -Ztarget-applies-to-host "
    b"-Zjson-target-spec build --offline --release --target /sdk/share/kumo/aarch64-unknown-kumo.json\n"
    b"linux /bin/objprobe " + CARGO_BIN + b"\n"
    b"run " + CARGO_BIN + b"\n"
    b"echo SH7-g ends\n"
)
CARGO_OBJECT = ("objprobe: /out/target/aarch64-unknown-kumo/release/sh7-hello 9040 bytes sha256 "
                "0453cd1804f6cfe26474cf37d232afb6921f3bf44628ba1fa787ad87d02b7a24")
CARGO_HELLO = "SH7-g hello from a proc macro"


def validate_cargo(text):
    """cargo exits 0, the binary's hash is Linux's, then it runs on KUMO, in that order."""
    begin, end = "SH7-g begins", "SH7-g ends"
    if text.count(begin) != 1 or text.count(end) != 1:
        raise RuntimeError("missing or duplicated SH7-g boot markers")
    window = text[text.index(begin) + len(begin):text.index(end)]
    expected = ("linux: /opt/rust/bin/cargo exited 0", CARGO_OBJECT, "linux: /bin/objprobe exited 0", CARGO_HELLO)
    lines = [line.partition("] ")[2] if line.startswith("[") else line for line in window.splitlines()]
    if [line for line in lines if line in expected] != list(expected):
        raise RuntimeError("SH7-g build, digest or native run differs from Linux")


OBJECT = "objprobe: /w/add.o 1048 bytes sha256 2e82dd52f777d11d706b752dbffbcac60cdf24d3cc40ed9b5626a6bbe32992cc"


def validate(text):
    """Match both successful rustc exits in order, then the independent file read."""
    begin, end = "SH7-e3 begins", "SH7-e3 ends"
    if text.count(begin) != 1 or text.count(end) != 1:
        raise RuntimeError("missing or duplicated SH7-e3 boot markers")
    window = text[text.index(begin) + len(begin):text.index(end)]
    rustc_exit = "linux: /opt/rust/bin/rustc exited 0"
    expected = (*VERSION, rustc_exit, rustc_exit, OBJECT, "linux: /bin/objprobe exited 0")
    # tio prefixes each serial line with a timestamp on metal.
    lines = [line.partition("] ")[2] if line.startswith("[") else line for line in window.splitlines()]
    if [line for line in lines if line in expected] != list(expected):
        raise RuntimeError("SH7-e3 version, compiler exits or object digest differs from Linux")
