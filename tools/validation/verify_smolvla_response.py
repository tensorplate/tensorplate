#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Verify the E15 real SmolVLA response payload."""

from __future__ import annotations

import base64
import json
import math
import pathlib
import struct
import sys


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        sys.stderr.write("usage: verify_smolvla_response.py <response.json>\n")
        return 2
    response = json.loads(pathlib.Path(argv[1]).read_text(encoding="utf-8"))
    if response.get("status") != "success":
        sys.stderr.write(f"expected success response, got {response.get('status')!r}\n")
        return 1
    outputs = response.get("outputs")
    if not isinstance(outputs, list) or len(outputs) != 1:
        sys.stderr.write("expected exactly one output\n")
        return 1
    output = outputs[0]
    if output.get("name") != "action.chunk":
        sys.stderr.write(f"expected output name action.chunk, got {output.get('name')!r}\n")
        return 1
    tensor = output.get("tensor") or {}
    if tensor.get("dtype") != "float32" or tensor.get("shape") != [1, 50, 6]:
        sys.stderr.write(f"unexpected output tensor metadata: {tensor!r}\n")
        return 1
    payload = base64.b64decode(output.get("payload_b64", ""))
    if len(payload) != 1 * 50 * 6 * 4:
        sys.stderr.write(f"expected 1200 output bytes, got {len(payload)}\n")
        return 1
    values = struct.unpack("<300f", payload)
    if not all(math.isfinite(value) for value in values):
        sys.stderr.write("action.chunk contains a non-finite value\n")
        return 1
    abs_sum = sum(abs(value) for value in values)
    if abs_sum <= 1e-6:
        sys.stderr.write("action.chunk appears to be all zeros\n")
        return 1
    print(
        "smolvla_response: ok "
        f"shape=[1,50,6] sum={sum(values):.6f} "
        f"abs_sum={abs_sum:.6f} min={min(values):.6f} max={max(values):.6f}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
