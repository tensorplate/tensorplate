#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# Create a real TensorRT identity bundle on a Jetson target.
#
# Usage:
#   tools/validation/create_trt_identity_bundle.sh /tmp/tp-trt-identity

set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <output-bundle-dir>" >&2
  exit 2
fi

repo_root="$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)"
out_dir="$1"
engine_path="${out_dir}/model.engine"
builder_bin="${TMPDIR:-/tmp}/tp_trt_identity_engine"

mkdir -p "${out_dir}"
rm -f "${engine_path}"

cxx="${CXX:-c++}"
cuda_root="${CUDA_HOME:-${CUDA_PATH:-/usr/local/cuda}}"
cuda_include=""
for candidate in \
  "${cuda_root}/include" \
  "${cuda_root}/targets/aarch64-linux/include" \
  /usr/local/cuda/include \
  /usr/local/cuda/targets/aarch64-linux/include
do
  if [ -f "${candidate}/cuda_runtime_api.h" ]; then
    cuda_include="${candidate}"
    break
  fi
done

if [ -z "${cuda_include}" ]; then
  echo "could not find cuda_runtime_api.h; set CUDA_HOME or install CUDA headers" >&2
  exit 1
fi

cuda_libdir=""
for candidate in \
  "${cuda_root}/lib64" \
  "${cuda_root}/targets/aarch64-linux/lib" \
  /usr/local/cuda/lib64 \
  /usr/local/cuda/targets/aarch64-linux/lib
do
  if [ -e "${candidate}/libcudart.so" ]; then
    cuda_libdir="${candidate}"
    break
  fi
done

if [ -n "${cuda_libdir}" ]; then
  cuda_link_flags="-L${cuda_libdir}"
else
  cuda_link_flags=""
fi

"${cxx}" -std=c++17 -O2 -Wall -Wextra \
  -I"${cuda_include}" \
  "${repo_root}/tools/validation/trt_identity_engine.cpp" \
  -o "${builder_bin}" \
  ${cuda_link_flags} \
  -lnvinfer -lcudart

"${builder_bin}" "${engine_path}"

digest="sha256:$(sha256sum "${engine_path}" | awk '{print $1}')"
size="$(wc -c < "${engine_path}" | tr -d ' ')"

python3 - "${out_dir}" "${digest}" "${size}" <<'PY'
import base64
import json
import pathlib
import struct
import sys

out_dir = pathlib.Path(sys.argv[1])
digest = sys.argv[2]
size = int(sys.argv[3])

manifest = {
    "schema_version": "0.1",
    "name": "tensorplate-trt-identity-vision",
    "version": "0.1.0",
    "format_version": "0.1",
    "model_class": "vision",
    "backend_hint": "tensorrt",
    "precision_hint": "fp32",
    "artifacts": [
        {
            "role": "model",
            "kind": "tensorrt_engine",
            "path": "model.engine",
            "digest": digest,
            "byte_size": size,
            "description": "release validation TensorRT identity engine generated on the target Jetson.",
        }
    ],
    "inputs": [
        {
            "name": "image",
            "modality": "image",
            "dtype": "float32",
            "shape": [1, 3, 4, 4],
            "layout": "row_major",
            "semantics": "observation.image",
        }
    ],
    "outputs": [
        {
            "name": "features",
            "dtype": "float32",
            "shape": [1, 3, 4, 4],
            "semantics": "vision.identity_features",
        }
    ],
    "target_hardware": {
        "device_family": "jetson-orin",
        "memory_estimate_bytes": 67108864,
    },
    "runtime_compatibility": {"min_runtime_version": "0.1.0"},
    "capability_requirements": {
        "deterministic_latency": True,
        "fixed_shape": True,
    },
    "precision": {
        "profile": "fp32",
        "jetson": {"supported_profiles": ["fp32"]},
    },
    "model_blocks": {
        "vision": {
            "task": "classification",
            "input_size": {"height": 4, "width": 4},
            "color_space": "rgb",
            "normalization": {"mean": [0.0, 0.0, 0.0], "std": [1.0, 1.0, 1.0]},
        }
    },
}

payload = struct.pack("<48f", *[float(i) for i in range(48)])
request = {
    "schema_version": "0.1",
    "request_id": "validation-trt-identity-1",
    "endpoint": "validation-trt-identity",
    "inputs": [
        {
            "name": "image",
            "tensor": {
                "dtype": "float32",
                "layout": "row_major",
                "shape": [1, 3, 4, 4],
            },
            "payload_b64": base64.b64encode(payload).decode("ascii"),
        }
    ],
}

(out_dir / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
(out_dir / "sample_infer.json").write_text(json.dumps(request, indent=2) + "\n", encoding="utf-8")
PY

echo "created ${out_dir}"
