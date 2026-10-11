#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Fake ballast interpreter for the candidate qualify test.

Runs the `vram_ballast.py` it is given, as an interpreter would, on a fake device:
the script's own argument rules, refusals and report line, with only the device
replaced. The device has TP_FAKE_DEVICE_FREE bytes free, gives no free figure when
TP_FAKE_DEVICE_UNREADABLE is 1, and its allocator takes whole blocks of
TP_FAKE_DEVICE_BLOCK bytes and refuses what does not fit. While memory is held the
marker file carries the bytes left free, which the fake CLI's loads read.

TP_FAKE_DEVICE_ABSENT=1 leaves the interpreter without a device,
TP_FAKE_DEVICE_BROKEN=1 makes the device's code raise what the script does not expect,
and TP_FAKE_DEVICE_FREED is bytes another tenant frees once the ballast holds.
TP_FAKE_BALLAST_FAULT is a ballast that goes wrong around its report: `silent` never
prints one, `chatter` prints another line first and stays, `flood` prints without ever
ending a line, and `exits` holds, reports and is gone before the report can be read.
"""

import atexit
import importlib.util
import io
import os
import signal
import sys
from pathlib import Path

BROKEN = "the fake device's code failed " + "x" * 400


class FakeDevice:
    def __init__(self) -> None:
        self.marker = Path(os.environ["TP_FAKE_OOM_MARKER"])
        self.free = int(os.environ.get("TP_FAKE_DEVICE_FREE", "10000"))
        self.block = int(os.environ.get("TP_FAKE_DEVICE_BLOCK", "64"))
        self.readable = os.environ.get("TP_FAKE_DEVICE_UNREADABLE") != "1"

    def free_bytes(self) -> int:
        if os.environ.get("TP_FAKE_DEVICE_BROKEN") == "1":
            # Blank lines after the traceback, as an exit hook may leave.
            atexit.register(sys.stderr.write, "\n  \n")
            raise ValueError(BROKEN)
        if not self.readable:
            raise RuntimeError("the fake device gives no free figure")
        return self.free

    def hold(self, count: int) -> int:
        taken = -(-count // self.block) * self.block
        if count <= 0 or taken > self.free:
            raise RuntimeError("the fake device is out of memory")
        self.free -= taken - int(os.environ.get("TP_FAKE_DEVICE_FREED", "0"))
        self.marker.write_text(str(self.free))
        return count

    def release(self) -> None:
        self.marker.unlink(missing_ok=True)


def report_after_exit(run) -> int:
    """Run the script to its report, and have a child print it once this process is gone."""
    signal.pause = lambda: None
    printed, sys.stdout = sys.stdout, io.StringIO()
    run()
    report, sys.stdout = sys.stdout.getvalue(), printed
    alive, gone = os.pipe()
    if os.fork() == 0:
        try:
            os.close(gone)
            os.read(alive, 1)  # the end of the stream: the parent has exited
            os.write(1, report.encode())
        finally:
            os._exit(0)
    sys.stderr.write("fake ballast: the device was lost\n")
    return 1


def main() -> int:
    fault = os.environ.get("TP_FAKE_BALLAST_FAULT")
    if fault == "chatter":
        sys.stdout.write("a notice before the report\n")
        sys.stdout.flush()
    if fault in ("silent", "chatter"):
        signal.pause()
    while fault == "flood":
        os.write(1, b"x" * 4096)
    spec = importlib.util.spec_from_file_location("vram_ballast", sys.argv[1])
    script = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(script)
    device = FakeDevice()

    def open_device() -> FakeDevice:
        if os.environ.get("TP_FAKE_DEVICE_ABSENT") == "1":
            raise script.Unavailable("no CUDA device")
        return device

    def run() -> int:
        return script.main(sys.argv[1:], open_device=open_device)

    try:
        return report_after_exit(run) if fault == "exits" else run()
    finally:
        device.release()


if __name__ == "__main__":
    sys.exit(main())
