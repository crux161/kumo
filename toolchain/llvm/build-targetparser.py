#!/usr/bin/env python3
#J950_r16_001
"""Build native LLVM TargetParser and close BinaryFormat's TargetParser dependency."""
from pathlib import Path
import runpy

if __name__ == "__main__":
    runpy.run_path(str(Path(__file__).with_name("build-support.py")))["main"]("targetparser")
