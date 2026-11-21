#!/usr/bin/env python3
#j931
"""SH8-e: SH7-g's Cargo workspace builds on RedoxFS and survives redoxfsd mid-build.

All Linux files, including the toolchain, inputs, Cargo home, TMPDIR and outputs, are on
one persistent virtio disk. The second clean build runs once, with a server kill after
Cargo has made its host-dependency directory. A third cold boot hashes and runs its result.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess

import persona_rustc_proof as oracle

ROOT = Path(__file__).resolve().parent.parent
PROGRAM = b"/media/DISK/opt/rust/bin/cargo"
COMMAND = (PROGRAM + b" -Zhost-config -Ztarget-applies-to-host -Zjson-target-spec build "
           b"--offline --release --target /sdk/share/kumo/aarch64-unknown-kumo.json\n")
VERIFY = (b"linux /media/DISK/bin/objprobe " + oracle.CARGO_BIN + b"\n"
          b"run /media/DISK" + oracle.CARGO_BIN + b"\n")
ARMED = b"/out/target/release/deps/message-d6b32edc9b72d2ca.d"


def load_redox():
    spec = importlib.util.spec_from_file_location("redoxsmoke", ROOT / "scripts/redoxfs-smoke.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "build/verification/sh8/sh8e")
    parser.add_argument("--timeout", type=float, default=2400)
    parser.add_argument("--sdk-source", type=Path, default=ROOT / "build/sysroot/aarch64-unknown-kumo",
                        help="SH7-g's exact SDK input; every byte is checked against its pins")
    args = parser.parse_args()
    work = args.output.resolve()
    work.mkdir(parents=True, exist_ok=True)
    redox = load_redox()
    axh = redox.load_axh()
    redox.subset_post_validate(axh)
    qemu = shutil.which("qemu-system-aarch64")
    if not qemu:
        raise RuntimeError("qemu-system-aarch64 missing")
    firmware = Path(qemu).resolve().parent.parent / "share/qemu"
    controls = (*axh.CONTROLS, "KUMO_PERSONA_LINK", "KUMO_PERSONA_CARGO", "KUMO_REDOXPROBE", "KUMO_INITRD_ZSTD")
    default_env = {k: v for k, v in os.environ.items() if k not in controls}
    proof_env = dict(default_env, KUMO_CONSOLE_LOGLEVEL="6", KUMO_REDOXPROBE="1", KUMO_INITRD_ZSTD="0")
    manifest = {"head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip(),
                "diff": subprocess.check_output(["git", "diff", "--binary"], cwd=ROOT).decode(), "cases": []}
    try:
        seed = work / "seed.img"
        with (work / "seed.log").open("wb") as log:
            subprocess.run(["cargo", "xtask", "redoxfs-image", str(seed), "--size", "1536",
                            "--uuid", "SH8-e Cargo", "--persona-cargo", "--sdk-source", str(args.sdk_source), "--force"],
                           cwd=ROOT, stdout=log, stderr=log, check=True)
        with (work / "build-proof.log").open("wb") as log:
            subprocess.run(axh.IMAGE_ARGS, cwd=ROOT, env=proof_env, stdout=log, stderr=log, check=True)
        for phase in ("build", "restart", "cold"):
            name = f"redoxfs-cargo-{phase}"
            disk = work / ("build.img" if phase == "build" else "restart.img")
            if phase != "cold":
                shutil.copyfile(seed, disk)
            body = b"mount\n"
            if phase == "build":
                body += b"linux --rw /media/DISK/bin/writeprobe\n"
            if phase == "build":
                body += b"env " + oracle.CARGO_ENV + b" linux --rw " + COMMAND
            if phase == "restart":
                body += (b"env " + oracle.CARGO_ENV + b" bg linux --rw " + COMMAND
                         + b"fsd-crash-proof " + ARMED + b" 1 " + oracle.CARGO_BIN + b"\nwait\n")
            body += VERIFY
            expected = [oracle.CARGO_OBJECT, oracle.CARGO_HELLO,
                        "linux: /media/DISK/bin/objprobe exited 0"]
            if phase != "cold":
                expected += ["Compiling message v0.1.0", "Compiling sh7-hello v0.1.0", "Finished `release` profile"]
            if phase == "build":
                expected += ["writeprobe: OK (0 failures)", "linux: /media/DISK/opt/rust/bin/cargo exited 0"]
            if phase == "restart":
                expected += ["fsd-crash-proof: final artifact absent before kill",
                             "fsd-crash-proof: observed " + ARMED.decode(),
                             "fsd-crash-proof: redoxfsd killed with the probe's handles open",
                             "redoxfsd: supervised instance 2 ready",
                             "wait: background job finished"]
            script = f"echo AXH {name} begins\n".encode() + body + f"echo AXH {name} ends\n".encode()
            case = axh.Case(name, script, tuple(expected), ready=0)
            case_work = work / name
            if case_work.exists():
                raise RuntimeError(f"evidence already exists: {case_work}; choose a fresh --output")
            result, text = redox.boot_with_disk(axh, case, case_work, qemu, firmware, args.timeout, disk, 6144)
            if "formatted the blank" in text or "fsd-crash-proof: FAIL" in text or "giving up" in text:
                raise RuntimeError(f"{name}: format, crash proof failure or give-up")
            restarts = text.count("redoxfsd: terminated; sessions invalidated")
            mounts = text.count("redoxfsd: mounted an existing RedoxFS volume")
            if restarts != int(phase == "restart") or mounts != 1 + int(phase == "restart"):
                raise RuntimeError(f"{name}: unexpected server mount/restart count")
            result["disk_stalls"] = [line for line in text.splitlines() if "virtio" in line and "stalled" in line]
            if phase == "restart":
                if text.count("persona-linux: filesystem session lost; reconnected over the mount control endpoint") < 1:
                    raise RuntimeError("the running Cargo tree did not reconnect")
                result["shim_reconnects"] = text.count("persona-linux: filesystem session lost;")
                kill = text.index("fsd-crash-proof: redoxfsd killed")
                finished = text.index("Finished `release` profile")
                at = 0
                for line in text.splitlines(keepends=True):
                    if "persona-linux: filesystem session lost;" in line and not kill < at < finished:
                        raise RuntimeError("shim reconnect was outside the running build")
                    at += len(line)
                ordered = ["bg: started linux", "fsd-crash-proof: observed", "redoxfsd killed",
                           "redoxfsd: supervised instance 2 ready",
                           "Finished `release` profile", oracle.CARGO_OBJECT, oracle.CARGO_HELLO]
            else:
                ordered = (["linux: /media/DISK/opt/rust/bin/cargo exited 0"] if phase == "build" else [])
                ordered += [oracle.CARGO_OBJECT, "linux: /media/DISK/bin/objprobe exited 0", oracle.CARGO_HELLO]
            offsets = [text.index(marker) for marker in ordered]
            if offsets != sorted(offsets):
                raise RuntimeError(f"{name}: build/recovery/verification out of order")
            manifest["cases"].append(result)
        manifest["passed"] = True
    except BaseException as error:
        manifest.update(passed=False, error=str(error))
        raise
    finally:
        with (work / "restore-default.log").open("wb") as log:
            restored = subprocess.run(axh.IMAGE_ARGS, cwd=ROOT, env=default_env, stdout=log, stderr=log)
        manifest["default_image_restored"] = restored.returncode == 0
        (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        if restored.returncode:
            raise RuntimeError(f"default image restore failed; inspect {work / 'restore-default.log'}")
    print("SH8-e RedoxFS Cargo smoke green", flush=True)


if __name__ == "__main__":
    main()
