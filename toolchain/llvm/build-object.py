#!/usr/bin/env python3
#J951_r17_001
"""Build LLVM Object's complete source list and its native ELF/archive reader proof."""
from pathlib import Path
import runpy

if __name__ == "__main__":
    runpy.run_path(str(Path(__file__).with_name("build-support.py")))["main"]("object")
