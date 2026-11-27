#!/usr/bin/env python3
#J949_r15_001
"""Build every pinned LLVM BinaryFormat unit and its native KUMO proof."""
from pathlib import Path
import runpy

if __name__ == "__main__":
    runpy.run_path(str(Path(__file__).with_name("build-support.py")))["main"]("binaryformat")
