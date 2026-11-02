#!/usr/bin/env python3
#j751
#j752
#j753
#j757
#j759
"""Prove the native Mesa device/VM backend with the real C runtime and real VMOs."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parent.parent
TOOL = ROOT / "toolchain/mesa"
OUT = ROOT / "build/proofs/g7o-d"
GREEN = b"G7o-d native device green: complete props, Mesa device/BO/VM lifecycle, immediate binds, generations, loss and contained close"
WX_GREEN = b"G7o-e W^X driver shapes green: Gallium and panvk executable pools bind read-only, RW maps only, WB_MMAP masked, one 4 GiB window"
NATIVE_GREEN = b"G7p-c native C sync green: ABI 15, shared BO/sync owner, captured submits, bounded wakes, refusal/loss containment; zero SyncRead"
GROUP_GREEN = b"G7p-d group state green: READ-only status, fatal/timeout/unknown after channel close, cached C reports, no replay"
SCREEN_GREEN = b"GL1-b screen green: Gallium's panfrost screen over the native device, its caps, a render-target texture, and a clean destroy"
AUTO_VA_GREEN = b"GL1-b auto-VA green: AUTO_VA and TRACK_ACTIVITY VMs, lowest-fit addresses, a freed range reused, caller addresses refused, a full window refused with nothing sent"
HEAP_GREEN = b"G7q-c native heaps green: ABI 15 heap callbacks, Panthor's refusals with no I/O, server-VM heaps, VM teardown destroys them, exact bytes back"


def run(command, **kwargs):
    return subprocess.run(list(map(str, command)), cwd=ROOT, check=True, **kwargs)


#j759
# The GL archives as G7k's inventory measured them. kmod and c11 are already linked as
# device dependencies, and libc is the host sysroot's. — TIMBERDOODLE
GL_SKIP = {"build/src/panfrost/lib/kmod/libpankmod_lib.a", "build/src/c11/impl/libmesa_util_c11.a", "sysroot/lib/libc.a"}


def gl_dependencies(out):
    """Materialize every GL archive and compile drm-refusal inside the offline container, where
    the build tree, its compile flags and libdrm's headers live, then copy them out."""
    inventory = json.loads((ROOT / "build/mesa-drivers/result/inventory.json").read_text())
    names = [n for n in inventory["gl"]["archive_bytes"] if n not in GL_SKIP]
    script = """import json, shlex, subprocess, sys
from pathlib import Path
mesa = Path('/work/kumo-mesa'); build = mesa / 'build'
out = Path('/output/gl-deps'); out.mkdir(exist_ok=True)
for index, name in enumerate(json.loads(sys.argv[1])):
    archive = mesa / name
    target = out / f'{index:02d}-{archive.name}'
    target.unlink(missing_ok=True)
    # A regular archive (libGLESv2 embeds its objects) is copied as is; a thin one is rebuilt
    # from the members it names.
    if archive.read_bytes()[:7] == b'!<arch>':
        subprocess.run(['cp', archive, target], check=True)
        continue
    members = subprocess.check_output(['llvm-ar-22', 't', archive], text=True).splitlines()
    subprocess.run(['llvm-ar-22', 'rcs', target, *[archive.parent / m for m in members]], check=True)
commands = json.loads((build / 'compile_commands.json').read_text())
entry = next(c for c in commands if c['file'].endswith('/ralloc.c'))
flags, skip = [], False
for arg in shlex.split(entry['command'])[1:]:
    if skip:
        skip = False
    elif arg in ('-o', '-MF', '-MQ', '-MT'):
        skip = True
    elif arg not in ('-c', '-MD', '-MMD', entry['file']):
        flags.append(arg)
generated = out / 'generated/util/format'
generated.mkdir(parents=True, exist_ok=True)
subprocess.run(['cp', build / 'src/util/format/u_format_gen.h', generated], check=True)
subprocess.run(['clang-22', *flags, '-I', str(mesa / 'drm'), '-Wall', '-Wextra', '-Werror',
                '-Wno-unused-parameter', '-c', '/repo/toolchain/mesa/drm-refusal.c',
                '-o', out / 'drm-refusal.o'], check=True, cwd=build)
"""
    run(["docker", "exec", "kumo-mesa-drivers-g7k", "python3", "-c", script, json.dumps(names)])
    deps = out / "gl-deps"
    if deps.exists():
        shutil.rmtree(deps)
    run(["docker", "cp", "kumo-mesa-drivers-g7k:/output/gl-deps", deps])
    archives = sorted(deps.glob("[0-9][0-9]-*.a"))
    assert len(archives) == len(names), "every GL archive must be materialized"
    return [deps / "drm-refusal.o", *archives, ROOT / "build/mesa-drivers/result/cxx/libkumo-cxx.a"]


def build(cases="device-smoke", out=OUT, crt0=ROOT / "toolchain/relibc/smoke/crt0.S", defines=(), gl=False):
    """Build one static proof ELF from `cases`.c and `cases`.rs over the real bridge. The
    G7o-f metal client reuses it with its own cases, startup file and output. — TIMBERDOODLE
    `defines` go to the cases file only, never the shared bridge: G7p-e selects its proof
    with one. — TIMBERDOODLE
    `gl` also links Gallium's panfrost driver: gl-screen.c, the inventory's GL archives (G7k),
    drm-refusal (G7m) and the C++ runtime (G7l). The ELF is then about 17 MB. — TIMBERDOODLE"""
    out.mkdir(parents=True, exist_ok=True)
    # This verifies the source/header pins, prepares native headers, builds the same C
    # bridge and checks its measured dependency closure; no download. — KESTREL
    run(["python3", ROOT / "scripts/mesa-native-probe.py"])
    native = ROOT / "build/mesa-native/probe"
    mesa = native / "source/mesa-26.2.5"
    sysroot = ROOT / "target/kumo-sysroot"
    cc = shutil.which("clang")
    nm = shutil.which("llvm-nm")
    assert cc and nm
    flags = ["--target=aarch64-unknown-none", "-D__kumo__", "-std=c11", "-O2", "-ffreestanding",
             "-fno-pic", "-fno-pie", "-fno-stack-protector", "-ffunction-sections", "-fdata-sections",
             "-Wall", "-Wextra", "-Werror", f"--sysroot={sysroot}", "-isystem", sysroot / "include",
             "-I", TOOL, "-I", native / "compat", "-DHAVE_PTHREAD", "-DHAVE_ENDIAN_H", "-DHAVE_STRUCT_TIMESPEC"]
    for include in ["include", "src", "src/panfrost", "src/panfrost/lib", "src/panfrost/model"]:
        flags.extend(["-isystem", mesa / include])
    flags.extend(["-isystem", native / "generated"])
    for name in ["native-device", cases]:
        run([cc, *flags, *(defines if name == cases else ()), "-Wno-unused-parameter", "-c",
             TOOL / f"{name}.c", "-o", out / f"{name}.o"])
    run(["cargo", "build", "--offline", "--locked", "--release", "--target", "aarch64-unknown-none",
         "-p", "kumo-rt", "-p", "kumo-gpu", "-p", "kumo-abi"])
    if cases == "device-smoke":
        run(["cargo", "build", "--offline", "--locked", "--release", "--target", "aarch64-unknown-none", "-p", "drv-gpu", "--lib"])
    deps = ROOT / "target/aarch64-unknown-none/release"
    elf = out / cases
    run(["rustc", "--edition=2021", "--crate-type=staticlib", "--target=aarch64-unknown-none",
         "-C", "panic=abort", "-C", "opt-level=2", "--extern", f"kumo_gpu={deps / 'libkumo_gpu.rlib'}",
         "--extern", f"kumo_abi={deps / 'libkumo_abi.rlib'}", *(["--extern", f"drv_gpu={deps / 'libdrv_gpu.rlib'}"] if cases == "device-smoke" else []), "-L", f"dependency={deps / 'deps'}",
         TOOL / f"{cases}.rs", "-o", out / "libdevice.a"])
    run([cc, "--target=aarch64-unknown-none", "-c", crt0, "-o", out / "crt0.o"])
    mode = subprocess.check_output(["docker", "inspect", "--format", "{{.HostConfig.NetworkMode}}", "kumo-mesa-drivers-g7k"], text=True).strip()
    assert mode == "none", "native device build container must remain offline"
    # Mesa's exported archives are thin. Materialize their real cached members, rather
    # than using dangling host paths or replacing the generic Mesa lifecycle. — KESTREL
    materialize = """from pathlib import Path
import subprocess
root=Path('/work/kumo-mesa/build/src')
out=Path('/output/device-deps');out.mkdir(exist_ok=True)
for name, relative in [('kmod','panfrost/lib/kmod/libpankmod_lib.a'),('util','util/libmesa_util.a'),('c11','c11/impl/libmesa_util_c11.a')]:
    archive=root/relative
    members=subprocess.check_output(['llvm-ar-22','t',archive],text=True).splitlines()
    target=out/(name+'.a')
    target.unlink(missing_ok=True)
    subprocess.run(['llvm-ar-22','rcs',target,*[archive.parent/m for m in members]],check=True)
"""
    run(["docker", "exec", "kumo-mesa-drivers-g7k", "python3", "-c", materialize])
    for name in ['kmod', 'util', 'c11']:
        run(["docker", "cp", f"kumo-mesa-drivers-g7k:/output/device-deps/{name}.a", out / f"{name}.a"])
    gl_inputs = []
    if gl:
        gl_inputs = gl_dependencies(out)
        # Gallium's headers need generated ones from the container's build tree.
        run([cc, *flags, "-isystem", mesa / "src/gallium/include", "-isystem", out / "gl-deps/generated",
             "-Wno-unused-parameter", "-c", TOOL / "gl-screen.c", "-o", out / "gl-screen.o"])
    command = ["ld.lld", "-static", "--gc-sections", "-z", "noexecstack", "-T", ROOT / "toolchain/relibc/smoke/link.ld",
               "-o", elf, out / "crt0.o", out / f"{cases}.o", out / "native-device.o", native / "kumo_kmod.o",
               native / "pan_props.o", native / "pan_model.o", *([out / "gl-screen.o"] if gl else []),
               "--start-group", out / "libdevice.a", *gl_inputs,
               out / "kmod.a", out / "util.a", out / "c11.a", sysroot / "lib/libc.a", "--end-group"]
    run(command)
    undefined = run([nm, "--undefined-only", elf], capture_output=True, text=True).stdout
    assert not undefined.strip(), "Device proof ELF has unresolved symbols"
    run(["rustfmt", "--edition", "2021", "--check", TOOL / f"{cases}.rs", TOOL / "native.rs"])
    paths = [elf, Path(__file__), TOOL / f"{cases}.c", TOOL / f"{cases}.rs", Path(crt0), TOOL / "native-device.c", TOOL / "native.rs", TOOL / "kumo_gpu.h", TOOL / "kumo_kmod.h",
             TOOL / "kumo_kmod.c", ROOT / "lib/kumo-gpu/src/bo.rs", ROOT / "lib/kumo-gpu/src/c.rs",
             ROOT / "lib/kumo-gpu/src/client.rs", ROOT / "lib/kumo-gpu/src/c_sync.rs", ROOT / "lib/kumo-gpu/src/sync.rs",
             ROOT / "lib/kumo-gpu/src/group.rs", ROOT / "userland/servers/drv-gpu/src/lib.rs",
             native / "manifest.json", out / "kmod.a", out / "util.a", out / "c11.a", sysroot / "lib/libc.a"]
    (out / "manifest.json").write_text(json.dumps({"defines": list(defines), "input_sha256": {
        str(path): hashlib.sha256(path.read_bytes()).hexdigest() for path in paths}}, indent=2) + "\n")
    return elf


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--native-sync", action="store_true", help="select G7p-c's native C acceptance and proof directory")
    parser.add_argument("--group-state", action="store_true", help="select G7p-d's group-state acceptance and proof directory")
    parser.add_argument("--heaps", action="store_true", help="select G7q-c's native heap acceptance and proof directory")
    parser.add_argument("--gl-screen", action="store_true",
                        help="also link Gallium and run GL1-b's screen as mode 17 (a 17 MB proof ELF)")
    args = parser.parse_args()
    out = (ROOT / "build/proofs/gl1-b" if args.gl_screen else ROOT / "build/proofs/g7q-c" if args.heaps
           else ROOT / "build/proofs/g7p-d" if args.group_state
           else ROOT / "build/proofs/g7p-c" if args.native_sync else OUT)
    elf = build(out=out, defines=["-DKUMO_GL_SCREEN=1"] if args.gl_screen else (), gl=args.gl_screen)
    print(f"G7o-d native device compile/link green: {elf}", flush=True)
    if args.run:
        spec = importlib.util.spec_from_file_location("uefi_smoke", ROOT / "scripts/cxx-smoke.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        transcript = module.boot(ROOT / "build/images/qemu-virt-aarch64", 120, elf=elf,
                                 label="G7o-d native device", green=GREEN, tag="G7o-d")
        proof = transcript.split(b"G7o-d native device begin", 1)[1]
        assert proof.count(WX_GREEN) == 1 and proof.index(WX_GREEN) < proof.index(GREEN)
        assert proof.count(NATIVE_GREEN) == 1 and proof.index(NATIVE_GREEN) < proof.index(GREEN)
        assert proof.count(GROUP_GREEN) == 1 and proof.index(GROUP_GREEN) < proof.index(GREEN)
        assert proof.count(HEAP_GREEN) == 1 and proof.index(HEAP_GREEN) < proof.index(GREEN)
        assert proof.count(AUTO_VA_GREEN) == 1 and proof.index(AUTO_VA_GREEN) < proof.index(GREEN)
        if args.gl_screen:
            assert proof.count(SCREEN_GREEN) == 1 and proof.index(SCREEN_GREEN) < proof.index(GREEN)
            print(SCREEN_GREEN.decode(), flush=True)
        print(AUTO_VA_GREEN.decode(), flush=True)
        print(HEAP_GREEN.decode(), flush=True)
        print(GROUP_GREEN.decode(), flush=True)
        print(NATIVE_GREEN.decode(), flush=True)
        print(WX_GREEN.decode(), flush=True)


if __name__ == "__main__":
    main()
