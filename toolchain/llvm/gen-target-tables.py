#!/usr/bin/env python3
#J950_r16_001
#J951_r17_001
"""Generate bootstrap LLVM tables with the pinned LLVM TableGen version."""
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
OBJECT_TABLES = (
    ("IR/Attributes.td", "attrs", "IR/Attributes.inc"),
    ("IR/Intrinsics.td", "intrinsic-enums", "IR/IntrinsicEnums.inc"),
    ("IR/RuntimeLibcalls.td", "runtime-libcalls", "IR/RuntimeLibcalls.inc"),
    ("CodeGen/ValueTypes.td", "vt", "CodeGen/GenVT.inc"),
)


def generate(source, generated, lock, requested=None, *, object_tables=False):
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
    recipes = [(f"llvm/lib/Target/{target}/{entry}.td", action, f"llvm/TargetParser/{name}")
               for target, entry, action, name in TABLES]
    if object_tables:
        recipes += [("llvm/include/llvm/" + td, action, "llvm/" + dest)
                    for td, action, dest in OBJECT_TABLES]
    for primary, action, destination in recipes:
        td = source / primary
        # LLVM 22's depfile lists includes, omitting the main .td input. — KESTREL
        if primary not in lock["files"]:
            raise RuntimeError(f"unpinned TableGen input: {primary}")
        dependencies.add(primary)
        output = generated / destination
        output.parent.mkdir(parents=True, exist_ok=True)
        depfile = output.with_name(output.name + ".d")
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
        **({"object_outputs_sha256": {
            "llvm/" + dest: hashlib.sha256((generated / "llvm" / dest).read_bytes()).hexdigest()
            for _, _, dest in OBJECT_TABLES}} if object_tables else {}),
    }
