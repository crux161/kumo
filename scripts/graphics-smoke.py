#!/usr/bin/env python3
#j648
#j649
#j654
#j655
#j656
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


def replace_autoexec(path, bitmap=False, bitmap_path=None, bitmap_file=None, two_clients=False, damage=False, zorder=False):
    original = path.read_bytes()
    assert original[:8] == b"KUMORD01" and struct.unpack_from("<I", original, 8)[0] == 1
    count = struct.unpack_from("<I", original, 12)[0]
    files = []
    for index in range(count):
        entry = 16 + index * 80
        name = original[entry:entry + 64].split(b"\0", 1)[0]
        offset, size = struct.unpack_from("<QQ", original, entry + 64)
        files.append((name, original[offset:offset + size]))
    if bitmap_file:
        files.append((b"share/images/input.bmp", bitmap_file.read_bytes()))
    if bitmap_path:
        files.append((b"share/images/empty.bmp", b""))
    out = bytearray(16 + 80 * len(files))
    out[:16] = original[:16]
    struct.pack_into("<I", out, 12, len(files))
    found = False
    for index, (name, data) in enumerate(files):
        if name == b"etc/autoexec":
            data = b"dmesg -n 6\ngfx-client\ngfx-demo\ngfx-demo\necho graphics-smoke-after-present\ndmesg -n 5\n"
            if two_clients:
                data = b"dmesg -n 6\ngfx-clients\necho graphics-smoke-after-present\ndmesg -n 5\n"
            if damage:
                data = b"dmesg -n 6\ngfx-damage\necho graphics-smoke-after-present\ndmesg -n 5\n"
            if zorder:
                # Paint both views first, then repaint the lower one: if the server did not
                # compose in its order, the lower copy would erase the overlap.
                data = b"dmesg -n 6\ngfx-demo\ngfx-client\necho graphics-smoke-after-present\ndmesg -n 5\n"
            if bitmap:
                command = b"bmp-view"
                if bitmap_path:
                    # Refusals must leave the grant available for the successful launch. — SAKER
                    command = (b"bmp-view /x\nbmp-view /etc/../etc/shadow\n"
                               b"bmp-view x y\nbmp-view /media/x/i\n"
                               b"bmp-view /share/images/empty.bmp\n"
                               b"cd /share/images\nbmp-view " + bitmap_path.encode())
                data = data.replace(b"gfx-client", command)
                if bitmap_path:
                    data = data.replace(b"graphics-smoke-after-present", b"g-ok")
            assert len(data) <= 256, "autoexec exceeds its finite startup buffer"
            found = True
        if name == b"bin/fat32.img" and bitmap_path == "/tmp/INPUT.BMP":
            # Put a 3x2 RGB fixture into one unused cluster of the disposable FAT image.
            data = bytearray(data)
            fixture = bytearray(78)
            fixture[:2] = b"BM"
            struct.pack_into("<I", fixture, 2, 78)
            struct.pack_into("<I", fixture, 10, 54)
            struct.pack_into("<IiiHH", fixture, 14, 40, 3, 2, 1, 24)
            fixture[54:] = bytes([255,255,255, 0,0,0, 0,255,255, 0,0,0,
                                  0,0,255, 0,255,0, 255,0,0, 0,0,0])
            bps = struct.unpack_from("<H", data, 11)[0]
            reserved = struct.unpack_from("<H", data, 14)[0]
            fats = data[16]
            spf = struct.unpack_from("<I", data, 36)[0]
            start = (reserved + fats * spf) * bps
            for fat in range(fats):
                struct.pack_into("<I", data, (reserved + fat * spf) * bps + 7 * 4, 0x0FFFFFFF)
            entry = start + 128
            data[entry:entry+11] = b"INPUT   BMP"
            data[entry+11] = 0x20
            struct.pack_into("<H", data, entry+26, 7)
            struct.pack_into("<I", data, entry+28, len(fixture))
            data[start+5*bps:start+5*bps+len(fixture)] = fixture
        entry = 16 + index * 80
        out[entry:entry+len(name)] = name
        struct.pack_into("<QQ", out, entry+64, len(out), len(data))
        out.extend(data)
    assert found, "image has no autoexec"
    path.write_bytes(out)


VIEW_W, VIEW_H = 256, 128
VIEW_OVERLAP = 64
CHROME, BAND_H, MARGIN = 2, 160, 8
BAND_BG = (0x16, 0x1B, 0x22)
VIEW_BG = (0, 0, 0)
CHROME_COLOR = (0x5A, 0x8F, 0xC8)
VIEW_SOLID = (0x00, 0xA0, 0xFF)
POISON = (0xFF, 0x00, 0xFF)
FG = (0, 0xFF, 0)


def view_rects(width, height):
    # Server-owned overlapping layout; must match kumo-graphics::band.
    content = VIEW_W + (VIEW_W - VIEW_OVERLAP) + 2 * CHROME
    assert width >= content + 2 * MARGIN, f"screen {width}x{height} cannot hold the view band"
    assert height >= BAND_H + 8 * 16, f"screen {width}x{height} cannot hold a console"
    left = (width - content) // 2
    top = (BAND_H - (VIEW_H + 2 * CHROME)) // 2
    first = left + CHROME
    return [(first, top + CHROME), (first + VIEW_W - VIEW_OVERLAP, top + CHROME)]


def compose_expected(x, y, rects, images):
    # Server order [0, 1]: the later view is above the earlier one. Each covering view
    # draws its chrome, its client content (or well), or nothing.
    color = BAND_BG
    for index in (0, 1):
        vx, vy = rects[index]
        if not (vx - CHROME <= x < vx + VIEW_W + CHROME and vy - CHROME <= y < vy + VIEW_H + CHROME):
            continue
        if vx <= x < vx + VIEW_W and vy <= y < vy + VIEW_H:
            image = images[index]
            color = image(x - vx, y - vy) if image else VIEW_BG
        else:
            color = CHROME_COLOR
    return color


def demo_pixel(x, y):
    if y < 32:
        return [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 255)][min(x // 64, 3)]
    blue = 0x40 if ((x // 16) + (y // 16)) % 2 == 0 else 0xC0
    return (x & 255, (y * 2) & 255, blue)


DAMAGE_X, DAMAGE_Y, DAMAGE_W, DAMAGE_H = 64, 32, 64, 64
DAMAGE_COLOR = (0xFF, 0x80, 0x00)


def damage_demo_pixel(x, y):
    if DAMAGE_X <= x < DAMAGE_X + DAMAGE_W and DAMAGE_Y <= y < DAMAGE_Y + DAMAGE_H:
        return DAMAGE_COLOR
    return demo_pixel(x, y)


def bitmap_reference(source=None, scratch=False):
    if scratch:
        colors = [[(255,0,0), (0,255,0), (0,0,255)],
                  [(255,255,255), (0,0,0), (255,255,0)]]
        return lambda x,y: (colors[y*2//128][(x-32)*3//192] if 32 <= x < 224 else (0,0,0))
    sample = source or Path(__file__).resolve().parent.parent / "userland/bmp-view/assets/sample.bmp"
    data = sample.read_bytes()
    offset = struct.unpack_from("<I", data, 10)[0]
    width, height = struct.unpack_from("<ii", data, 18)
    bpp = struct.unpack_from("<H", data, 28)[0]
    compression = struct.unpack_from("<I", data, 30)[0]
    assert data[:2] == b"BM" and bpp in (24, 32) and compression in (0, 3)
    if compression == 3:
        assert struct.unpack_from("<III", data, 54) == (0xFF0000, 0xFF00, 0xFF)
    top_down, height = height < 0, abs(height)
    dw, dh = (256, max(1, height*256//width)) if width*128 > height*256 else (max(1,width*128//height),128)
    left, top = (256-dw)//2, (128-dh)//2

    def pixel(x, y):
        if not left <= x < left+dw or not top <= y < top+dh:
            return (0, 0, 0)
        sx, sy = (x-left)*width//dw, (y-top)*height//dh
        if not top_down:
            sy = height-1-sy
        index = offset + sy*((width*(bpp//8)+3)//4*4) + sx*(bpp//8)
        b, g, r = data[index:index+3]
        return (r, g, b)

    return pixel


def check_pixels(path, bitmap=False, bitmap_file=None, scratch=False, two_clients=False, damage=False):
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

    # The band is a composed server surface: background, then each view's chrome, well and
    # client content in the server's back-to-front order. Checking every band pixel against
    # the same model proves the overlap and z-order, not only the two wells.
    if damage:
        images = [damage_demo_pixel, None]
    elif bitmap:
        images = [bitmap_reference(bitmap_file, scratch), lambda x, y: VIEW_SOLID]
    elif two_clients:
        images = [demo_pixel, demo_pixel]
    else:
        images = [demo_pixel, lambda x, y: VIEW_SOLID]
    for y in range(BAND_H):
        for x in range(width):
            want = compose_expected(x, y, rects, images)
            assert at(x, y) == want, f"band pixel {(x, y)} is {at(x, y)}, want {want}"

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
    parser.add_argument("--bitmap", action="store_true", help="display and verify bmp-view instead of gfx-client")
    parser.add_argument("--two-clients", action="store_true", help="run two concurrent separate gfx-client processes, one per view")
    parser.add_argument("--damage", action="store_true", help="prove damage tracking with the supervisor client's full-then-partial present")
    parser.add_argument("--zorder", action="store_true", help="prove server z-order: the lower client presents after the upper view")
    parser.add_argument("--bitmap-path", nargs="?", const="sample.bmp", help="exercise a target file path, relative to /share/images")
    parser.add_argument("--bitmap-file", type=Path, help="inject a host BMP at /share/images/input.bmp and verify it")
    args = parser.parse_args()
    if args.bitmap_file:
        args.bitmap_path = "input.bmp"
    if args.bitmap_path:
        args.bitmap = True
    if args.two_clients and args.bitmap:
        parser.error("--two-clients and --bitmap are separate proofs")
    if args.damage and (args.bitmap or args.two_clients):
        parser.error("--damage is a separate proof")
    if args.zorder and (args.bitmap or args.two_clients or args.damage):
        parser.error("--zorder is a separate proof")
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
    replace_autoexec(work / "esp/EFI/KUMO/initrd.img", args.bitmap, args.bitmap_path, args.bitmap_file,
                     args.two_clients, args.damage, args.zorder)
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
                ready = b"INDRA login:" in transcript and (
                    b"g-ok" if args.bitmap_path else b"graphics-smoke-after-present"
                ) in transcript
                if args.two_clients:
                    # The clients are admitted ASYNC after the marker; wait for both
                    # separate processes to present before the screenshot.
                    ready = ready and transcript.count(
                        b"gfx-client: 256x128 surface copied; buffer released (software GOP)"
                    ) >= 2
                if args.damage:
                    ready = ready and b"gfx-damage: full then partial damage copied" in transcript
                if ready:
                    break
                time.sleep(0.1)
            else:
                raise RuntimeError(f"boot timed out; inspect {serial}")
            client = b"gfx-client: 256x128 surface copied; buffer released (software GOP)"
            if args.bitmap_path:
                for marker in [b"bmp-view: not found", b"bmp-view: permission denied", b"usage: bmp-view [path]",
                               b"bmp-view: no such volume", b"bmp-view: file must be 1..16777216 bytes"]:
                    assert marker in transcript, f"missing refusal {marker!r}; inspect {serial}"
            if args.bitmap:
                client = b"bmp-view: bitmap copied; buffer released (software GOP)"
            assert b"private RAM, read-only walk, VMO denial, DMA gap, reuse OK" in transcript, f"missing kernel device-table RAM proof; inspect {serial}"
            assert b"VMO identity, closed handle, exact unmap, offline release OK" in transcript, f"missing kernel VMO pin proof; inspect {serial}"
            print("Device VMO pin smoke green: identity, closed handle, exact unmap, offline release")
            assert b"duplicate, queued transfer, process drain, final drop OK" in transcript, f"missing DeviceCtx capability reference proof; inspect {serial}"
            print("DeviceCtx reference smoke green: duplicate, queued transfer, process drain, final drop")
            assert b"checked refusal, final close, backing retained, unit reserved OK" in transcript, f"missing checked context retirement proof; inspect {serial}"
            print("DeviceCtx retirement smoke green: checked refusal, final close, backing retained, unit reserved")
            assert b"engine replacement, roots/backing held, alias denial, pre-zero gap OK" in transcript, f"missing kernel retention proof; inspect {serial}"
            print("DeviceCtx retention smoke green: engine replacement, roots/backing held, alias denial, pre-zero gap")
            assert b"sora: VmoCreate SIMD preserved" in transcript, f"missing EL0 vector proof; inspect {serial}"
            print("Device table RAM smoke green: private pages, read-only walk, VMO alias denial, DMA gap, reuse, EL0 SIMD")
            if args.two_clients:
                assert transcript.count(client) == 2, f"missing two client presents; inspect {serial}"
                for view in (0, 1):
                    marker = f"gfx-clients: view {view} handed to an async client process".encode()
                    assert marker in transcript, f"missing view {view} handoff; inspect {serial}"
                assert b"gfx-demo:" not in transcript, f"supervisor painted a view; inspect {serial}"
            elif args.damage:
                assert b"gfx-damage: full then partial damage copied" in transcript, f"missing damage proof; inspect {serial}"
                assert b"gfx-damage: damage proof failed" not in transcript
                assert b"gfx-demo:" not in transcript
            elif args.zorder:
                demo0 = b"gfx-demo: view 0 copied; buffer released (software GOP)"
                demo1 = b"gfx-demo: view 1 copied; buffer released (software GOP)"
                assert transcript.count(demo0) == 1, f"missing view 0 demo; inspect {serial}"
                assert transcript.count(demo1) == 1, f"missing view 1 demo; inspect {serial}"
                assert transcript.count(client) == 1, f"missing lower client present; inspect {serial}"
                assert transcript.index(demo1) < transcript.index(client), f"lower client did not present last; inspect {serial}"
                assert b"gfx-demo: one or more views failed" not in transcript
            else:
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
            check_pixels(work / "screen.ppm", args.bitmap, args.bitmap_file,
                         args.bitmap_path == "/tmp/INPUT.BMP", args.two_clients, args.damage)
            if args.bitmap:
                print("Bitmap UEFI smoke green: all 32768 pixels match the BMP, letterboxing, chrome, console, login")
            if args.two_clients:
                print("Two-client UEFI smoke green: two separate client processes, one per view, chrome, console, login")
            if args.damage:
                print("Damage UEFI smoke green: full then partial copy, undeclared surface poison never reached the glass")
            if args.zorder:
                print("Z-order UEFI smoke green: lower client presented after the upper view; overlap kept the upper content")
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
