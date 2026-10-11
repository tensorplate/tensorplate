#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Fake ballast interpreter for the candidate qualify test.

Runs the `vram_ballast.py` it is given, as an interpreter would, on a fake device:
the script's own argument rules, refusals and report line, with only the device
replaced. The device has TP_FAKE_DEVICE_FREE bytes free, gives no free figure when
TP_FAKE_DEVICE_UNREADABLE is 1, and its allocator takes whole blocks of
TP_FAKE_DEVICE_BLOCK bytes and refuses what does not fit. While memory is held the
marker file carries the bytes left free, which the fake CLI's loads read.
"""

import importlib.util
import os
import sys
from pathlib import Path


class FakeDevice:
    def __init__(self) -> None:
        self.marker = Path(os.environ["TP_FAKE_OOM_MARKER"])
        self.free = int(os.environ.get("TP_FAKE_DEVICE_FREE", "10000"))
        self.block = int(os.environ.get("TP_FAKE_DEVICE_BLOCK", "64"))
        self.readable = os.environ.get("TP_FAKE_DEVICE_UNREADABLE") != "1"

    def free_bytes(self) -> int:
        if not self.readable:
            raise RuntimeError("the fake device gives no free figure")
        return self.free

    def hold(self, count: int) -> int:
        taken = -(-count // self.block) * self.block
        if count <= 0 or taken > self.free:
            raise RuntimeError("the fake device is out of memory")
        self.free -= taken
        self.marker.write_text(str(self.free))
        return count

    def release(self) -> None:
        self.marker.unlink(missing_ok=True)


def main() -> int:
    spec = importlib.util.spec_from_file_location("vram_ballast", sys.argv[1])
    script = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(script)
    device = FakeDevice()
    try:
        return script.main(sys.argv[1:], open_device=lambda: device)
    finally:
        device.release()


if __name__ == "__main__":
    sys.exit(main())
