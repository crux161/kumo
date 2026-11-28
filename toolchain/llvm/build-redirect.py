#!/usr/bin/env python3
#J956_r22_001
"""Build LLVM Support and the PROPOSAL/027 P2c spawn-redirect proof (redirect-smoke.cpp)."""
from pathlib import Path
import runpy

if __name__ == "__main__":
    runpy.run_path(str(Path(__file__).with_name("build-support.py")))["main"]("redirect")
