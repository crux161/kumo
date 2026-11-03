#!/usr/bin/env python3
#j765
#j768
#j769
#j770
#j772
"""Prove the native Mesa device/VM backend with the real C runtime and real VMOs."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import sys

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
CSIF_GREEN = b"GL1-c1 native CSIF green: attested register counts, 256-register edge, cached query, foreign device refusal"
CONTEXT_SYNC_GREEN = b"GL1-c2 native context sync green: full-width binary IDs, signaled and empty payloads, first/second-create refusal, repeated group refusal, every sync released"
CONTEXT_INIT_GREEN = b"GL1-c destroy: the screen and device are gone; the session-owned group was not reused"
FRONTEND_LINES = [b"GL1-e drawable: owned 64x48 RGBA8 color and Z24/S8 depth/stencil attachments",
                  b"GL1-e current: Mesa's GLES2 state-tracker context made current; glthread and threaded pipe off",
                  b"GL1-e front: validate supplied the attachments; native flush completed before the callback; callback refusal propagated",
                  b"GL1-e destroy: current context unbound, drawable and display released, session context recreation refused"]
CLIENT_LINES = [b"GL1-f renderer: Mali-G610 MC4 (Panfrost)",
                b"GL1-f version: OpenGL ES ",
                b"GL1-f finish: public glFinish returned with GL_NO_ERROR and the native device live",
                b"GL1-f destroy: context unbound, drawable and display destroyed"]
COMPUTE_LINES = [b"GL2-a context: current GLES 3.1-capable context over the native GPU session",
                 b"GL2-a compiler: GLSL ES 310 compute source compiled and linked at runtime",
                 b"GL2-a buffer: 64 output words and 16 guard words initialized to 0xdeadbeef",
                 b"GL2-a dispatch: four groups of sixteen invocations, memory barrier and native glFinish complete",
                 b"GL2-a readback: 0 of 64 output words differ; 0 of 16 guards changed (modeled service, poison retained)",
                 b"GL2-a destroy: shader, program, SSBO, context, drawable and display released"]
BATCH_LINES = [b"GL1-d poll: a zero-timeout fence_finish found the batch still pending, and returned its wake",
               b"GL1-d fence: fence_finish completed natively, by ARM, WAIT and ARM again",
               b"GL1-d readback: 0 of 4096 pixels differ from 0xdeadbeef (the modeled service runs no GPU)",
               b"GL1-d destroy: fence, texture, context and screen are gone; every BO left through a deferred unmap",
               b"GL1-d model release: buffers=0 bindings=0 waits=1 heaps=0 bytes=0 submits=2 wakes=3"]
CLEAR_LINES = [b"GL2-b context: current GLES context over the native GPU session",
               b"GL2-b target: complete owned 64x48 RGBA8 frontend framebuffer",
               b"GL2-b baseline: public glReadPixels observed pixels different from the requested clear color",
               b"GL2-b clear: public glClear and native glFinish complete, expected RGBA8 0xff996633",
               b"GL2-b readback: 0 of 3072 pixels differ; 0 of 32 readback guards changed (modeled service, initial framebuffer retained)",
               b"GL2-b destroy: context, owned framebuffer drawable and display released"]


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
entry = next(c for c in commands if c['file'].endswith('/st_manager.c'))
frontend_flags, skip = [], False
for arg in shlex.split(entry['command'])[1:]:
    if skip:
        skip = False
    elif arg in ('-o', '-MF', '-MQ', '-MT'):
        skip = True
    elif arg not in ('-c', '-MD', '-MMD', entry['file']):
        frontend_flags.append(arg)
for source in ('kumo_gl', 'gl-frontend'):
    subprocess.run(['clang-22', *frontend_flags, '-I', '/repo/toolchain/mesa',
                    '-I', str(mesa / 'mesa-26.2.5/src/panfrost'),
                    '-I', str(mesa / 'mesa-26.2.5/src/panfrost/lib'),
                    '-I', str(mesa / 'mesa-26.2.5/src/panfrost/model'), '-Wall', '-Wextra',
                    '-Werror', '-Wno-unused-parameter', '-c', f'/repo/toolchain/mesa/{source}.c',
                    '-o', out / f'{source}.o'], check=True, cwd=build)
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
compat = out / 'generated/sys'
compat.mkdir(parents=True, exist_ok=True)
subprocess.run(['cp', mesa / 'sysroot/include/sys/ioccom.h', compat], check=True)
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
    return [deps / "kumo_gl.o", deps / "gl-frontend.o", deps / "drm-refusal.o", *archives, ROOT / "build/mesa-drivers/result/cxx/libkumo-cxx.a"]


def build(cases="device-smoke", out=OUT, crt0=ROOT / "toolchain/relibc/smoke/crt0.S", defines=(), gl=False, runtime=None):
    """Build a static ELF from `cases`.c and a Rust runtime (default: `cases`.rs).
    The GLES2 client reuses the metal session bootstrap with its own main. — KESTREL
    `defines` go to the cases file only, never the shared bridge: G7p-e selects its proof
    with one. — TIMBERDOODLE
    `gl` also links Gallium's panfrost driver, the frontend and lifecycle cases, the inventory's
    GL archives (G7k), drm-refusal (G7m) and the C++ runtime (G7l). — KESTREL"""
    # The standalone GLES client shares the established GPU-slot bootstrap without
    # copying its Rust session owner; existing proof targets keep their own runtime. — KESTREL
    runtime = runtime or cases
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
    mode = subprocess.check_output(["docker", "inspect", "--format", "{{.HostConfig.NetworkMode}}", "kumo-mesa-drivers-g7k"], text=True).strip()
    assert mode == "none", "native device build container must remain offline"
    gl_inputs = gl_dependencies(out) if gl else []
    if gl:
        flags.extend(["-isystem", mesa / "src/gallium/include",
                      "-isystem", out / "gl-deps/generated"])
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
         TOOL / f"{runtime}.rs", "-o", out / "libdevice.a"])
    run([cc, "--target=aarch64-unknown-none", "-c", crt0, "-o", out / "crt0.o"])
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
    gl_objects = []
    linker_script = ROOT / "toolchain/relibc/smoke/link.ld"
    compiler = None
    if gl:
        # Public GLSL compilation needs G7n's existing 2 MiB ELF stack request.
        # Reuse its script/header writer; Sora already bounds and grants it. — KESTREL
        spec = importlib.util.spec_from_file_location("mesa_compiler", TOOL / "build-compiler.py")
        compiler = importlib.util.module_from_spec(spec)
        # Keep the reused module's generated bytecode with build artifacts. — KESTREL
        previous_prefix = sys.pycache_prefix
        try:
            sys.pycache_prefix = str(out / "pycache")
            spec.loader.exec_module(compiler)
        finally:
            sys.pycache_prefix = previous_prefix
        compiler.ROOT, compiler.OUT = ROOT, out
        (out / "kumo").mkdir(exist_ok=True)
        linker_script = compiler.link_script()
        # Gallium's headers need generated ones from the container's build tree.
        for name in ["gl-screen", "gl-context-init", "gl-batch", *(["gl-client", "gl-context", "gl-compute", "gl-clear"] if cases == "device-smoke" else [])]:
            obj = out / f"{name}.o"
            run([cc, *flags, "-isystem", mesa / "src/gallium/include", "-isystem", out / "gl-deps/generated",
                 "-Wno-unused-parameter", "-c", TOOL / f"{name}.c", "-o", obj])
            gl_objects.append(obj)
    command = ["ld.lld", "-static", "--gc-sections", "-z", "noexecstack", "-T", linker_script,
               "-o", elf, out / "crt0.o", out / f"{cases}.o", out / "native-device.o", native / "kumo_kmod.o",
               native / "pan_props.o", native / "pan_model.o", *gl_objects,
               *(["--wrap=drmIoctl"] if gl and cases == "device-smoke" else []),
               "--start-group", out / "libdevice.a", *gl_inputs,
               out / "kmod.a", out / "util.a", out / "c11.a", sysroot / "lib/libc.a", "--end-group"]
    run(command)
    if compiler:
        compiler.set_stack_size(elf, compiler.STACK_BYTES)
    undefined = run([nm, "--undefined-only", elf], capture_output=True, text=True).stdout
    # State-tracker use pulls C++ shader/uniform queries. Their optional TLS
    # initializer is already measured by G7k; the TLS variable itself is defined
    # in Mesa's C glapi. Allow only measured weak symbols, never a strong gap. — KESTREL
    optional = set()
    if gl:
        inventory = json.loads((ROOT / "build/mesa-drivers/result/inventory.json").read_text())
        optional = {name for name, item in inventory["gl"]["undefined"].items() if not item["required"]}
    for line in undefined.splitlines():
        fields = line.split()
        assert len(fields) == 2 and fields[0] == "w" and fields[1] in optional, line
    run(["rustfmt", "--edition", "2021", "--check", TOOL / f"{runtime}.rs", TOOL / "native.rs"])
    paths = [elf, Path(__file__), TOOL / f"{cases}.c", TOOL / f"{runtime}.rs", Path(crt0), TOOL / "native-device.c", TOOL / "native.rs", TOOL / "kumo_gpu.h", TOOL / "kumo_kmod.h",
             TOOL / "kumo_kmod.c", ROOT / "lib/kumo-gpu/src/bo.rs", ROOT / "lib/kumo-gpu/src/c.rs",
             ROOT / "lib/kumo-gpu/src/client.rs", ROOT / "lib/kumo-gpu/src/c_sync.rs", ROOT / "lib/kumo-gpu/src/sync.rs",
             ROOT / "lib/kumo-gpu/src/group.rs", ROOT / "userland/servers/drv-gpu/src/lib.rs",
             native / "manifest.json", out / "kmod.a", out / "util.a", out / "c11.a", sysroot / "lib/libc.a"]
    paths.extend(TOOL / f"{obj.stem}.c" for obj in gl_objects)
    if gl:
        paths.extend([TOOL / "kumo_gl.c", TOOL / "kumo_gl.h", TOOL / "gl-frontend.c",
                      TOOL / "build-compiler.py", linker_script,
                      *gl_inputs, ROOT / "build/mesa-drivers/result/inventory.json"])
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
                        help="also link Gallium and run its screen, context, batch, frontend, GLES2 client, compute and clear proofs (modes 17-24)")
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
        assert proof.count(CSIF_GREEN) == 1 and proof.index(CSIF_GREEN) < proof.index(GREEN)
        print(CSIF_GREEN.decode(), flush=True)
        assert proof.count(WX_GREEN) == 1 and proof.index(WX_GREEN) < proof.index(GREEN)
        assert proof.count(NATIVE_GREEN) == 1 and proof.index(NATIVE_GREEN) < proof.index(GREEN)
        assert proof.count(GROUP_GREEN) == 1 and proof.index(GROUP_GREEN) < proof.index(GREEN)
        assert proof.count(HEAP_GREEN) == 1 and proof.index(HEAP_GREEN) < proof.index(GREEN)
        assert proof.count(AUTO_VA_GREEN) == 1 and proof.index(AUTO_VA_GREEN) < proof.index(GREEN)
        if args.gl_screen:
            assert proof.count(SCREEN_GREEN) == 1 and proof.index(SCREEN_GREEN) < proof.index(GREEN)
            assert proof.count(CONTEXT_SYNC_GREEN) == 1 and proof.index(CONTEXT_SYNC_GREEN) < proof.index(GREEN)
            assert proof.count(CONTEXT_INIT_GREEN) == 1 and proof.index(CONTEXT_INIT_GREEN) < proof.index(GREEN)
            print(SCREEN_GREEN.decode(), flush=True)
            print(CONTEXT_SYNC_GREEN.decode(), flush=True)
            print("GL1-c3 context green: Gallium initialization, native ARM/wake/completion, one group per session, heap and context cleanup; modeled service", flush=True)
            # GL1-d: each mode-20 line once, in order, ahead of the final green. — TIMBERDOODLE
            at = 0
            for line in BATCH_LINES:
                assert proof.count(line) == 1 and proof.index(line) >= at, line
                at = proof.index(line)
            assert at < proof.index(GREEN)
            print("GL1-d batch green: a clear flushed as one native submission, a held poll timed out with its wake returned, "
                  "the fence completed by ARM/WAIT/ARM, bo_wait and deferred unmaps answered from completion; modeled service", flush=True)
            at = 0
            for line in FRONTEND_LINES:
                assert proof.count(line) == 1 and proof.index(line) >= at, line
                at = proof.index(line)
            assert at < proof.index(GREEN)
            assert b"GL1-e model release: buffers=0 bindings=0 waits=1 heaps=0 bytes=0 submits=1" in proof
            print("GL1-e frontend green: real Mesa GLES2 current context, color/depth validation, completed flush callback, lifecycle refusals and clean teardown; modeled service", flush=True)
            at = 0
            for line in CLIENT_LINES:
                assert proof.count(line) == 1 and proof.index(line) >= at, line
                at = proof.index(line)
            assert at < proof.index(GREEN)
            assert b"GL1-f model release: buffers=0 bindings=0 waits=1 heaps=0 bytes=0 submits=1" in proof
            print("GL1-f GLES2 client green: public renderer/version queries, glFinish, native device status and clean teardown; modeled service", flush=True)
            at = 0
            for line in COMPUTE_LINES:
                assert proof.count(line) == 1 and proof.index(line) >= at, line
                at = proof.index(line)
            assert at < proof.index(GREEN)
            assert b"GL2-a model release: buffers=0 bindings=0 waits=1 heaps=0 bytes=0 submits=2" in proof
            print("GL2-a compute green: runtime GLSL compilation, native dispatch/barrier/finish, poison and guards retained, clean teardown; modeled service runs no GPU", flush=True)
            at = 0
            for line in CLEAR_LINES:
                assert proof.count(line) == 1 and proof.index(line) >= at, line
                at = proof.index(line)
            assert at < proof.index(GREEN)
            assert b"GL2-b model release: buffers=0 bindings=0 waits=1 heaps=0 bytes=0 submits=3" in proof
            print("GL2-b clear green: public glClear/glReadPixels, native completion, initial framebuffer retained, readback guards intact and clean teardown; modeled service runs no GPU", flush=True)
        print(AUTO_VA_GREEN.decode(), flush=True)
        print(HEAP_GREEN.decode(), flush=True)
        print(GROUP_GREEN.decode(), flush=True)
        print(NATIVE_GREEN.decode(), flush=True)
        print(WX_GREEN.decode(), flush=True)


if __name__ == "__main__":
    main()
