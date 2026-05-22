#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Verify the E15 TensorRT ResNet50 response payload."""

from __future__ import annotations

import base64
import json
import math
import pathlib
import struct
import sys


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        sys.stderr.write("usage: verify_trt_resnet50_response.py <response.json>\n")
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
    if output.get("name") != "gpu_0/softmax_1":
        sys.stderr.write(f"expected output name gpu_0/softmax_1, got {output.get('name')!r}\n")
        return 1
    tensor = output.get("tensor") or {}
    if tensor.get("dtype") != "float32" or tensor.get("shape") != [1, 1000]:
        sys.stderr.write(f"unexpected output tensor metadata: {tensor!r}\n")
        return 1
    payload = base64.b64decode(output.get("payload_b64", ""))
    if len(payload) != 4000:
        sys.stderr.write(f"expected 4000 output bytes, got {len(payload)}\n")
        return 1
    values = struct.unpack("<1000f", payload)
    for idx, value in enumerate(values):
        if not math.isfinite(value):
            sys.stderr.write(f"non-finite output at {idx}: {value}\n")
            return 1
        if value < -1e-5 or value > 1.00001:
            sys.stderr.write(f"softmax output out of range at {idx}: {value}\n")
            return 1
    total = sum(values)
    if not 0.95 <= total <= 1.05:
        sys.stderr.write(f"softmax sum out of expected range: {total}\n")
        return 1
    top5 = sorted(enumerate(values), key=lambda item: item[1], reverse=True)[:5]
    top5_text = ", ".join(f"{idx}:{score:.6f}" for idx, score in top5)
    print(f"trt_resnet50_response: ok sum={total:.6f} top5=[{top5_text}]")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
