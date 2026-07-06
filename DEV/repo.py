#!/usr/bin/env python3
"""KUMO repo codename gate.

The signed ``BRANDED`` file names the *edition* of the repository everyone must be working on
(see the coordination guardrail: when a divergence forces a revert-to-last-known-good, CRUX
cycles the codename to the next gemstone and re-signs ``BRANDED``). This script is the gate that
proves a working copy is on the right page:

  1. ``BRANDED`` is authentically signed by CRUX (not tampered);
  2. the marker matches the codex's ``assigned`` codename (the copies agree);
  3. that codename is not in the ``expired`` list (this edition is still live).

It exits ``0`` only when the working copy is ALIGNED, and non-zero with a reason otherwise, so it
can front a workflow or CI step instead of merely reporting.

Sign:   minisign -Sm BRANDED -t "ASSIGNED BY CRUX on MMDDYYYY*(MMDDYYYY)"
Verify: minisign -Vm ./BRANDED -x ./BRANDED.minisig -p PUBKEYS/CRUX.pub
"""

import json
import shutil
import subprocess
import sys
import os
from pathlib import Path

# source .venv/bin/activate for dependencies 
import hashlib
import requests
from rich.console import Console
from rich.panel import Panel
from rich import box

console = Console()

# https://raw.githubusercontent.com/crux161/kumo/refs/heads/feat/i2c-recovery/DEV/SHA/CODENAMES.json.sha256

REPO_URL = "https://raw.githubusercontent.com/crux161/kumo/"
CODEX_URL = "refs/heads/feat/i2c-recovery/DEV/SHA/CODENAMES.json.sha256"

DEV = Path(__file__).resolve().parent
ROOT = DEV.parent
CODEX = DEV / "CODENAMES.json"
CODEX_SHA = DEV / "SHA/CODENAMES.json.sha256"
BRANDED = ROOT / "BRANDED"
BRANDED_SIG = ROOT / "BRANDED.minisig"
PUBKEY = ROOT / "PUBKEYS" / "CRUX.pub"
ID = False

project="Kumo"
report_title = f"{project} Repo Status"

def block(reason):
    """Print a blocking reason and exit non-zero — the gate refuses to pass."""
    print(f"  => BLOCKED: {reason}")
    sys.exit(1)


def load_codex():
    try:
        with open(CODEX) as handle:
            return json.load(handle)
    except FileNotFoundError:
        block(f"{CODEX} is required to run")
    except json.JSONDecodeError as err:
        block(f"{CODEX} is not valid JSON: {err}")


def verify_branded_signature():
    """Verify BRANDED against CRUX's public key; return the trusted comment on success."""
    if not BRANDED_SIG.is_file():
        block(f"{BRANDED_SIG.name} missing — cannot prove BRAND is untampered")
    if not PUBKEY.is_file():
        block(f"{PUBKEY} missing — no key to verify BRAND against")
    if shutil.which("minisign") is None:
        block("minisign not installed — cannot verify BRAND authenticity")

    proc = subprocess.run(
        ["minisign", "-Vm", str(BRANDED), "-x", str(BRANDED_SIG), "-p", str(PUBKEY)],
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        detail = (proc.stdout + proc.stderr).strip()
        block(f"BRANDED signature did not verify against {PUBKEY.name}:\n{detail}")

    for line in proc.stdout.splitlines():
        if line.startswith("Trusted comment:"):
            return line.split(":", 1)[1].strip()
    return "(no trusted comment)"

def obtain_trusted_sha():
    """Obtain trusted shasum from repo, test files against this"""
    response = requests.get(REPO_URL + CODEX_URL)
    # Check if the request was successful
    if response.status_code == 200:
        # Open a local file in write-binary mode ('wb')
        with open(CODEX_SHA, "wb") as file:
            file.write(response.content)
        return True
    else:
        block(f"Failed to download file. Status code: {response.status_code}")
        return False

def verify_file_checksum(file_path, shasum_file_path, hash_algo="sha256"):
    """
    Compares a file against its expected hash listed in a shasum file.
    Supports 'sha256', 'sha1', 'md5', etc.
    """
    if not os.path.exists(file_path) or not os.path.exists(shasum_file_path):
        print("❌ Error: One or both files do not exist.")
        return False

    # 1. Parse the shasum file to extract the expected hash
    expected_hash = None
    target_filename = os.path.basename(file_path)
    
    with open(shasum_file_path, "r", encoding="utf-8") as f:
        for line in f:
            # Split the line by spaces (handles standard format: hash  filename)
            parts = line.strip().split(maxsplit=1)
            if len(parts) == 2:
                line_hash, line_filename = parts
                # Clean up path modifications like leading asterisks or relative dots
                line_filename = os.path.basename(line_filename.lstrip("*"))
                
                if line_filename == target_filename:
                    expected_hash = line_hash.lower()
                    break

    if not expected_hash:
        print(f"❌ Error: No checksum entry found for '{target_filename}' in the shasum file.")
        return False
    
    # 2. Compute the actual hash of the target file in chunks (memory safe)
    hasher = hashlib.new(hash_algo)

    # 64KB chunks
    chunk_size = 65536 
    with open(CODEX, "rb") as f:
        while chunk := f.read(chunk_size):
            hasher.update(chunk)
            
    computed_hash = hasher.hexdigest().lower()
    if computed_hash == expected_hash:
        return [True, computed_hash]
    else:
        return [False, computed_hash]

def main():

    have_trusted_sha = obtain_trusted_sha()
    data = load_codex()
    try:
        codenames = data["CODENAMES"]
        assigned = codenames["assigned"][0]
        expired = [word.upper() for word in codenames["expired"]["words"]]
    except (KeyError, IndexError, TypeError) as err:
        block(f"{CODEX.name} is missing an expected field: {err}")

    if have_trusted_sha:
        ID = verify_file_checksum(CODEX, CODEX_SHA)  

    if not BRANDED.is_file():
        block(f"{BRANDED.name} missing — no edition marker in this working copy")
    marker = BRANDED.read_text().strip()

    trusted = verify_branded_signature()

    # The signed marker is the source of truth; the codex must agree (case-insensitive, so the
    # uppercase BRANDED file and title-case codex entry line up).
    if marker.upper() != assigned.upper():
        block(f"BRANDED marker '{marker}' != codex assigned '{assigned}' — copies have diverged")

    activated = marker.upper() not in expired    
    
    if not activated:
        block(f"codename '{marker}' is in the expired list — this edition is retired")

    trusted_marker = trusted.split()[0]
    if trusted_marker == marker : 
        marker_is_trusted = True
    else:
        marker_is_trusted = False

    report = f"""
      Project:            \033[1m{ROOT}\033[0m                              
      codex assigned:     \033[1m{assigned}\033[0m                              
      BRAND marker:       \033[1m{marker}\033[0m                              
      signature:          \033[1m({trusted if marker_is_trusted else 'INVALID SIGNATURE'})\033[0m                              
      marker vs codex:    \033[1m{'MATCH ✅' if marker_is_trusted else 'FAIL ⛔️'}\033[0m                              
      status:             \033[1m{'ACTIVATED' if activated else 'EXPIRED'}\033[0m
      CODEX sha256:       \033[1m{ID[1]}\033[0m
      CODEX Checksum:     \033[1m{'VALID ✅' if ID[0] else 'INVALID⛔️'}\033[0m
      Trusted Checksum:   \033[1m{'VALID ✅' if have_trusted_sha else 'INVALID⛔️'}\033[0m                              
      Repo => ALIGNED on  \033[1m{marker}\033[0m 💎                              
    """
    console.print(Panel(report, title=report_title, box=box.HORIZONTALS))
    sys.exit(0)


if __name__ == "__main__":
    main()
