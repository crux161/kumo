#!/usr/bin/env python3
#j735
#j738
#j745
"""Container half of scripts/mesa-drivers.py; never downloads or runs a target ELF."""
# Build-boundary and inventory decisions — SAKER, 2026-10-31.
import argparse
from collections import defaultdict
import csv
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tarfile

ROOT = Path("/repo")
TOOL = ROOT / "toolchain/mesa"
WORK = Path("/work/kumo-mesa")
OUT = Path("/output")
TAR_SHA = "794b3d1729075dae7ebd47c7283b7ca0cdb5dec668f7f11cc0a5d82f6138a0a5"
EXCLUDE = ("/gtest/", "standalone", "libglsl_util", "libvulkan_lite",
           "libgalliumvl.a", "libgalliumvlwinsys")
MATH = set("acosf asinf atan2f atanf cos cosf exp2f expf frexp frexpf ldexp ldexpf "
           "llrint llround llroundf log log10 log2f logf lrint lrintf lroundf pow powf sin sinf".split())
CXX = set("_ZNSt3__112__next_primeEm _ZNSt3__122__libcpp_verbose_abortEPKcz "
          "_ZNSt3__15mutex4lockEv _ZNSt3__15mutex6unlockEv _ZNSt3__15mutexD1Ev "
          "_ZdaPv _ZdlPvm _Znam _Znwm __cxa_guard_acquire __cxa_guard_release".split())
BUILD_ID = {"build_id_find_nhdr_for_addr", "build_id_length", "copy_build_id_to_sha1"}


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(command, log=None, check=True):
    p = subprocess.run(list(map(str, command)), capture_output=True, text=True)
    if log:
        (OUT / log).write_text(p.stdout + p.stderr)
    if check and p.returncode:
        raise RuntimeError(f"{command[0]} failed ({p.returncode}); {log}\n" + (p.stdout + p.stderr)[-5000:])
    return p


def stage():
    tar = ROOT / "build/mesa-native/mesa.tar.xz"
    assert sha(tar) == TAR_SHA, "Mesa source pin mismatch"
    if WORK.exists():
        shutil.rmtree(WORK)
    WORK.mkdir()
    with tarfile.open(tar) as archive:
        archive.extractall(WORK, filter="data")
    mesa = WORK / "mesa-26.2.5"
    run(["patch", "--batch", "--fuzz=0", "-p1", "-d", mesa, "-i", TOOL / "kumo-build.patch"], "patch.log")
    run(["patch", "--batch", "--fuzz=0", "-p1", "-d", mesa, "-i", TOOL / "native-device-header.patch"], "native-header.log")
    shutil.copytree(ROOT / "target/kumo-sysroot", WORK / "sysroot")
    include = WORK / "sysroot/include"
    # Private header repairs; the installed sysroot remains unchanged. All actual
    # runtime gaps still become undefined references, including open_memstream.
    math = (include / "math.h").read_text()
    for name in ("isfinite", "isinf", "isnan", "isnormal"):
        old = f"int {name}(double x);"
        if old in math:
            assert math.count(old) == 1, f"recheck math header repair: {name}"
            math = math.replace(old, f"#ifndef __cplusplus\nint ({name})(double x);\n#endif")
        else:
            assert f"#define {name}(x)" in math, f"missing classification macro: {name}"
    if "#define\tNAN\t\t(__nan.__uf)" in math:
        math = math.replace("#define\tNAN\t\t(__nan.__uf)", '#define NAN __builtin_nanf("")')
    else:
        assert '#define NAN __builtin_nanf("")' in math, "recheck NAN header"
    if "#define HUGE_VAL " not in math:
        math = "#define HUGE_VAL __builtin_huge_val()\n" + math
    (include / "math.h").write_text(math)
    for directory in ("include", "src"):
        for header in (ROOT / "vendor/relibc/openlibm" / directory).glob("*.h"):
            shutil.copyfile(header, include / header.name)
    (include / "sys/ioccom.h").write_text("/* Inventory header adapter; no ioctl implementation. */\n#include <sys/ioctl.h>\n")
    shutil.copytree("/usr/lib/llvm-22/include/c++/v1", WORK / "libcxx", symlinks=False)
    config = WORK / "libcxx/__config_site"
    text = config.read_text()
    for old, new in [("_LIBCPP_HAS_THREAD_API_PTHREAD 0", "_LIBCPP_HAS_THREAD_API_PTHREAD 1"),
                     ("_LIBCPP_HAS_LOCALIZATION 1", "_LIBCPP_HAS_LOCALIZATION 0"),
                     ("_LIBCPP_PSTL_BACKEND_STD_THREAD", "_LIBCPP_PSTL_BACKEND_SERIAL")]:
        assert old in text, f"libc++ config drift: {old}"
        text = text.replace(old, new)
    config.write_text(text)
    drm = WORK / "drm"
    drm.mkdir()
    for header in Path("/usr/include").glob("xf86drm*.h"):
        shutil.copyfile(header, drm / header.name)
    # Use Mesa's own DRM UAPI version with libdrm's declarations. No Linux host
    # header directories are in the cross compiler's search path.
    for header in (mesa / "include/drm-uapi").glob("*.h"):
        shutil.copyfile(header, drm / header.name)
    for name in ("kumo-aarch64.cross", "inventory-compat.h", "link-gl.c", "link-vk.c", "configure-drivers.sh"):
        shutil.copyfile(TOOL / name, WORK / name)
    return mesa


def category(symbol):
    if symbol in MATH or symbol == "open_memstream":
        return "relibc gap"
    if symbol in CXX:
        return "C++ runtime"
    if symbol.startswith("drmSyncobj"):
        return "syncobj / sync_file"
    if symbol == "drmIoctl":
        return "Panthor ioctl user"
    if symbol.startswith("drm"):
        return "DRM / libdrm"
    if symbol == "pan_kmod_kumo_bo_mmap":
        return "other: native capability backend"
    if symbol in BUILD_ID:
        return "other: static build ID"
    raise AssertionError(f"unclassified required symbol: {symbol}")


def inventory(archives):
    results = {}
    for driver in ("gl", "vk"):
        selected = [p for p in archives if
                    (driver == "gl" and "/vulkan/" not in str(p)) or
                    (driver == "vk" and not any(x in str(p) for x in ("/gallium/", "/mesa/", "/glsl/")))]
        obj = WORK / f"link-{driver}.o"
        run(["clang-22", "--target=aarch64-unknown-none", "-ffreestanding", "-fno-pic", "-O2",
             "-c", WORK / f"link-{driver}.c", "-o", obj])
        command = ["ld.lld-22", "-static", "--gc-sections", "-e", "main", "--no-demangle", "--error-limit=0",
                   f"--why-extract={OUT}/link-{driver}.extract", f"-Map={OUT}/link-{driver}.map",
                   "-o", OUT / f"link-{driver}.elf", obj, "--start-group", *selected,
                   WORK / "sysroot/lib/libc.a", "--end-group"]
        link = run(command, f"link-{driver}.errors", check=False)
        required = set(re.findall(r"^ld.lld-22: error: undefined symbol: (.+)$", link.stderr, re.M))
        roots = {"panfrost_create_screen", "st_api_create_context", "_mesa_glapi_get_dispatch", "glClear"} if driver == "gl" else {"vk_icdGetInstanceProcAddr"}
        assert not required & roots, f"probe root missing from archive set: {required & roots}"
        assert link.returncode == 1 and required, "inventory unexpectedly clean or link failed differently"
        errors = re.findall(r"^ld.lld-22: error: (.+)$", link.stderr, re.M)
        assert all(error.startswith("undefined symbol: ") for error in errors), errors
        run([*command, "--unresolved-symbols=ignore-all"], f"link-{driver}.permissive.log")
        # These deliberately incomplete ELFs are diagnostic artifacts, never programs.
        undefined = run(["llvm-nm-22", "-P", "--undefined-only", OUT / f"link-{driver}.elf"],
                        f"link-{driver}.undefined").stdout
        elf_undefined = {line.split()[0] for line in undefined.splitlines()}
        assert required <= elf_undefined
        extracted = set()
        with (OUT / f"link-{driver}.extract").open() as f:
            for row in csv.DictReader(f, delimiter="\t"):
                extracted.add(row["extracted"])
        arch_archive = "libpanfrost-v10.a(" if driver == "gl" else "libpanvk_v10.a("
        assert any(arch_archive in owner for owner in extracted), "v10 was not linked"
        header = run(["llvm-readelf-22", "-h", "-l", OUT / f"link-{driver}.elf"],
                     f"link-{driver}.elf-headers").stdout
        assert "AArch64" in header and "EXEC" in header and "INTERP" not in header
        provenance = defaultdict(list)
        weak = defaultdict(list)
        nm = run(["llvm-nm-22", "-A", "-P", "--undefined-only", *selected,
                  WORK / "sysroot/lib/libc.a"]).stdout
        for line in nm.splitlines():
            match = re.fullmatch(r"(.+)\[([^]]+)\]: (\S+) ([Uwv]) [0-9a-f]+ [0-9a-f]+", line)
            if not match:
                continue
            archive, member, symbol, binding = match.groups()
            if f"{archive}({member})" not in extracted:
                continue
            owner = f"{Path(archive).relative_to(WORK)}({member})"
            provenance[symbol].append(owner)
            if binding in ("w", "v"):
                weak[symbol].append(owner)
        entries = {}
        for symbol in sorted(elf_undefined):
            owners = sorted(set(provenance[symbol]))
            assert owners, f"no object provenance for {symbol}"
            if symbol in required:
                kind = category(symbol)
            else:
                assert weak[symbol], f"unexpected non-required undefined symbol: {symbol}"
                assert symbol.startswith(("panvk_", "vk_common_", "vk_cmd_enqueue_unless_primary_", "wsi_")) or symbol == "_ZTH23_mesa_glapi_tls_Context", symbol
                kind = "other: optional weak dispatch / TLS"
            entries[symbol] = {"category": kind, "required": symbol in required, "owners": owners}
        used_archives = sorted({name.split("(", 1)[0] for name in extracted})
        sizes = {str(Path(p).relative_to(WORK)): Path(p).stat().st_size for p in used_archives}
        run(["llvm-size-22", *[Path(p) for p in used_archives]], f"link-{driver}.object-sizes")
        run(["llvm-size-22", "-A", OUT / f"link-{driver}.elf"], f"link-{driver}.section-sizes")
        results[driver] = {"required_count": len(required), "optional_count": len(elf_undefined - required),
                           "undefined": entries, "archive_bytes": sizes, "extracted_objects": len(extracted),
                           "elf_bytes": (OUT / f"link-{driver}.elf").stat().st_size}
        print(f"{driver.upper()}: {len(required)} required gaps, {len(elf_undefined - required)} optional weak symbols", flush=True)
    (OUT / "inventory.json").write_text(json.dumps(results, indent=2) + "\n")
    with (OUT / "optional-undefined.tsv").open("w") as f:
        writer = csv.writer(f, delimiter="\t")
        writer.writerow(["driver", "symbol", "category", "archive(object)"])
        for driver, result in results.items():
            for symbol, entry in result["undefined"].items():
                if not entry["required"]:
                    writer.writerow([driver, symbol, entry["category"], "; ".join(entry["owners"])])
    lines = ["# G7k — measured Mesa driver link inventory", "", "Mesa 26.2.5, aarch64-unknown-none, KUMO relibc. These incomplete probe ELFs must never be executed.", "",
             "All upstream per-architecture variants are built; v10 is present in both links. The legacy JM kernel backend `panfrost_kmod.c` is excluded on KUMO; Panthor remains fully inventoried.", "",
             "| Driver | Required gaps | Optional weak symbols | Extracted objects | Probe ELF bytes |", "|---|---:|---:|---:|---:|"]
    for driver, result in results.items():
        lines.append(f"| {driver.upper()} | {result['required_count']} | {result['optional_count']} | {result['extracted_objects']} | {result['elf_bytes']} |")
    lines += ["", "Optional generated panvk dispatch entries and the GL TLS wrapper are verified against weak references in extracted objects. Every name and owner is in `optional-undefined.tsv`; every required and optional entry is in `inventory.json`. Local undefined symbol-table placeholders are included in this accounting.", "",
              "| Required symbol | Driver | Classification | Requesting archive(object), first owner |", "|---|---|---|---|"]
    symbols = sorted(set().union(*[{s for s, e in r["undefined"].items() if e["required"]} for r in results.values()]))
    for symbol in symbols:
        drivers = [d for d, r in results.items() if symbol in r["undefined"] and r["undefined"][symbol]["required"]]
        entry = results[drivers[0]]["undefined"][symbol]
        lines.append(f"| `{symbol}` | {'/'.join(d.upper() for d in drivers)} | {entry['category']} | `{entry['owners'][0]}` |")
    lines += ["", "## Archive sizes", "", "Object text/data/BSS sizes for each driver are in `link-{gl,vk}.object-sizes`; the extracted-member trace and map preserve the exact link graph. Full archive sizes include unextracted members.", "", "| Archive | GL bytes | VK bytes |", "|---|---:|---:|"]
    for archive in sorted(set().union(*[set(r["archive_bytes"]) for r in results.values()])):
        lines.append(f"| `{archive}` | {results['gl']['archive_bytes'].get(archive, '—')} | {results['vk']['archive_bytes'].get(archive, '—')} |")
    lines += ["", "No required pthread creation, dl, expat or driconf symbol remains unresolved in these links. This is link closure evidence only: existing libc exports can still refuse operations at runtime. G7l/G7m must prove their behavior.", "",
              "The plan's `_glapi_get_dispatch` is named `_mesa_glapi_get_dispatch` in this pinned source. GL retains that actual symbol, `panfrost_create_screen`, `st_api_create_context` and `glClear`; Vulkan retains `vk_icdGetInstanceProcAddr`.", ""]
    (OUT / "DRIVER-INVENTORY.md").write_text("\n".join(lines))
    return results


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--jobs", type=int, default=8)
    args = parser.parse_args()
    packages = run(["dpkg-query", "-W", "-f=${Package} ${Version}\n"]).stdout
    for package in ("clang-22", "lld-22", "llvm-22", "libc++-22-dev", "libc++abi-22-dev"):
        assert f"{package} 1:22.1.8-3\n" in packages, f"tool version drift: {package}"
    assert "meson 1.12.1-2\n" in packages and "libdrm-dev 2.4.134-3\n" in packages
    (OUT / "packages.txt").write_text(packages)
    stage()
    run(["sh", WORK / "configure-drivers.sh"], "setup.log")
    build = WORK / "build"
    targets = run(["ninja", "-C", build, "-t", "targets", "all"]).stdout
    archives = [build / line.split(":", 1)[0] for line in targets.splitlines()
                if re.search(r"\.a: STATIC_LINKER(?:_RSP)?$", line)
                and not any(part in "/" + line for part in EXCLUDE)]
    run(["ninja", "-C", build, f"-j{args.jobs}", *[p.relative_to(build) for p in archives]], "build.log")
    inventory(sorted(archives))
    deps = run(["ninja", "-C", build, "-t", "deps"], "headers.log").stdout
    assert not re.search(r"^\s+/usr/include/", deps, re.M), "Linux host header contamination"
    inputs = set(TOOL.glob("*")) | {ROOT / "scripts/mesa-drivers.py", WORK / "sysroot/lib/libc.a"}
    for directory in ("sysroot/include", "libcxx", "drm"):
        inputs.update((WORK / directory).rglob("*"))
    native = [Path("/work/build") / p for p in ("src/compiler/clc/mesa_clc", "src/compiler/spirv/vtn_bindgen2", "src/panfrost/clc/panfrost_compile")]
    inputs.update(native)
    for executable in ("clang-22", "ld.lld-22", "llvm-nm-22", "meson", "ninja", "python3"):
        inputs.add(Path(shutil.which(executable)).resolve())
    versions = {name: run([name, "--version"]).stdout for name in ("clang-22", "ld.lld-22", "meson", "ninja", "python3")}
    (OUT / "manifest.json").write_text(json.dumps({"mesa_tar_sha256": TAR_SHA, "tools": versions,
        "inputs_sha256": {str(p): sha(p) for p in sorted(inputs) if p.is_file()},
        "archives_sha256": {str(p.relative_to(WORK)): sha(p) for p in sorted(archives)}}, indent=2) + "\n")
    for p in archives:
        dest = OUT / "archives" / p.relative_to(build)
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(p, dest)
    print("G7k green: both cross-builds and probe links inventoried; no target execution", flush=True)


if __name__ == "__main__":
    main()
