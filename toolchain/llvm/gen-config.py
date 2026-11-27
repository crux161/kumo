#!/usr/bin/env python3
#J948_r14_001
"""Render LLVM's CMake config templates for native KUMO (aarch64-unknown-kumo).

LLVM's CMake probes the host and writes llvm/Config/*.h; KUMO has no native CMake run yet, so
this renders the pinned templates with the answers that run would give on relibc. Every value
here is a fact about relibc or a deliberate "off" for this first slice: no dlopen, no crash
backtraces, no posix_spawn (KUMO has no exec), no zlib/zstd/ICU/iconv. — TEAL
"""
import argparse
from pathlib import Path
import re

VALUES = {
    # abi-breaking.h
    "LLVM_ENABLE_ABI_BREAKING_CHECKS": "0",
    "LLVM_ENABLE_REVERSE_ITERATION": "0",
    # llvm-config.h
    "LLVM_DEFAULT_TARGET_TRIPLE": "aarch64-unknown-kumo",
    "LLVM_HOST_TRIPLE": "aarch64-unknown-kumo",
    "LLVM_ENABLE_THREADS": "1",
    "LLVM_HAS_ATOMICS": "1",
    "LLVM_ON_UNIX": "1",
    "LLVM_VERSION_MAJOR": "22",
    "LLVM_VERSION_MINOR": "1",
    "LLVM_VERSION_PATCH": "8",
    "PACKAGE_VERSION": "22.1.8",
    "LLVM_UNREACHABLE_OPTIMIZE": "1",
    "HAVE_SYSEXITS_H": "1",
    # config.h: what relibc's headers and libc.a provide
    "BUG_REPORT_URL": "https://github.com/llvm/llvm-project/issues/",
    "BACKTRACE_HEADER": "execinfo.h",
    "HAVE_DECL_FE_ALL_EXCEPT": "1",
    "HAVE_DECL_FE_INEXACT": "1",
    "HAVE_FUTIMENS": "1",
    "HAVE_GETPAGESIZE": "1",
    "HAVE_GETRUSAGE": "1",
    "HAVE_LIBPTHREAD": "1",
    "HAVE_PREAD": "1",
    "HAVE_PTHREAD_H": "1",
    "HAVE_PTHREAD_MUTEX_LOCK": "1",
    "HAVE_PTHREAD_RWLOCK_INIT": "1",
    "HAVE_SETENV": "1",
    "HAVE_SIGALTSTACK": "1",
    "HAVE_STRERROR_R": "1",
    "HAVE_SYSCONF": "1",
    "HAVE_SYS_MMAN_H": "1",
    "HAVE_SYS_IOCTL_H": "1",
    "HAVE_STRUCT_STAT_ST_MTIM_TV_NSEC": "1",
    "HAVE_UNISTD_H": "1",
    "LTDL_SHLIB_EXT": ".so",
    "LLVM_PLUGIN_EXT": ".so",
    "PACKAGE_BUGREPORT": "https://github.com/llvm/llvm-project/issues/",
    "PACKAGE_NAME": "LLVM",
    "PACKAGE_STRING": "LLVM 22.1.8",
    "PACKAGE_VENDOR": "KUMO",
}
TEMPLATES = {
    "abi-breaking.h.cmake": "abi-breaking.h",
    "llvm-config.h.cmake": "llvm-config.h",
    "config.h.cmake": "config.h",
    "Targets.def.in": "Targets.def",
    "AsmParsers.def.in": "AsmParsers.def",
    "AsmPrinters.def.in": "AsmPrinters.def",
    "Disassemblers.def.in": "Disassemblers.def",
    "TargetExegesis.def.in": "TargetExegesis.def",
    "TargetMCAs.def.in": "TargetMCAs.def",
}


def render(text):
    """CMake's configure_file: #cmakedefine01, #cmakedefine and ${VAR}/@VAR@ substitution.
    An unset variable is 0 for #cmakedefine01, #undef for #cmakedefine, and empty elsewhere."""
    substitute = lambda line: re.sub(r"\$\{(\w+)\}|@(\w+)@", lambda m: VALUES.get(m.group(1) or m.group(2), ""), line)
    out = []
    for line in text.splitlines():
        match = re.match(r"#cmakedefine01 (\w+)", line)
        if match:
            out.append(f"#define {match.group(1)} {VALUES.get(match.group(1), '0')}")
            continue
        match = re.match(r"#cmakedefine (\w+)(.*)", line)
        if match:
            name = match.group(1)
            out.append(f"#define {name}{substitute(match.group(2))}" if name in VALUES else f"/* #undef {name} */")
            continue
        out.append(substitute(line))
    return "\n".join(out) + "\n"


def generate(templates_dir, out):
    config = out / "llvm/Config"
    config.mkdir(parents=True, exist_ok=True)
    for template, name in TEMPLATES.items():
        (config / name).write_text(render((templates_dir / template).read_text()))
    support = out / "llvm/Support"
    support.mkdir(parents=True, exist_ok=True)
    # VCSRevision.h names no revision: the source pins live in SOURCE.lock.
    (support / "VCSRevision.h").write_text("#undef LLVM_REVISION\n#undef LLVM_REPOSITORY\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("templates", type=Path, help="llvm/include/llvm/Config with the .cmake/.in templates")
    parser.add_argument("out", type=Path, help="include directory to write llvm/Config/*.h into")
    args = parser.parse_args()
    generate(args.templates, args.out)


if __name__ == "__main__":
    main()
