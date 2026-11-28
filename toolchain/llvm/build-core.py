#!/usr/bin/env python3
#J958_r24_001
"""Build LLVM Core and its complete native component dependencies (SH9-d6)."""
from pathlib import Path
import runpy

if __name__ == "__main__":
    runpy.run_path(str(Path(__file__).with_name("build-support.py")))["main"]("core")
