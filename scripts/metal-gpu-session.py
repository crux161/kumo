#!/usr/bin/env python3
#j899
#j900
"""gpu-client's boot session outcome on the Orange Pi 5 Plus, over N PXE boots.

Per boot it records whether drv-fb's R1 refused the firmware's live display mode (the
firmware sometimes also drives HDMI1 and DP0 on VP2, which KUMO's table does not describe),
and how gpu-client's session ended: `J0c complete`, `presenter cancelled diagnostic`
(j899: the presenter gave up, exit 0), or `session failed: <cause>`. A boot fails the suite
when the session fails or a TOWER names gpu-client; an R1 refusal alone does not.

Reuses scripts/metal-sh3e.py's board machinery (import only). Prerequisites per the opi5
serial-loop recipe; build the image with KUMO_CONSOLE_LOGLEVEL=6 so gpu-client's lines reach
the console.
"""

import argparse
import importlib.util
import json
import re
import shutil
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BOOT = b"echo GPUSESSION begins\necho GPUSESSION ends\n"
OUTCOMES = (
    ("complete", "gpu-client: J0c complete"),
    ("cancelled", "gpu-client: presenter cancelled diagnostic"),
    ("failed", "gpu-client: session failed"),
)


def load_board():
    spec = importlib.util.spec_from_file_location("sh3e", ROOT / "scripts/metal-sh3e.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def live(text):
    """Lines as printed live; the kernel's settle-time replay carries a second timestamp."""
    return "\n".join(
        line for line in text.splitlines() if not re.match(r"^\[[0-9:.]+\] \[ *[0-9]+\.[0-9]+\]", line)
    )


def boot_once(board, tftp, log, timeout):
    pxehost = board.start_pxehost(tftp, log.with_name("pxehost.log"))
    tio_pid, master = board.start_tio(log)
    try:
        board.wake_board(log)
        board.wait_for("GPUSESSION ends", log, timeout)
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            text = live(board.log_whole(log))
            tail = text[text.index("GPUSESSION ends"):]
            if any(marker in tail for _, marker in OUTCOMES):
                time.sleep(2.0)  # let the TOWER (if any) finish printing
                break
            time.sleep(0.5)
        text = live(board.log_whole(log))
    finally:
        board.stop_all(pxehost, tio_pid, master)
    outcome = next((name for name, marker in OUTCOMES if marker in text), "none")
    cause = re.search(r"gpu-client: session failed: ([^\n]*)", text)
    launch = re.search(r"gpu-client: proc koid=0x([0-9a-f]+)", text)
    towers = [t for t in text.split("TOWER")[1:]]
    gpu_tower = bool(launch) and any(
        f"victim koid=0x{int(launch[1], 16):016x}" in t[:300] for t in towers
    )
    return {
        "r1_refused": "display: R1 refused" in text,
        # j900: the firmware's HDMI1/DP0 mirror switched off before R1's comparison.
        "mirror_detached": "R1 switched off the firmware's VP2 mirror" in text,
        "r1_rebuilt": "display: R1 rebuilt from KUMO's table" in text,
        "presenter_cancelled": "native diagnostic cancelled" in text,
        "outcome": outcome,
        "cause": cause[1] if cause else None,
        "towers": len(towers),
        "gpu_client_tower": gpu_tower,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, default=ROOT / "build/verification/j899/metal")
    parser.add_argument("--boots", type=int, default=2)
    parser.add_argument("--timeout", type=float, default=240.0)
    args = parser.parse_args()

    board = load_board()
    if shutil.which("tio") is None:
        sys.exit("tio not on PATH")
    if not board.PXEHOST.is_file():
        sys.exit(f"missing {board.PXEHOST}; build with scripts/build-pxehost-kumo.sh")
    work = args.work
    work.mkdir(parents=True, exist_ok=True)
    tftp = work / "tftp"
    board.stage_tree(tftp)
    board.stage_boot(tftp, BOOT)

    results = []
    for index in range(1, args.boots + 1):
        log = work / f"boot{index}" / "boot.log"
        log.parent.mkdir(parents=True, exist_ok=True)
        result = boot_once(board, tftp, log, args.timeout)
        results.append(result)
        print(f"boot {index}: {result}", flush=True)
    (work / "manifest.json").write_text(json.dumps(results, indent=2) + "\n")
    bad = [r for r in results if r["outcome"] in ("failed", "none") or r["gpu_client_tower"]]
    if bad:
        sys.exit(f"{len(bad)} boot(s) ended badly; see {work}")
    print("gpu-client session suite green", flush=True)


if __name__ == "__main__":
    main()
