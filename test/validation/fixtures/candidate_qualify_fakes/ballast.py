#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Fake VRAM ballast for the candidate qualify test.

Marks the fake CLI out of memory while alive.
"""

import os
import signal
import sys
import time

marker = os.environ["TP_FAKE_OOM_MARKER"]
open(marker, "w").close()


def bye(*_):
    os.unlink(marker)
    sys.exit(0)


signal.signal(signal.SIGTERM, bye)
sys.stdout.write("held 1024\n")
sys.stdout.flush()
while True:
    time.sleep(0.1)
