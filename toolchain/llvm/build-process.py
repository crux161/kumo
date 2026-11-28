#!/usr/bin/env python3
#J953_r19_001
"""Build LLVM Support with posix_spawn on and its native process-launch proof (SH9-p1)."""
from pathlib import Path
import runpy

if __name__ == "__main__":
    runpy.run_path(str(Path(__file__).with_name("build-support.py")))["main"]("process")
