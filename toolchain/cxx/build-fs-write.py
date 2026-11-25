#!/usr/bin/env python3
#J944_r10_001
"""Build the pinned libc++ filesystem write slice (ofstream + mutating operations) for KUMO."""
import argparse
from pathlib import Path
import runpy

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "toolchain/cxx"
FSTREAM = runpy.run_path(str(TOOL / "build-fstream.py"))
# SH9-c4's localized library plus the filesystem translation units. With localization on and
# no <sys/sendfile.h>, operations.cpp takes libc++'s portable fstream copy_file path, so the
# SH9-c2 sendfile declaration is not needed here. — TEAL
UNITS = FSTREAM["UNITS"] + ("filesystem/operations", "filesystem/directory_iterator",
                            "filesystem/directory_entry", "filesystem/path", "filesystem/filesystem_error")
# The relibc calls the proof must reach: the mutations themselves, and the fd walk remove_all
# uses on a POSIX libc.
REACHED = FSTREAM["RELIBC_LOCALE_CALLS"] + ("mkdir", "rename", "remove", "truncate", "openat", "fdopendir",
                                            "unlinkat")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sysroot", type=Path, default=ROOT / "target/kumo-sysroot")
    parser.add_argument("--out", type=Path, default=ROOT / "target/cxx-fs-write")
    parser.add_argument("--eh-out", type=Path, default=ROOT / "target/cxx-eh")
    args = parser.parse_args()
    FSTREAM["build"](args.sysroot, args.out, args.eh_out, units=UNITS, smoke=TOOL / "fs-write-smoke.cpp",
                     archive_name="libkumo-cxx-fs-write.a", elf_name="cxx-fs-write-smoke", reached=REACHED,
                     read_only=False, label="SH9-c5 libc++ filesystem writes",
                     scope="libc++ ofstream and mutating std::filesystem operations over relibc on the granted "
                           "RAM FAT volume; no VFS or kernel calls")


if __name__ == "__main__":
    main()
