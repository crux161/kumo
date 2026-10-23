#!/usr/bin/env python3
#j567
"""Boot a disposable copy of the full UEFI image and verify F0 pixels via QMP.

No login credentials or changes to the built image are needed: only the copy's autoexec
is replaced. QEMU, its EDK2 firmware, and Python's standard library are the dependencies.
"""

import argparse
import json
from pathlib import Path
import re
import shutil
import select
import struct
import subprocess
import tempfile
import time


def replace_autoexec(path):
    original = path.read_bytes()
    assert original[:8] == b"KUMORD01" and struct.unpack_from("<I", original, 8)[0] == 1
    count = struct.unpack_from("<I", original, 12)[0]
    out = bytearray(original[:16 + 80 * count])
    found = False
    for index in range(count):
        entry = 16 + index * 80
        name = original[entry:entry + 64].split(b"\0", 1)[0]
        offset, size = struct.unpack_from("<QQ", original, entry + 64)
        data = original[offset:offset + size]
        if name == b"etc/autoexec":
            data = b"dmesg -n 6\ngfx-demo\ngfx-demo\necho graphics-smoke-after-present\ndmesg -n 5\n"
            found = True
        struct.pack_into("<QQ", out, entry + 64, len(out), len(data))
        out.extend(data)
    assert found, "image has no autoexec"
    path.write_bytes(out)


def check_pixels(path):
    data = path.read_bytes()
    header = re.match(rb"P6\s+(\d+)\s+(\d+)\s+255\n", data)
    assert header, "unexpected QEMU screenshot format"
    width, height = map(int, header.groups())
    pixels = data[header.end():]
    assert len(pixels) == width * height * 3
    bars = b"".join(bytes(rgb) * 64 for rgb in [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 255)])
    images = []
    for y in range(height - 127):
        # Server owns placement: F0 inserts at column zero of the console.
        if pixels[y * width * 3:y * width * 3 + len(bars)] != bars:
            continue
        expected = [bars] * 32
        for row in range(32, 128):
            expected.append(b"".join(bytes((x, (row * 2) & 255, 64 if (x // 16 + row // 16) % 2 == 0 else 192)) for x in range(256)))
        if all(pixels[(y + row) * width * 3:(y + row) * width * 3 + 768] == expected[row] for row in range(128)):
            images.append(y)
    assert len(images) >= 2, f"expected two complete surface copies; found {images} on {width}x{height}"
    print(f"Graphics pixels green: two complete 256x128 images at y={images}; screen {width}x{height}")


def main():
    root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", type=Path, default=root / "build/images/qemu-virt-aarch64")
    parser.add_argument("--firmware-code", type=Path, default=Path("/opt/homebrew/share/qemu/edk2-aarch64-code.fd"))
    parser.add_argument("--firmware-vars", type=Path, default=Path("/opt/homebrew/share/qemu/edk2-arm-vars.fd"))
    parser.add_argument("--timeout", type=float, default=90)
    args = parser.parse_args()
    work = Path(tempfile.mkdtemp(prefix="kumo-gfx-"))
    print(f"Graphics smoke artifacts: {work}", flush=True)
    shutil.copytree(args.image, work / "esp")
    replace_autoexec(work / "esp/EFI/KUMO/initrd.img")
    shutil.copyfile(args.firmware_vars, work / "vars.fd")
    serial = work / "serial.log"
    with (work / "qemu.log").open("wb") as log:
        process = subprocess.Popen([
            "qemu-system-aarch64", "-machine", "virt,gic-version=3", "-cpu", "cortex-a72",
            "-m", "512", "-display", "none", "-device", "ramfb", "-monitor", "none",
            "-serial", f"file:{serial}", "-qmp", "stdio",
            "-drive", f"if=pflash,format=raw,file={args.firmware_code},readonly=on",
            "-drive", f"if=pflash,format=raw,file={work / 'vars.fd'}",
            "-drive", f"file=fat:rw:{work / 'esp'},format=raw,if=virtio",
            "-net", "none",
        ], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, bufsize=0)
        try:
            deadline = time.monotonic() + args.timeout
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise RuntimeError((work / "qemu.log").read_text())
                transcript = serial.read_bytes() if serial.exists() else b""
                if b"INDRA login:" in transcript and b"graphics-smoke-after-present" in transcript:
                    break
                time.sleep(0.1)
            else:
                raise RuntimeError(f"boot timed out; inspect {serial}")
            marker = b"gfx-demo: 256x128 surface copied; buffer released (software GOP)"
            assert transcript.count(marker) == 2, f"missing release replies; inspect {serial}"
            assert b"gfx-demo: presentation failed" not in transcript
            time.sleep(0.2)  # let the final queued console line reach drv-fb
            def read_qmp():
                if not select.select([process.stdout], [], [], 5)[0]:
                    raise RuntimeError("QMP response timed out")
                return json.loads(process.stdout.readline())

            read_qmp()  # greeting

            def command(name, arguments=None):
                request = {"execute": name}
                if arguments:
                    request["arguments"] = arguments
                process.stdin.write(json.dumps(request).encode() + b"\n")
                while True:
                    response = read_qmp()
                    if "error" in response:
                        raise RuntimeError(response)
                    if "return" in response:
                        return response["return"]

            command("qmp_capabilities")
            command("screendump", {"filename": str(work / "screen.ppm")})
            check_pixels(work / "screen.ppm")
            print("Graphics UEFI smoke green: two acknowledged presents, buffer reuse, subsequent console output, login")
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


if __name__ == "__main__":
    main()
