#!/usr/bin/env python3
#j918
#j919
#j920
#j922
#j924
"""SH7-b/c: one QEMU boot proving the in-process persona shim (PLAN.md §10 R1), with a static
program (SH7-b) and a dynamically linked musl program run through musl's loader (SH7-c).

Reuses the AXH harness's own boot machinery (import only). Sora's boot runs the M10 aarch64
Linux ELF inside `persona-linux-shim`: the shim loads the program into its own address space,
the kernel redirects the program's `svc`s to the shim (no Linux number in the kernel), and the
program's `write` arrives as an INFO record. The case requires the program's line and Sora's
`M10-c Check ok` exactly once each, and no TOWER. The image is rebuilt with the proof loglevel,
then the default image is restored.

--rustc boots the opt-in KUMO_PERSONA_RUSTC=1 image with 4 GiB of QEMU memory. It runs the
pinned Linux compiler's -Vv, compiles /w/add.rs for aarch64-unknown-none and checks the hash
of /w/add.o in a second process. Run scripts/persona-rustc.sh to stage the compiler and sysroot.
"""

import argparse
import importlib.util
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

import persona_rustc_proof

ROOT = Path(__file__).resolve().parent.parent


def load_axh():
    spec = importlib.util.spec_from_file_location("axh", ROOT / "scripts/netstack-smoke.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "build/verification/sh7/sh7b")
    parser.add_argument("--timeout", type=float, help="seconds per boot (default 90, or 900 with --rustc)")
    parser.add_argument("--rustc", action="store_true", help="compile and verify /w/add.o with the opt-in rustc image")
    parser.add_argument(
        "--link",
        action="store_true",
        help="SH7-f: rustc links /w/hello for aarch64-unknown-kumo through rust-lld, then it runs",
    )
    args = parser.parse_args()

    axh = load_axh()
    qemu = shutil.which("qemu-system-aarch64")
    if qemu is None:
        sys.exit("qemu-system-aarch64 missing")
    firmware = Path(qemu).resolve().parent.parent / "share/qemu"

    source = ROOT / "build/images/qemu-virt-aarch64"
    work = args.output
    work.mkdir(parents=True, exist_ok=True)

    case = axh.Case(
        "persona",
        # SH7-d2 (j890): every Linux program runs through the shell's `linux` word, which
        # grants the shim a filesystem; the shim loads the program and its interpreter by path.
        b"echo AXH persona begins\n"
        b"run asidpair snapshot\n"
        b"linux /bin/m10-hello\n"
        b"linux /bin/hello-dyn\n"
        b"linux /bin/vmprobe\n"
        b"linux /bin/greet\n"
        b"linux /bin/fileprobe\n"
        b"linux /bin/writeprobe\n"
        b"linux /bin/pipeprobe\n"
        b"linux /bin/threadprobe\n"
        b"linux /bin/sigprobe\n"
        b"linux /bin/spawnprobe\n"
        b"linux /bin/bigread\n"
        b"echo AXH persona ends\n",
        (
            "asidpair: PASS snapshot private bytes, child aliases, explicit handles",
            "M10 elf linux hi",
            "linux: /bin/m10-hello exited 0",
            "SH7-c dynamic musl hello: argc=1 argv0=/bin/hello-dyn",
            "linux: /bin/hello-dyn exited 0",
            "vmprobe: OK (0 failures)",
            "linux: /bin/vmprobe exited 0",
            "greet: hello from libgreet.so",
            "linux: /bin/greet exited 0",
            "fileprobe: OK (0 failures)",
            "linux: /bin/fileprobe exited 0",
            "writeprobe: create, 5003-byte write and read-back OK",
            "writeprobe: pwrite, ftruncate, append, O_TRUNC and O_EXCL OK",
            "writeprobe: rename, cwd, dir fds, unlink and rmdir OK",
            "writeprobe: OK (0 failures)",
            "linux: /bin/writeprobe exited 0",
            "pipeprobe: flags, errors and empty reads OK",
            "pipeprobe: 5003 exact bytes, short-read tails and EOF OK",
            "pipeprobe: EPIPE, 128 reuse cycles and EMFILE rollback OK",
            "pipeprobe: OK (0 failures)",
            "linux: /bin/pipeprobe exited 0",
            "threadprobe: create and join, tid != pid OK",
            "threadprobe: 4 threads x 10000 locked increments = 40000 OK",
            "threadprobe: a parked pipe reader woke on a sibling's write OK",
            "threadprobe: detached thread and timed wait OK",
            "threadprobe: OK (0 failures)",
            "linux: /bin/threadprobe exited 0",
            "sigprobe: raise, siginfo and ucontext OK",
            "sigprobe: masks, sigpending and SIG_IGN OK",
            "sigprobe: altstack and nested frames OK",
            "sigprobe: EINTR and SA_RESTART on a parked pipe read OK",
            "sigprobe: futex-parked handler and interrupted sleep OK",
            "sigprobe: OK (0 failures)",
            "linux: /bin/sigprobe exited 0",
            "spawnprobe: OK (0 failures)",
            "linux: /bin/spawnprobe exited 0",
            "bigread: OK (0 failures)",
            "linux: /bin/bigread exited 0",
        ),
        ready=0,
    )

    proof_env = dict(os.environ, KUMO_CONSOLE_LOGLEVEL="6")
    memory_mib = 2048
    if args.rustc:
        case = axh.Case(
            "persona-rustc",
            b"echo AXH persona-rustc begins\n" + persona_rustc_proof.BOOT + b"echo AXH persona-rustc ends\n",
            (*persona_rustc_proof.VERSION, persona_rustc_proof.OBJECT, "linux: /bin/objprobe exited 0"),
            ready=0,
        )
        proof_env.update(KUMO_PERSONA_RUSTC="1", KUMO_INITRD_ZSTD="0")
        memory_mib = 4096
    if args.link:
        case = axh.Case(
            "persona-link",
            b"echo AXH persona-link begins\n" + persona_rustc_proof.LINK_BOOT + b"echo AXH persona-link ends\n",
            (persona_rustc_proof.LINKED, persona_rustc_proof.HELLO, "linux: /bin/objprobe exited 0"),
            ready=0,
        )
        proof_env.update(KUMO_PERSONA_RUSTC="1", KUMO_PERSONA_LINK="1", KUMO_INITRD_ZSTD="0")
        memory_mib = 5120
    default_env = {k: v for k, v in os.environ.items() if k not in axh.CONTROLS}
    manifest = {"head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip()}
    try:
        with (work / "build-proof.log").open("wb") as log:
            subprocess.run(axh.IMAGE_ARGS, cwd=ROOT, env=proof_env, stdout=log, stderr=log, check=True)
        # boot() insists on a fresh case directory; a previous run's evidence must not
        # block or pollute this one.
        shutil.rmtree(work / case.name, ignore_errors=True)
        manifest["case"] = axh.boot(
            ROOT, source, case, work / case.name, qemu, firmware,
            args.timeout if args.timeout is not None else (900 if args.rustc or args.link else 90), memory_mib
        )
        if args.link:
            persona_rustc_proof.validate_link((work / case.name / "serial.log").read_text(errors="replace"))
        if args.rustc:
            persona_rustc_proof.validate((work / case.name / "serial.log").read_text(errors="replace"))
        manifest["passed"] = True
    except BaseException as error:
        manifest["passed"] = False
        manifest["error"] = str(error)
        raise
    finally:
        with (work / "restore-default.log").open("wb") as log:
            subprocess.run(axh.IMAGE_ARGS, cwd=ROOT, env=default_env, stdout=log, stderr=log, check=True)
        (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print("SH7 persona smoke green", flush=True)


if __name__ == "__main__":
    main()
