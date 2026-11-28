#!/usr/bin/env python3
#J952_r18_001
"""Build LLVM MC and DWARF low-level with a complete native link and generic API proof."""
from pathlib import Path
import runpy

if __name__ == "__main__":
    runpy.run_path(str(Path(__file__).with_name("build-support.py")))["main"]("mc")
