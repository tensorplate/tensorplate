#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Fake memory sampler for the candidate qualify test.

It takes the operator wrapper's arguments and answers with the real entry's JSON Lines
output, summary shape and exit codes.
"""

import json
import os
import sys
import time

args = sys.argv[1:]


def opt(name):
    return args[args.index(name) + 1]


def ms(text):
    return int(text[:-2]) if text.endswith("ms") else int(text[:-1]) * 1000


interval, duration, out, phase, domain = (
    ms(opt("--interval")),
    ms(opt("--duration")),
    opt("--out"),
    opt("--phase"),
    opt("--domain"),
)
processes = [args[i + 1] for i, a in enumerate(args) if a == "--process"]
if os.path.exists(out):
    sys.stderr.write("refusing to overwrite\n")
    sys.exit(1)
ticks = duration // interval
mode = os.environ.get("TP_FAKE_SAMPLER_MODE", "complete")
base = 4_000_000_000 if domain == "device_vram" else 6_000_000_000
with open(out, "w") as handle:
    for tick in range(ticks):
        handle.write(
            json.dumps(
                {
                    "schema_version": "0.1",
                    "domain": domain,
                    "source": "fake",
                    "sampled_monotonic_ns": tick * interval * 1_000_000,
                }
            )
            + "\n"
        )
        time.sleep(interval / 1000)
procs = []
for item in processes:
    pid, role = item.split(":")
    procs.append(
        {
            "pid": int(pid),
            "role": role,
            "peak": {"max_bytes": 1_000_000_000 + int(pid), "available_samples": ticks},
        }
    )
complete = mode == "complete"
report = {
    "expected_ticks": ticks,
    "completed_ticks": ticks if complete else ticks - 1,
    "late_ticks": 0 if complete else 1,
    "complete": complete,
    "domains": [
        {
            "domain": domain,
            "consumed": {
                "max_bytes": base + (1 if phase == "load" else 0),
                "available_samples": ticks,
            },
            "processes": procs,
        }
    ],
}
print(
    json.dumps(
        {
            "schema_version": "0.1",
            "phase": phase,
            "interval_ms": interval,
            "duration_ms": duration,
            "report": report,
        }
    )
)
sys.exit(0 if complete else 3)
