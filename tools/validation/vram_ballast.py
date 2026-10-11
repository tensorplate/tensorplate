#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Hold device memory until terminated.

`vram_ballast.py BYTES` holds that many bytes. `vram_ballast.py --leave-free
BYTES` reads the device's free memory and holds all of it but that many.

Once it has a device it prints one line. `held H free F left L` gives the
bytes held and the free memory read before and while holding them, `unknown`
for a figure the device did not give. `refused free F` means nothing is held,
and the exit status says why.

The qualify tool starts this with the runtime's own interpreter before a
deploy it expects to fail with `oom_error`, and terminates it afterwards.
"""

from __future__ import annotations

import re
import signal
import sys
from collections.abc import Callable
from typing import Any

BYTES, LEAVE_FREE = "bytes", "leave_free"
USAGE = "usage: vram_ballast.py (BYTES | --leave-free BYTES)\n"
EXIT_USAGE = 2
EXIT_NO_DEVICE = 3
EXIT_ALLOCATION_REFUSED = 4
EXIT_FREE_UNREADABLE = 5
EXIT_NOTHING_TO_HOLD = 6
_HELD = re.compile(r"held ([0-9]+) free ([0-9]+|unknown) left ([0-9]+|unknown)\n")
_REFUSED = re.compile(r"refused free ([0-9]+|unknown)\n")


class Unavailable(Exception):
    """This interpreter has no device to hold memory on."""


class TorchDevice:
    def __init__(self) -> None:
        try:
            import torch
        except ImportError as exc:
            raise Unavailable("torch is not importable in this interpreter") from exc
        if not torch.cuda.is_available():
            raise Unavailable("no CUDA device")
        self._torch = torch
        self._held: Any = None

    def free_bytes(self) -> int:
        """Free memory on the current device, as `cudaMemGetInfo` gives it to this process."""
        return int(self._torch.cuda.mem_get_info()[0])

    def hold(self, count: int) -> int:
        self._held = self._torch.empty(count, dtype=self._torch.uint8, device="cuda")
        return int(self._held.numel())


def parse_rule(arguments: list[str]) -> tuple[str, int] | None:
    if len(arguments) == 1:
        rule, text = BYTES, arguments[0]
    elif len(arguments) == 2 and arguments[0] == "--leave-free":
        rule, text = LEAVE_FREE, arguments[1]
    else:
        return None
    if re.fullmatch(r"[0-9]+", text) is None or int(text) == 0:
        return None
    return rule, int(text)


def command_arguments(rule: str, count: int) -> list[str]:
    return [str(count)] if rule == BYTES else ["--leave-free", str(count)]


def parse_report(line: str) -> dict[str, int | None] | None:
    """The figures of a report line, None for one given as `unknown`; None for any other line."""

    def figure(text: str) -> int | None:
        return None if text == "unknown" else int(text)

    held = _HELD.fullmatch(line)
    if held is not None:
        return {"held": int(held[1]), "free": figure(held[2]), "left": figure(held[3])}
    refused = _REFUSED.fullmatch(line)
    if refused is not None:
        return {"held": None, "free": figure(refused[1]), "left": None}
    return None


def _free(device: Any) -> int | None:
    try:
        return device.free_bytes()
    except RuntimeError:
        return None


def _text(figure: int | None) -> str:
    return "unknown" if figure is None else str(figure)


def main(argv: list[str], open_device: Callable[[], Any] = TorchDevice) -> int:
    rule = parse_rule(argv[1:])
    if rule is None:
        sys.stderr.write(USAGE)
        return EXIT_USAGE
    kind, count = rule
    try:
        device = open_device()
    except Unavailable as exc:
        sys.stderr.write(f"vram_ballast: {exc}\n")
        return EXIT_NO_DEVICE
    free = _free(device)

    def refused(status: int, why: str) -> int:
        sys.stdout.write(f"refused free {_text(free)}\n")
        sys.stdout.flush()
        sys.stderr.write(f"vram_ballast: {why}\n")
        return status

    if kind == LEAVE_FREE:
        if free is None:
            return refused(EXIT_FREE_UNREADABLE, "the device's free memory cannot be read")
        if free <= count:
            return refused(
                EXIT_NOTHING_TO_HOLD,
                f"{free} bytes are free, not more than the {count} to leave free",
            )
        count = free - count
    try:
        held = device.hold(count)
    except RuntimeError:
        return refused(EXIT_ALLOCATION_REFUSED, "the device refused the allocation")
    left = _free(device)
    # Before the line, so a reader that has it can always end this with SIGTERM.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    sys.stdout.write(f"held {held} free {_text(free)} left {_text(left)}\n")
    sys.stdout.flush()
    signal.pause()
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
