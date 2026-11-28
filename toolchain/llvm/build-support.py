#!/usr/bin/env python3
#J948_r14_001
#J949_r15_001
#J950_r16_001
#J951_r17_001
#J952_r18_001
#J953_r19_001
#J956_r22_001
"""Build LLVM's bootstrap libraries (llvmorg-22.1.8) for native KUMO.

The sources are the measured closure pinned in toolchain/llvm/SOURCE.lock; toolchain/llvm/kumo.patch
ports LLVM's Unix layer and KUMO triple on a staged copy; gen-config.py renders the CMake config headers.
Everything compiles against the installed libc++ layer (toolchain/cxx/build-libcxx.py) and relibc,
and the proof links against the resulting archives alone. — TEAL
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import re
import runpy
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "toolchain/llvm"
CXX = ROOT / "toolchain/cxx"
# LLVM's own tools run deep (parsers, the demangler, CommandLine): give thread zero a real stack.
MAIN_STACK = 4 * 1024 * 1024


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(command, *, capture=False, cwd=None):
    command = list(map(str, command))
    if capture:
        return subprocess.check_output(command, text=True, cwd=cwd)
    subprocess.run(command, check=True, cwd=cwd)
    return ""


def units(source):
    """Support's portable sources plus BLAKE3's portable and NEON paths (no x86 SIMD), and Demangle."""
    support = source / "llvm/lib/Support"
    found = sorted(support.glob("*.cpp")) + sorted(support.glob("*.c"))
    found += [support / "BLAKE3" / name for name in ("blake3.c", "blake3_dispatch.c", "blake3_portable.c", "blake3_neon.c")]
    demangle = sorted((source / "llvm/lib/Demangle").glob("*.cpp"))
    return found, demangle


def main(component="support"):
    if component not in ("support", "binaryformat", "targetparser", "object", "mc", "process", "redirect"):
        raise ValueError(f"unknown LLVM bootstrap component: {component}")
    object_library = component == "object"
    mc_library = component == "mc"
    targetparser = component in ("targetparser", "object", "mc")
    binaryformat = component in ("binaryformat", "targetparser", "object", "mc")
    whole_closure = component in ("targetparser", "mc")
    tag, label = {"support": ("SH9-d1", "LLVM Support"), "binaryformat": ("SH9-d2", "LLVM BinaryFormat"),
                  "targetparser": ("SH9-d3", "LLVM TargetParser"), "object": ("SH9-d4", "LLVM Object"),
                  "mc": ("SH9-d5", "LLVM MC"),
                  "process": ("SH9-p1", "LLVM process launch"),
                  "redirect": ("P2c", "LLVM spawn redirects")}[component]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sysroot", type=Path, default=ROOT / "target/kumo-sysroot")
    parser.add_argument("--out", type=Path, default=ROOT / f"target/llvm-{component}")
    parser.add_argument("--libcxx-out", type=Path, default=ROOT / "target/cxx-libcxx")
    parser.add_argument("--eh-out", type=Path, default=ROOT / "target/cxx-eh")
    parser.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    parser.add_argument("--tblgen", type=Path, help="LLVM 22.1.8 llvm-tblgen (TargetParser/Object/MC)")
    args = parser.parse_args()
    sysroot, out = args.sysroot.resolve(), args.out.resolve()
    libcxx_out, eh_out = args.libcxx_out.resolve(), args.eh_out.resolve()
    libc = sysroot / "lib/libc.a"
    run([sys.executable, CXX / "build-libcxx.py", "--sysroot", sysroot, "--out", libcxx_out, "--eh-out", eh_out])
    layer = libcxx_out / "install"
    for needed in ("include/c++/v1/__config_site", "lib/libc++.a", "lib/libc++abi.a", "lib/libunwind.a"):
        if not (layer / needed).is_file():
            raise RuntimeError(f"installed libc++ layer lacks {needed}")

    lock_path = TOOL / "SOURCE.lock"
    lock = json.loads(lock_path.read_text())
    vendor = TOOL / "vendor"
    for name, pin in lock["files"].items():
        data = (vendor / name).read_bytes()
        blob = hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest()
        if hashlib.sha256(data).hexdigest() != pin["sha256"] or blob != pin["git_blob"]:
            raise RuntimeError(f"source pin mismatch: {name}")
    source = out / "source"
    if source.exists():
        shutil.rmtree(source)
    for name in lock["files"]:
        destination = source / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(vendor / name, destination)
    # Inside this repository `git apply` resolves paths from the repository root, not the cwd,
    # and would skip a staged tree silently; name the stage as the patch directory then, as
    # scripts/relibc-check.sh does. Outside it, plain `git apply` from the stage works. — TEAL
    if source.is_relative_to(ROOT):
        run(["git", "-C", ROOT, "apply", f"--directory={source.relative_to(ROOT)}", TOOL / "kumo.patch"])
    else:
        run(["git", "apply", TOOL / "kumo.patch"], cwd=source)
    if "__kumo__" not in (source / "llvm/lib/Support/Unix/Path.inc").read_text():
        raise RuntimeError("kumo.patch did not apply to the staged LLVM sources")
    generated = out / "generated"
    if generated.exists():
        shutil.rmtree(generated)
    runpy.run_path(str(TOOL / "gen-config.py"))["generate"](source / "llvm/include/llvm/Config", generated)
    tablegen = (runpy.run_path(str(TOOL / "gen-target-tables.py"))["generate"](
        source, generated, lock, args.tblgen, object_tables=object_library or mc_library) if targetparser else None)
    fixtures = (runpy.run_path(str(TOOL / "make-object-fixtures.py"))["generate"](
        out / "fixtures", generated) if object_library else None)

    out.mkdir(parents=True, exist_ok=True)
    common = ["--target=aarch64-unknown-none", f"--sysroot={sysroot}", "-D__kumo__", "-O2", "-ffreestanding",
              "-fno-pic", "-fno-pie", "-fno-stack-protector", "-ffunction-sections", "-fdata-sections",
              "-D_GNU_SOURCE", "-D__STDC_CONSTANT_MACROS", "-D__STDC_FORMAT_MACROS", "-D__STDC_LIMIT_MACROS",
              "-DLLVM_BUILD_STATIC", "-I", generated, "-I", source / "llvm/include",
              "-I", source / "third-party/siphash/include"]
    cxx = ["-nostdinc++", "-isystem", layer / "include/c++/v1", "-isystem", sysroot / "include", "-std=c++17",
           "-fexceptions", "-frtti", "-funwind-tables"]
    c = ["-isystem", sysroot / "include", "-std=c11"]
    objects = out / "objects"
    objects.mkdir(exist_ok=True)

    def compile_unit(path):
        rel = path.relative_to(source)
        obj = objects / (str(rel).replace("/", "_") + ".o")
        lang = cxx if path.suffix == ".cpp" else c
        driver = "clang++" if path.suffix == ".cpp" else "clang"
        run([driver, *common, "-I", path.parent, *lang, "-MD", "-MF", f"{obj}.d", "-c", path, "-o", obj])
        return obj

    support_units, demangle_units = units(source)
    library_units = {"Support": support_units, "Demangle": demangle_units}
    component_paths = {name: name for name in (
        (["BinaryFormat"] if binaryformat else []) + (["TargetParser"] if targetparser else [])
        + (["Object"] if object_library else []) + (["MC"] if mc_library else []))}
    if mc_library:
        component_paths["DebugInfoDWARFLowLevel"] = "DebugInfo/DWARF/LowLevel"
    for name, directory in component_paths.items():
        # Keep every unit from each pinned upstream component's source list. — KESTREL
        cmake = source / f"llvm/lib/{directory}/CMakeLists.txt"
        names = re.findall(r"^  (\w+\.cpp)$", cmake.read_text(), re.MULTILINE)
        found = sorted((source / f"llvm/lib/{directory}").glob("*.cpp"))
        if not names or {path.name for path in found} != set(names):
            raise RuntimeError(f"{name} closure differs from its pinned CMake source list")
        library_units[name] = found
    with ThreadPoolExecutor(max_workers=args.jobs) as pool:
        library_objects = {name: list(pool.map(compile_unit, paths)) for name, paths in library_units.items()}
    libdir = out / "install/lib"
    libdir.mkdir(parents=True, exist_ok=True)
    archives = [libdir / f"libLLVM{name}.a" for name in library_units]
    for archive, members in zip(archives, library_objects.values()):
        archive.unlink(missing_ok=True)
        run(["llvm-ar", "rcs", archive, *members])

    smoke = TOOL / f"{component}-smoke.cpp"
    smoke_obj = out / "smoke.o"
    run(["clang++", *common, *cxx, "-MD", "-MF", f"{smoke_obj}.d", "-c", smoke, "-o", smoke_obj])
    crt = out / "crt0.o"
    run(["clang", "--target=aarch64-unknown-none", "-c", ROOT / "toolchain/relibc/smoke/crt0.S", "-o", crt])
    libcxx_build = runpy.run_path(str(CXX / "build-libcxx.py"))
    script = libcxx_build["stack_link_script"](out)
    elf = out / f"llvm-{component}-smoke"
    # Retain all component functions beyond Support/Demangle to prove their link closure. — KESTREL
    link_archives = (["--whole-archive", *archives[2:], "--no-whole-archive", *archives[:2]]
                     if whole_closure else archives)
    run(["ld.lld", "-static", *([] if whole_closure else ["--gc-sections"]), "--eh-frame-hdr", "-z", "noexecstack",
         "-T", CXX / "eh-sections.ld", "-T", script, f"-Map={out / 'link.map'}", "-o", elf, crt, smoke_obj,
         "--start-group", *link_archives, layer / "lib/libc++.a", layer / "lib/libc++abi.a",
         layer / "lib/libunwind.a", libc, "--end-group"])
    libcxx_build["set_stack_size"](elf, MAIN_STACK)
    undefined = run(["llvm-nm", "--undefined-only", elf], capture=True)
    (out / "undefined.txt").write_text(undefined)
    if undefined.strip():
        raise RuntimeError(f"unresolved symbols in the {label} smoke:\n{undefined}")

    # Record the measured LLVM inputs, and reject accidental use of an unpinned header. — KESTREL
    dependencies = set(tablegen["source_dependencies"] if tablegen else ())
    resource_roots = tuple(Path(run([driver, "-print-resource-dir"], capture=True).strip()).resolve()
                           for driver in ("clang", "clang++"))
    resource_inputs = {}
    for depfile in [*(objects / (str(path.relative_to(source)).replace("/", "_") + ".o.d")
                      for paths in library_units.values() for path in paths), Path(f"{smoke_obj}.d")]:
        for path in libcxx_build["dependencies"](depfile, ROOT, (source, generated, layer, sysroot, TOOL, *resource_roots)):
            if path.is_relative_to(source):
                name = str(path.relative_to(source))
                if name not in lock["files"]:
                    raise RuntimeError(f"unpinned LLVM input: {name}")
                dependencies.add(name)
            elif any(path.is_relative_to(root) for root in resource_roots):
                resource_inputs[str(path)] = sha(path)

    unbuilt = ["TargetParser"] if binaryformat and not targetparser else []
    if object_library or mc_library:
        links = set()
        for directory in component_paths.values():
            cmake = (source / f"llvm/lib/{directory}/CMakeLists.txt").read_text()
            # DEPENDS names generated targets, not LLVM link components. — KESTREL
            block = re.search(r"LINK_COMPONENTS\s+([^)]*)\)", cmake).group(1)
            links.update(re.split(r"\bDEPENDS\b", block, maxsplit=1)[0].split())
        unbuilt = sorted(links - library_units.keys())
    manifest = {
        "llvm_revision": lock["revision"],
        "llvm_ref": lock["ref"],
        "scope": f"LLVM {' + '.join(library_units)} for aarch64-unknown-kumo against the installed libc++ layer; "
                 "platform port staged by toolchain/llvm/kumo.patch; config rendered by gen-config.py",
        "unbuilt_link_components": unbuilt,
        "whole_archive_components": list(library_units)[2:] if whole_closure else [],
        "gc_sections": not whole_closure,
        "proof_scope": ("native ELF and archive readers" if object_library else
                        "generic MC data directives and DWARF low-level APIs" if mc_library else label),
        "tablegen": tablegen,
        "fixtures": fixtures,
        "units": {name.lower(): len(paths) for name, paths in library_units.items()},
        "source_dependencies": sorted(dependencies),
        "source_closure_files": len(lock["files"]),
        "source_closure_bytes": sum((vendor / name).stat().st_size for name in lock["files"]),
        "clang": run(["clang++", "--version"], capture=True).strip(),
        "clang_c": run(["clang", "--version"], capture=True).strip(),
        "compiler_resource_inputs_sha256": dict(sorted(resource_inputs.items())),
        "inputs_sha256": {str(path): sha(path) for path in (
            Path(__file__).resolve(), lock_path, TOOL / "kumo.patch", TOOL / "gen-config.py", smoke,
            CXX / "build-libcxx.py", CXX / "SOURCE.lock", libc, layer / "lib/libc++.a", layer / "include/c++/v1/__config_site",
            *((TOOL / f"build-{component}.py",) if component != "support" else ()),
            *((TOOL / "gen-target-tables.py",) if targetparser else ()),
            *((TOOL / "make-object-fixtures.py", TOOL / "object-fixture.S") if object_library else ()))},
        "generated_sha256": {str(path.relative_to(generated)): sha(path)
                             for path in sorted(generated.rglob("*")) if path.is_file()},
        "archives_sha256": {path.name: sha(path) for path in archives},
        "main_stack_bytes": MAIN_STACK,
        "elf_sha256": sha(elf),
        "elf_bytes": elf.stat().st_size,
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"{tag} {label} compile/link green: {elf} "
          f"({' + '.join(f'{len(paths)} {name}' for name, paths in library_units.items())} units)", flush=True)


if __name__ == "__main__":
    main()
