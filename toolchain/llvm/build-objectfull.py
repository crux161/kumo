#!/usr/bin/env python3
#J960_r26_001
"""Build and prove the complete native LLVM Object component set (SH9-d8)."""
from pathlib import Path
import runpy

if __name__ == "__main__":
    runpy.run_path(str(Path(__file__).with_name("build-support.py")))["main"]("objectfull")
