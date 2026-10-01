#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Hold a fixed number of bytes of device memory until terminated.

The qualify tool starts this with the runtime's own interpreter before a
deploy it expects to fail with `oom_error`, and terminates it afterwards.
"""

import signal
import sys


def main(argv: list[str]) -> int:
    if len(argv) != 2 or not argv[1].isdigit() or int(argv[1]) <= 0:
        sys.stderr.write("usage: vram_ballast.py <bytes>\n")
        return 2
    try:
        import torch
    except ImportError:
        sys.stderr.write("vram_ballast: torch is not importable in this interpreter\n")
        return 3
    if not torch.cuda.is_available():
        sys.stderr.write("vram_ballast: no CUDA device\n")
        return 3
    try:
        held = torch.empty(int(argv[1]), dtype=torch.uint8, device="cuda")
    except RuntimeError:
        sys.stderr.write("vram_ballast: the device refused the allocation\n")
        return 4
    sys.stdout.write(f"held {held.numel()}\n")
    sys.stdout.flush()
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    signal.pause()
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
