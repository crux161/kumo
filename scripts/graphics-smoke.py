#!/usr/bin/env python3
#j568
#j569
#j573
#j616
#j617
"""Boot a disposable copy of the full UEFI image and verify F1a two-view pixels via QMP.

No login credentials or changes to the built image are needed. By default only the copy's
autoexec is replaced. QEMU, its EDK2 firmware, and Python's standard library are the dependencies.

With --with-dtb, stage QEMU's own DTB to exercise device discovery from a child syscall.
That mode requires an image built with KUMO_CONSOLE_LOGLEVEL=6 (or 7) to assert drv-gpu's trace.
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
            data = b"dmesg -n 6\ngfx-client\ngfx-demo\ngfx-demo\necho graphics-smoke-after-present\ndmesg -n 5\n"
            found = True
        struct.pack_into("<QQ", out, entry + 64, len(out), len(data))
        out.extend(data)
    assert found, "image has no autoexec"
    path.write_bytes(out)


VIEW_W, VIEW_H = 256, 128
CHROME, GAP, BAND_H, MARGIN = 2, 16, 160, 8
BAND_BG = (0x16, 0x1B, 0x22)
VIEW_BG = (0, 0, 0)
CHROME_COLOR = (0x5A, 0x8F, 0xC8)
VIEW_SOLID = (0x00, 0xA0, 0xFF)
POISON = (0xFF, 0x00, 0xFF)
FG = (0, 0xFF, 0)


def view_rects(width, height):
    # Server-owned layout; must match kumo-graphics::band.
    content = 2 * (VIEW_W + 2 * CHROME) + GAP
    assert width >= content + 2 * MARGIN, f"screen {width}x{height} cannot hold the view band"
    assert height >= BAND_H + 8 * 16, f"screen {width}x{height} cannot hold a console"
    left = (width - content) // 2
    top = (BAND_H - (VIEW_H + 2 * CHROME)) // 2
    return [
        (left + index * (VIEW_W + 2 * CHROME + GAP) + CHROME, top + CHROME)
        for index in range(2)
    ]


def demo_pixel(x, y):
    if y < 32:
        return [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 255)][min(x // 64, 3)]
    blue = 0x40 if ((x // 16) + (y // 16)) % 2 == 0 else 0xC0
    return (x & 255, (y * 2) & 255, blue)


def check_pixels(path):
    data = path.read_bytes()
    header = re.match(rb"P6\s+(\d+)\s+(\d+)\s+255\n", data)
    assert header, "unexpected QEMU screenshot format"
    width, height = map(int, header.groups())
    pixels = data[header.end():]
    assert len(pixels) == width * height * 3
    rects = view_rects(width, height)

    def at(x, y):
        offset = (y * width + x) * 3
        return tuple(pixels[offset:offset + 3])

    assert at(0, 0) == BAND_BG, f"band background {at(0, 0)} != {BAND_BG}"
    for vx, vy in rects:
        for y in range(vy - CHROME, vy + VIEW_H + CHROME):
            for x in range(vx - CHROME, vx + VIEW_W + CHROME):
                border = x < vx or x >= vx + VIEW_W or y < vy or y >= vy + VIEW_H
                if border:
                    assert at(x, y) == CHROME_COLOR, f"chrome at {(x, y)} is {at(x, y)}"

    # View 0 is painted by the separate gfx-client process; view 1 by the supervisor demo,
    # which presents an over-sized surface whose visible corner is solid. Both are copied by
    # the server into server-owned rectangles.
    vx, vy = rects[0]
    for y in range(VIEW_H):
        for x in range(VIEW_W):
            assert at(vx + x, vy + y) == demo_pixel(x, y), f"view 0 pixel {(x, y)}"
    vx, vy = rects[1]
    for y in range(VIEW_H):
        for x in range(VIEW_W):
            assert at(vx + x, vy + y) == VIEW_SOLID, f"view 1 pixel {(x, y)}"

    # The clipped surface's out-of-view poison must never reach the glass.
    poison = sum(1 for y in range(height) for x in range(width) if at(x, y) == POISON)
    assert poison == 0, f"clipped surface leaked {poison} poison pixels"

    # The text console below the band still renders glyphs (echo, boot log, login prompt).
    text = sum(1 for y in range(BAND_H, height) for x in range(width) if at(x, y) == FG)
    assert text > 100, f"console region has only {text} foreground pixels"
    print(
        f"Graphics pixels green: two views at {rects} on {width}x{height}, "
        f"chrome intact, clip poison absent, console text {text}px"
    )


def main():
    root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", type=Path, default=root / "build/images/qemu-virt-aarch64")
    parser.add_argument("--firmware-code", type=Path, default=Path("/opt/homebrew/share/qemu/edk2-aarch64-code.fd"))
    parser.add_argument("--firmware-vars", type=Path, default=Path("/opt/homebrew/share/qemu/edk2-arm-vars.fd"))
    parser.add_argument("--timeout", type=float, default=90)
    parser.add_argument("--with-dtb", action="store_true", help="exercise drv-gpu with a staged QEMU DTB; requires image log level >=6")
    args = parser.parse_args()
    work = Path(tempfile.mkdtemp(prefix="kumo-gfx-"))
    print(f"Graphics smoke artifacts: {work}", flush=True)
    shutil.copytree(args.image, work / "esp")
    if args.with_dtb:
        # Nijigumo currently searches these fixed candidate paths. Use one only in the
        # disposable ESP: the bytes describe QEMU, and the manifest's board stays QEMU. — KESTREL
        dtb = work / "esp/EFI/KUMO/dtb/qcom/sc8280xp-lenovo-thinkpad-x13s.dtb"
        dtb.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run([
            "qemu-system-aarch64", "-machine", f"virt,gic-version=3,dumpdtb={dtb}",
            "-cpu", "cortex-a72", "-m", "512", "-display", "none", "-device", "ramfb",
            "-net", "none",
        ], check=True, capture_output=True, timeout=15)
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
                if args.with_dtb and b"TOWER: CPU EXCEPTION" in transcript:
                    raise RuntimeError(f"kernel fault during DTB syscall smoke; inspect {serial}")
                if b"INDRA login:" in transcript and b"graphics-smoke-after-present" in transcript:
                    break
                time.sleep(0.1)
            else:
                raise RuntimeError(f"boot timed out; inspect {serial}")
            client = b"gfx-client: 256x128 surface copied; buffer released (software GOP)"
            assert b"private RAM, read-only walk, VMO denial, DMA gap, reuse OK" in transcript, f"missing kernel device-table RAM proof; inspect {serial}"
            assert b"VMO identity, closed handle, exact unmap, offline release OK" in transcript, f"missing kernel VMO pin proof; inspect {serial}"
            print("Device VMO pin smoke green: identity, closed handle, exact unmap, offline release")
            assert b"sora: VmoCreate SIMD preserved" in transcript, f"missing EL0 vector proof; inspect {serial}"
            print("Device table RAM smoke green: private pages, read-only walk, VMO alias denial, DMA gap, reuse, EL0 SIMD")
            assert transcript.count(client) == 1, f"missing client present; inspect {serial}"
            demo = b"gfx-demo: view 1 copied; buffer released (software GOP)"
            assert transcript.count(demo) == 2, f"missing view 1 replies; inspect {serial}"
            assert b"gfx-demo: one or more views failed" not in transcript
            assert b"gfx-client: presenter rejected surface" not in transcript
            if args.with_dtb:
                assert b" declared / " in transcript, f"staged DTB was not loaded; inspect {serial}"
                marker = b"drv-gpu: DevicePowerUp NotSupported; aperture untouched"
                assert transcript.count(marker) == 1, f"missing drv-gpu trace (image log level >=6 required); inspect {serial}"
                assert b"drv-gpu: probe failed" not in transcript
                print("DTB syscall smoke green: drv-gpu rejected absent GPU with a nonzero DTB; login reached")
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
            print("Graphics UEFI smoke green: separate client + supervisor views, clipping, chrome, reuse, console text, login")
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


if __name__ == "__main__":
    main()
