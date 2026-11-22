#J935_r01_001
"""Regression: a proof must boot its patched initrd, including on zstd-enabled ESPs."""
from pathlib import Path
import runpy
import struct
import tempfile
import unittest

stage_image = runpy.run_path(str(Path(__file__).with_name("cxx-smoke.py")))["stage_image"]


class StageImageTest(unittest.TestCase):
    def test_replaces_payload_and_invalidates_compressed_copy(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            boot = source / "EFI/KUMO"
            boot.mkdir(parents=True)
            original = bytearray(b"KUMORD01" + struct.pack("<II", 1, 2) + bytes(160))
            for i, name in enumerate((b"etc/autoexec", b"bin/relibc-smoke")):
                entry = 16 + i * 80
                original[entry:entry + len(name)] = name
                struct.pack_into("<QQ", original, entry + 64, len(original), 3)
                original.extend(b"old")
            (boot / "initrd.img").write_bytes(original)
            (boot / "initrd.img.zst").write_bytes(b"stale compressed payload")
            conf = ("board = qemu-virt-aarch64\n"
                    "initrd = \\EFI\\KUMO\\initrd.img\n"
                    "initrd_zstd = old\ninitrd_zstd_sha256 = old\ninitrd_size = 3\n")
            (boot / "nijigumo.conf").write_text(conf)
            elf = root / "proof"
            elf.write_bytes(b"new executable")
            destination = root / "destination"
            stage_image(source, destination, elf, b"run proof\n")
            copied = destination / "EFI/KUMO"
            self.assertFalse((copied / "initrd.img.zst").exists())
            self.assertEqual((copied / "nijigumo.conf").read_text(),
                             "board = qemu-virt-aarch64\ninitrd = \\EFI\\KUMO\\initrd.img\n")
            data = (copied / "initrd.img").read_bytes()
            for i, expected in enumerate((b"run proof\n", b"new executable")):
                offset, size = struct.unpack_from("<QQ", data, 16 + i * 80 + 64)
                self.assertEqual(data[offset:offset + size], expected)
            self.assertEqual((boot / "initrd.img").read_bytes(), original)
            self.assertEqual((boot / "nijigumo.conf").read_text(), conf)
            self.assertTrue((boot / "initrd.img.zst").exists())


if __name__ == "__main__":
    unittest.main()
