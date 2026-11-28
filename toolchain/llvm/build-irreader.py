#!/usr/bin/env python3
#J959_r25_001
"""Build native LLVM IR readers and upstream llvm-dis (SH9-d7)."""
from pathlib import Path
import runpy

if __name__ == "__main__":
    runpy.run_path(str(Path(__file__).with_name("build-support.py")))["main"]("irreader")
