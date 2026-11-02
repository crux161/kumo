#!/usr/bin/env python3
#j735
"""G7k: prepare a pinned tool cache, then inventory both Mesa drivers offline."""
# Build-boundary and inventory decisions — SAKER, 2026-10-31.
import argparse
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parent.parent
CACHE = "kumo-mali-shaders"
IMAGE = "kumo-mesa-toolchain:g7k"
BASE = "debian@sha256:92d7265e51d67d3a2094c1aad9faa88c74cf9d52e6c354200e429cd60237cc55"
VERSION = "1:22.1.8-3"


def run(*command, **kwargs):
    print("+ " + " ".join(map(str, command)), flush=True)
    return subprocess.run(list(map(str, command)), cwd=ROOT, check=True, **kwargs)


def prepare():
    # The existing recipe owns the base image, signed Debian snapshot and Mesa pin.
    # It compares the existing shader assets; it never installs or updates an image.
    run("env", "KUMO_MALI_SHADERS_KEEP=1", "scripts/build-mali-shaders.sh")
    base = run("docker", "inspect", "--format", "{{.Config.Image}}", CACHE,
               capture_output=True, text=True).stdout.strip()
    assert base == BASE, "shader cache has a different base image"
    run("docker", "exec", CACHE, "sh", "-ec",
        "test \"$(cat /etc/apt/sources.list.d/snapshot.list | head -1)\" = "
        "'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/20261024T000000Z sid main'")
    run("docker", "exec", "-e", "DEBIAN_FRONTEND=noninteractive", CACHE,
        "apt-get", "-o", "Acquire::Retries=3", "install", "-y", "-q",
        *[f"{package}={VERSION}" for package in
          ("clang-22", "lld-22", "libc++-22-dev", "libc++abi-22-dev")])
    run("docker", "exec", CACHE, "ninja", "-C", "/work/build",
        "src/compiler/spirv/vtn_bindgen2")
    run("docker", "commit", CACHE, IMAGE)
    tar = ROOT / "build/mesa-native/mesa.tar.xz"
    tar.parent.mkdir(parents=True, exist_ok=True)
    run("docker", "cp", f"{CACHE}:/work/mesa_26.2.5.orig.tar.xz", tar)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prepare", action="store_true",
                        help="allow network ONLY to provision the pinned build-tool cache")
    parser.add_argument("--keep", action="store_true", help="keep the offline build container running")
    parser.add_argument("--jobs", type=int, default=8)
    args = parser.parse_args()
    if args.prepare:
        prepare()
    assert args.jobs > 0
    assert (ROOT / "build/mesa-native/mesa.tar.xz").is_file(), "use --prepare for the pinned tarball"
    assert (ROOT / "target/kumo-sysroot/lib/libc.a").is_file(), "run scripts/relibc-headers.sh first"
    out = ROOT / "build/mesa-drivers/result"
    out.mkdir(parents=True, exist_ok=True)
    image = json.loads(run("docker", "image", "inspect", IMAGE,
                           capture_output=True, text=True).stdout)[0]
    (out / "container.json").write_text(json.dumps({
        "image_id": image["Id"], "base": BASE, "network": "none",
        "snapshot": "20261024T000000Z",
    }, indent=2) + "\n")
    command = ["docker", "run", "--network=none", "--name=kumo-mesa-drivers-g7k",
               "--mount", f"type=bind,source={ROOT},target=/repo,readonly",
               "--mount", f"type=bind,source={out},target=/output"]
    if not args.keep:
        command.append("--rm")
        run(*command, IMAGE, "python3", "/repo/toolchain/mesa/build-drivers.py",
            "--jobs", str(args.jobs))
    else:
        run(*command, "-d", IMAGE, "sleep", "infinity")
        run("docker", "exec", "kumo-mesa-drivers-g7k", "python3",
            "/repo/toolchain/mesa/build-drivers.py", "--jobs", str(args.jobs))
    print(f"G7k driver inventory: {out / 'DRIVER-INVENTORY.md'}", flush=True)


if __name__ == "__main__":
    main()
