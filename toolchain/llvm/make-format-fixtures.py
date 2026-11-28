#!/usr/bin/env python3
#J960_r26_001
"""Generate deterministic COFF, Mach-O and WebAssembly reader fixtures on the host."""
import hashlib
import json
from pathlib import Path
import subprocess
import struct


def generate(out, generated):
    out.mkdir(parents=True, exist_ok=True)
    generated.mkdir(parents=True, exist_ok=True)
    source = Path(__file__).with_name("format-fixture.c")
    recipes = (("aarch64-pc-windows-msvc", "coff.obj", "FixtureCOFF"),
               ("arm64-apple-macosx13", "macho.o", "FixtureMachO"),
               ("wasm32-unknown-unknown", "wasm.o", "FixtureWasm"))
    header = "// Generated host inputs; native instruction encoding is outside this proof.\n#pragma once\n"
    hashes = {}
    for triple, name, symbol in recipes:
        command = ["clang", "--target=" + triple, "-ffreestanding", "-fno-stack-protector",
                   "-fno-unwind-tables", "-fno-asynchronous-unwind-tables", "-c", str(source), "-o", str(out / name)]
        if triple.endswith("windows-msvc"):
            command += ["-mno-incremental-linker-compatible"]  # No COFF wall-clock timestamp. — TEAL
        subprocess.run(command, check=True)
        data = (out / name).read_bytes()
        if triple.endswith("windows-msvc") and struct.unpack_from("<I", data, 4)[0] != 0:
            raise RuntimeError("COFF fixture contains a wall-clock timestamp")
        hashes[name] = hashlib.sha256(data).hexdigest()
        header += f"alignas(8) inline constexpr unsigned char {symbol}[] = {{\n"
        header += "\n".join("    " + ", ".join(f"0x{byte:02x}" for byte in data[i:i+16]) + "," for i in range(0, len(data), 16)) + "\n};\n"
    (generated / "format-fixtures.h").write_text(header)
    result = {"scope": "host compiles foreign object inputs; native APIs read their formats",
              "clang": subprocess.check_output(["clang", "--version"], text=True).strip(),
              "source_sha256": hashlib.sha256(source.read_bytes()).hexdigest(), "files_sha256": hashes,
              "header_sha256": hashlib.sha256(header.encode()).hexdigest()}
    (out / "manifest.json").write_text(json.dumps(result, indent=2) + "\n")
    return result
