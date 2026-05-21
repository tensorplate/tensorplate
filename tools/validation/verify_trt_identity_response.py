#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Verify the E15 TensorRT identity response payload."""

from __future__ import annotations

import base64
import json
import math
import pathlib
import struct
import sys


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        sys.stderr.write("usage: verify_trt_identity_response.py <response.json>\n")
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
    if output.get("name") != "features":
        sys.stderr.write(f"expected output name features, got {output.get('name')!r}\n")
        return 1
    tensor = output.get("tensor") or {}
    if tensor.get("dtype") != "float32" or tensor.get("shape") != [1, 3, 4, 4]:
        sys.stderr.write(f"unexpected output tensor metadata: {tensor!r}\n")
        return 1
    payload = base64.b64decode(output.get("payload_b64", ""))
    values = struct.unpack("<48f", payload)
    for idx, value in enumerate(values):
        if not math.isclose(value, float(idx), rel_tol=0.0, abs_tol=1e-6):
            sys.stderr.write(f"value mismatch at {idx}: {value} != {idx}\n")
            return 1
    print("trt_identity_response: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
