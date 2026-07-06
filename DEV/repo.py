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
from pathlib import Path

# source .venv/bin/activate for dependencies 
import requests
from rich.console import Console
from rich.panel import Panel
from rich import box

console = Console()

REPO_URL = "https://github.com/crux161/kumo"

DEV = Path(__file__).resolve().parent
ROOT = DEV.parent
CODEX = DEV / "CODENAMES.json"
BRANDED = ROOT / "BRANDED"
BRANDED_SIG = ROOT / "BRANDED.minisig"
PUBKEY = ROOT / "PUBKEYS" / "CRUX.pub"

project="Kumo"


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


def main():
    data = load_codex()
    try:
        codenames = data["CODENAMES"]
        assigned = codenames["assigned"][0]
        expired = [word.upper() for word in codenames["expired"]["words"]]
    except (KeyError, IndexError, TypeError) as err:
        block(f"{CODEX.name} is missing an expected field: {err}")

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
      Project:            \033[1m{project}\033[0m                              
      codex assigned:     \033[1m{assigned}\033[0m                              
      BRAND marker:       \033[1m{marker}\033[0m                              
      signature:          \033[1m({trusted if marker_is_trusted else 'INVALID SIGNATURE'})\033[0m                              
      marker vs codex:    \033[1m{'MATCH ✅' if marker_is_trusted else 'FAIL ⛔️'}\033[0m                              
      status:             \033[1m{'ACTIVATED' if activated else 'EXPIRED'}\033[0m                              
      Repo => ALIGNED on  \033[1m{marker}\033[0m 💎                              
    """
    console.print(Panel(report, title="Repo Status", box=box.HORIZONTALS))
    sys.exit(0)


if __name__ == "__main__":
    main()
