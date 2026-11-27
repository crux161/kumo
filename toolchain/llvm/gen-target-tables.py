#!/usr/bin/env python3
#J950_r16_001
"""Generate TargetParser's four upstream tables with the pinned LLVM TableGen version."""
import hashlib
import os
from pathlib import Path
import re
import shutil
import subprocess

TABLES = (
    ("ARM", "ARM", "arm-target-def", "ARMTargetParserDef.inc"),
    ("AArch64", "AArch64", "arm-target-def", "AArch64TargetParserDef.inc"),
    ("RISCV", "RISCV", "riscv-target-def", "RISCVTargetParserDef.inc"),
    ("PowerPC", "PPC", "target-features", "PPCGenTargetFeatures.inc"),
)


def generate(source, generated, lock, requested=None):
    version = lock["ref"].removeprefix("llvmorg-")
    if requested is not None or os.environ.get("LLVM_TBLGEN"):
        candidate = str(requested) if requested is not None else os.environ["LLVM_TBLGEN"]
    else:
        candidates = (shutil.which("llvm-tblgen-" + version.split(".")[0]),
                      f"/opt/homebrew/opt/llvm@{version.split('.')[0]}/bin/llvm-tblgen",
                      shutil.which("llvm-tblgen"))
        candidate = next((p for p in candidates if p and Path(p).is_file()), None)
    resolved = shutil.which(candidate) if candidate else None
    if not resolved:
        raise RuntimeError(f"LLVM {version} llvm-tblgen is required; set --tblgen or LLVM_TBLGEN")
    tool = Path(resolved).resolve()
    banner = subprocess.check_output([str(tool), "--version"], text=True).strip()
    if not re.search(r"\bLLVM version " + re.escape(version) + r"\b", banner):
        raise RuntimeError(f"TableGen must match the pinned LLVM {version}; {tool} reports {banner}")
    out = generated / "llvm/TargetParser"
    out.mkdir(parents=True, exist_ok=True)
    dependencies = set()
    for target, entry, action, name in TABLES:
        td = source / "llvm/lib/Target" / target / (entry + ".td")
        # LLVM 22's depfile lists includes, omitting the main .td input. — KESTREL
        primary = str(td.relative_to(source))
        if primary not in lock["files"]:
            raise RuntimeError(f"unpinned TableGen input: {primary}")
        dependencies.add(primary)
        output, depfile = out / name, out / (name + ".d")
        command = [str(tool), "-gen-" + action, "-I", str(source / "llvm/include"),
                   "-I", str(td.parent), str(td), "-o", str(output), "-d", str(depfile)]
        subprocess.run(command, check=True)
        for raw in depfile.read_text().replace("\\\n", " ").split(":", 1)[1].split():
            path = Path(raw).resolve()
            if not path.is_relative_to(source):
                raise RuntimeError(f"TableGen input outside the pinned source stage: {path}")
            name = str(path.relative_to(source))
            if name not in lock["files"]:
                raise RuntimeError(f"unpinned TableGen input: {name}")
            dependencies.add(name)
    return {
        "tool": str(tool),
        "version": banner,
        "tool_sha256": hashlib.sha256(tool.read_bytes()).hexdigest(),
        "source_dependencies": sorted(dependencies),
        "outputs_sha256": {name: hashlib.sha256((out / name).read_bytes()).hexdigest()
                           for _, _, _, name in TABLES},
    }
