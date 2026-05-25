#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# Create a production-size TensorRT ResNet50 validation bundle on a Jetson target.
#
# Usage:
#   tools/validation/create_trt_resnet50_bundle.sh /var/lib/tensorplate/validation/tp-trt-resnet50
#
# The default ONNX model is NVIDIA TensorRT's installed ResNet50 sample:
#   /usr/src/tensorrt/data/resnet50/ResNet50.onnx

set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  echo "usage: $0 <output-bundle-dir> [resnet50.onnx]" >&2
  exit 2
fi

out_dir="$1"
onnx_path="${2:-/usr/src/tensorrt/data/resnet50/ResNet50.onnx}"
engine_path="${out_dir}/model.engine"

if [ ! -f "${onnx_path}" ]; then
  echo "ResNet50 ONNX not found: ${onnx_path}" >&2
  exit 1
fi

trtexec="${TRTEXEC:-}"
if [ -z "${trtexec}" ]; then
  for candidate in \
    /usr/src/tensorrt/bin/trtexec \
    /usr/bin/trtexec \
    trtexec
  do
    if command -v "${candidate}" >/dev/null 2>&1 || [ -x "${candidate}" ]; then
      trtexec="${candidate}"
      break
    fi
  done
fi

if [ -z "${trtexec}" ]; then
  echo "trtexec not found; set TRTEXEC or install TensorRT samples" >&2
  exit 1
fi

mkdir -p "${out_dir}"
rm -f "${engine_path}"

"${trtexec}" \
  --onnx="${onnx_path}" \
  --saveEngine="${engine_path}" \
  --fp16 \
  --skipInference

digest="sha256:$(sha256sum "${engine_path}" | awk '{print $1}')"
size="$(wc -c < "${engine_path}" | tr -d ' ')"

python3 - "${out_dir}" "${digest}" "${size}" <<'PY'
import base64
import json
import math
import pathlib
import struct
import sys

out_dir = pathlib.Path(sys.argv[1])
digest = sys.argv[2]
size = int(sys.argv[3])

manifest = {
    "schema_version": "0.1",
    "name": "tensorplate-trt-resnet50",
    "version": "0.1.0",
    "format_version": "0.1",
    "model_class": "vision",
    "backend_hint": "tensorrt",
    "precision_hint": "fp16",
    "artifacts": [
        {
            "role": "model",
            "kind": "tensorrt_engine",
            "path": "model.engine",
            "digest": digest,
            "byte_size": size,
            "description": "TensorRT FP16 ResNet50 engine generated from NVIDIA's Jetson-installed sample ONNX.",
        }
    ],
    "inputs": [
        {
            "name": "gpu_0/data_0",
            "modality": "image",
            "dtype": "float32",
            "shape": [1, 3, 224, 224],
            "layout": "row_major",
            "semantics": "observation.image.normalized_nchw",
        }
    ],
    "outputs": [
        {
            "name": "gpu_0/softmax_1",
            "dtype": "float32",
            "shape": [1, 1000],
            "semantics": "vision.class_probability",
        }
    ],
    "target_hardware": {
        "device_family": "jetson-orin",
        "memory_estimate_bytes": 536870912,
    },
    "runtime_compatibility": {"min_runtime_version": "0.1.0"},
    "capability_requirements": {
        "deterministic_latency": True,
        "fixed_shape": True,
    },
    "precision": {
        "profile": "fp16",
        "jetson": {"supported_profiles": ["fp32", "fp16"]},
    },
    "model_blocks": {
        "vision": {
            "task": "classification",
            "input_size": {"height": 224, "width": 224},
            "color_space": "rgb",
            "normalization": {
                "mean": [0.485, 0.456, 0.406],
                "std": [0.229, 0.224, 0.225],
            },
        }
    },
}

means = [0.485, 0.456, 0.406]
stds = [0.229, 0.224, 0.225]
values: list[float] = []
for c in range(3):
    for y in range(224):
        for x in range(224):
            # Deterministic synthetic image with nontrivial channel and spatial variation.
            raw = 0.5 + 0.25 * math.sin((x + 1) / 17.0) + 0.20 * math.cos((y + 1) / 23.0)
            raw += 0.05 * c
            raw = max(0.0, min(1.0, raw))
            values.append((raw - means[c]) / stds[c])

payload = struct.pack(f"<{len(values)}f", *values)
request = {
    "schema_version": "0.1",
    "request_id": "e15-trt-resnet50-1",
    "endpoint": "e15-trt-resnet50",
    "inputs": [
        {
            "name": "gpu_0/data_0",
            "tensor": {
                "dtype": "float32",
                "layout": "row_major",
                "shape": [1, 3, 224, 224],
            },
            "payload_b64": base64.b64encode(payload).decode("ascii"),
        }
    ],
}

(out_dir / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
(out_dir / "sample_infer.json").write_text(json.dumps(request, indent=2) + "\n", encoding="utf-8")
PY

echo "created ${out_dir}"
