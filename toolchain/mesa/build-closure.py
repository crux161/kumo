#!/usr/bin/env python3
#j740
#j745
#j762
"""Close both measured Mesa links and build the real KUMO utility acceptance."""
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import shlex
import shutil
import subprocess

ROOT = Path("/repo")
TOOL = ROOT / "toolchain/mesa"
MESA = Path("/work/kumo-mesa")
BUILD = MESA / "build"
OUT = Path("/output/closure")


def run(command):
    result = subprocess.run(list(map(str, command)), cwd=BUILD, capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError(" ".join(map(str, command)) + "\n" + result.stdout + result.stderr)
    return result.stdout


def compile_flags():
    commands = json.loads((BUILD / "compile_commands.json").read_text())
    entry = next(c for c in commands if c["file"].endswith("/ralloc.c"))
    original = shlex.split(entry["command"])
    flags = []
    skip = False
    for arg in original[1:]:
        if skip:
            skip = False
        elif arg in ("-o", "-MF", "-MQ", "-MT"):
            skip = True
        elif arg not in ("-c", "-MD", "-MMD", entry["file"]):
            flags.append(arg)
    kmod = next(c for c in commands if c["file"].endswith("/pan_kmod.c"))
    includes = [arg for arg in shlex.split(kmod["command"]) if arg.startswith("-I")]
    return [*flags, *includes, "-I", str(MESA / "mesa-26.2.5/src"), "-I", str(BUILD / "src"),
            "-I", str(MESA / "drm"), "-I", str(TOOL), "-I", str(MESA / "mesa-26.2.5/src/panfrost"), "-I", str(MESA / "mesa-26.2.5/src/panfrost/lib"), "-Wall", "-Wextra", "-Werror", "-Wno-unused-parameter",
            "-Wno-missing-field-initializers"]


def main():
    OUT.mkdir(exist_ok=True)
    measured = json.loads(Path("/output/inventory.json").read_text())
    header = MESA / "mesa-26.2.5/src/panfrost/lib/kmod/pan_kmod.h"
    if "return pan_kmod_kumo_bo_mmap" not in header.read_text():
        assert hashlib.sha256(header.read_bytes()).hexdigest() == "55fa7f89e5728a209fb615a26ee4099733bba29cee0695c754878f0c5dc6c4c4"
        run(["patch", "--batch", "--fuzz=0", "-p1", "-d", MESA / "mesa-26.2.5", "-i", TOOL / "native-device-header.patch"])
    assert hashlib.sha256(header.read_bytes()).hexdigest() == "5080af914d916bb48c4c7351f25334703fb5575f393e82500f82d4ef34b7f979"
    for name in ("kumo_gpu.h", "kumo_kmod.h"):
        source = TOOL / name
        dest = MESA / "mesa-26.2.5/src/gallium/drivers/panfrost" / name
        if not dest.exists() or source.read_bytes() != dest.read_bytes():
            shutil.copyfile(source, dest)

    # Install the current SDK without rewriting unchanged headers, so Ninja's
    # dependency graph rebuilds exactly the affected target objects. — SAKER
    for source in (ROOT / "target/kumo-sysroot").rglob("*"):
        if source.is_file():
            dest = MESA / "sysroot" / source.relative_to(ROOT / "target/kumo-sysroot")
            if not dest.exists() or source.read_bytes() != dest.read_bytes():
                dest.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(source, dest)
    names = sorted({name.removeprefix("build/") for driver in measured.values()
                    for name in driver["archive_bytes"] if name != "sysroot/lib/libc.a"})
    (OUT / "rebuild.log").write_text(run(["ninja", "-j8", *names]))
    # Header routes change the real link graph. Re-measure it before the C++
    # closure check so new native dependencies retain object provenance. — KESTREL
    spec = importlib.util.spec_from_file_location("drivers", TOOL / "build-drivers.py")
    drivers = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(drivers)
    targets = run(["ninja", "-t", "targets", "all"])
    archives = sorted(BUILD / line.split(":", 1)[0] for line in targets.splitlines()
                      if re.search(r"\.a: STATIC_LINKER(?:_RSP)?$", line)
                      and not any(part in "/" + line for part in drivers.EXCLUDE))
    measured = drivers.inventory(archives)
    (OUT / "cxx.log").write_text(run(["python3", ROOT / "toolchain/cxx/build.py"]))
    flags = compile_flags()
    objects = []
    depfiles = []
    for name in ("drm-refusal", "static-build-id", "util-smoke", "kumo_kmod", "native-device"):
        obj = OUT / f"{name}.o"
        dep = OUT / f"{name}.d"
        run(["clang-22", *flags, "-MD", "-MF", dep, "-c", TOOL / f"{name}.c", "-o", obj])
        objects.append(obj)
        depfiles.append(dep)
    runtime = Path("/output/cxx/libkumo-cxx.a")
    libc = MESA / "sysroot/lib/libc.a"
    link = ["ld.lld-22", "-static", "--gc-sections", "--build-id=sha1", "-z", "noexecstack"]
    for driver in ("gl", "vk"):
        archives = [MESA / name for name in measured[driver]["archive_bytes"] if name != "sysroot/lib/libc.a"]
        elf = OUT / f"link-{driver}.elf"
        run([*link, "-e", "main", f"-Map={elf}.map", f"--why-extract={elf}.extract", "-o", elf,
             MESA / f"link-{driver}.o", *objects[:2], *objects[3:], "--start-group", runtime, *archives, libc, "--end-group"])
        undefined = run(["llvm-nm-22", "-P", "--undefined-only", elf])
        (OUT / f"link-{driver}.undefined").write_text(undefined)
        optional = {symbol for symbol, entry in measured[driver]["undefined"].items() if not entry["required"]}
        # LLD localizes hidden weak Vulkan placeholders, which llvm-nm then
        # prints as U. They must remain in G7k's proven optional set. — SAKER
        assert {line.split()[0] for line in undefined.splitlines()} <= optional, undefined
        print(f"G7m {driver.upper()} link closure green: zero required undefined symbols", flush=True)
    # Preserve the real linker note in a mapped read-only segment and PT_NOTE.
    # The base SDK script discards all notes; only this Mesa target needs one. — SAKER
    script = (ROOT / "toolchain/relibc/smoke/link.ld").read_text()
    assert script.count("    tls    PT_TLS  FLAGS(4);") == 1
    script = script.replace("    tls    PT_TLS  FLAGS(4);", "    tls    PT_TLS  FLAGS(4);\n    note PT_NOTE FLAGS(4);")
    assert script.count("    .data : ALIGN(0x1000)") == 1
    script = script.replace("    .data : ALIGN(0x1000)",
                            "    .note.gnu.build-id : ALIGN(4) { KEEP(*(.note.gnu.build-id)) } :rodata :note\n\n    .data : ALIGN(0x1000)")
    (OUT / "link.ld").write_text(script)
    run(["clang-22", "--target=aarch64-unknown-none", "-c", ROOT / "toolchain/relibc/smoke/crt0.S",
         "-o", OUT / "crt0.o"])
    archives = [MESA / name for name in measured["gl"]["archive_bytes"] if name != "sysroot/lib/libc.a"]
    elf = OUT / "util-smoke"
    run([*link, "-T", OUT / "link.ld", f"-Map={elf}.map", f"--why-extract={elf}.extract", "-o", elf,
         OUT / "crt0.o", *objects, "--start-group", runtime, *archives, libc, "--end-group"])
    undefined = run(["llvm-nm-22", "--undefined-only", elf])
    assert not undefined.strip(), undefined
    notes = run(["llvm-readelf-22", "--notes", elf])
    (OUT / "elf-notes.txt").write_text(notes)
    build_id = re.search(r"Build ID: ([0-9a-f]{40})", notes).group(1)
    inputs = {libc, runtime, ROOT / "toolchain/relibc/smoke/link.ld", Path("/output/inventory.json"),
              BUILD / "compile_commands.json", TOOL / "kumo-build.patch"}
    inputs.update(TOOL / f"{name}.c" for name in ("drm-refusal", "static-build-id", "util-smoke"))
    inputs.update(TOOL / name for name in ["build-closure.py", "native-device-header.patch", "kumo_gpu.h", "kumo_kmod.h", "native-device.c", "kumo_kmod.c"])
    for dep in depfiles:
        for path in dep.read_text().replace("\\\n", " ").split(":", 1)[1].split():
            assert not path.startswith("/usr/include/"), "Linux target-header contamination"
            inputs.add((BUILD / path).resolve())
    (OUT / "manifest.json").write_text(json.dumps({
        "clang": run(["clang-22", "--version"]), "lld": run(["ld.lld-22", "--version"]), "build_id": build_id,
        "elf_sha256": hashlib.sha256(elf.read_bytes()).hexdigest(),
        "inputs_sha256": {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(inputs)},
        "compile_flags": flags,
        "archives_sha256": {str(MESA / "build" / name): hashlib.sha256((BUILD / name).read_bytes()).hexdigest()
                            for name in names},
    }, indent=2) + "\n")
    print(f"G7m Mesa utilities compile/link green: {elf}; build ID {build_id}", flush=True)


if __name__ == "__main__":
    main()
