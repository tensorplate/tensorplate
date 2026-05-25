#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Create a real SmolVLA TensorPlate validation bundle.

This script expects the Jetson validation Python environment to provide
LeRobot/Transformers. It writes a bundle plus a sample inference request
using the public ``lerobot/smolvla_base`` policy contract.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import math
import pathlib
import struct
from typing import Any


def _sha256(path: pathlib.Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return f"sha256:{h.hexdigest()}"


def _b64_f32(values: list[float]) -> str:
    return base64.b64encode(struct.pack(f"<{len(values)}f", *values)).decode("ascii")


def _b64_i64(values: list[int]) -> str:
    return base64.b64encode(struct.pack(f"<{len(values)}q", *values)).decode("ascii")


def _make_image(camera_index: int) -> list[float]:
    values: list[float] = []
    for c in range(3):
        for y in range(256):
            for x in range(256):
                value = 0.45 + 0.25 * math.sin((x + 1 + camera_index * 3) / 19.0)
                value += 0.20 * math.cos((y + 1 + c * 5) / 29.0)
                value += 0.03 * camera_index + 0.02 * c
                values.append(max(0.0, min(1.0, value)))
    return values


def _tokenize(task: str, model_id: str, cache_dir: str, max_length: int) -> tuple[list[int], list[int]]:
    try:
        from transformers import AutoTokenizer
    except Exception as exc:  # pragma: no cover - exercised on Jetson validation env.
        raise SystemExit(
            "transformers is required to create a tokenized SmolVLA sample request"
        ) from exc

    tokenizer = AutoTokenizer.from_pretrained(model_id, cache_dir=cache_dir, padding_side="right")
    encoded = tokenizer([task], padding="max_length", max_length=max_length, return_tensors=None)
    return list(encoded["input_ids"][0]), [int(v) for v in encoded["attention_mask"][0]]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("out_dir", type=pathlib.Path)
    parser.add_argument("--model-id", default="lerobot/smolvla_base")
    parser.add_argument("--vlm-tokenizer", default="HuggingFaceTB/SmolVLM2-500M-Video-Instruct")
    parser.add_argument("--cache-dir", default="/var/lib/tensorplate/hf-cache")
    parser.add_argument("--device", default="cuda")
    parser.add_argument("--num-steps", type=int, default=2)
    parser.add_argument("--task", default="pick up the cube")
    args = parser.parse_args()

    if args.num_steps <= 0:
        parser.error("--num-steps must be positive")

    out_dir: pathlib.Path = args.out_dir
    out_dir.mkdir(parents=True, exist_ok=True)

    task = args.task if args.task.endswith("\n") else f"{args.task}\n"
    config: dict[str, Any] = {
        "model_id": args.model_id,
        "cache_dir": args.cache_dir,
        "device": args.device,
        "num_steps": args.num_steps,
        "task": task,
    }
    config_path = out_dir / "smolvla_config.json"
    config_path.write_text(json.dumps(config, indent=2) + "\n", encoding="utf-8")

    tokens, attention = _tokenize(task, args.vlm_tokenizer, args.cache_dir, max_length=48)
    manifest = {
        "schema_version": "0.1",
        "name": "tensorplate-smolvla-base",
        "version": "0.1.0",
        "format_version": "0.1",
        "model_class": "vla",
        "backend_hint": "python_pytorch",
        "precision_hint": "fp32",
        "artifacts": [
            {
                "role": "model",
                "kind": "python_pytorch_entry",
                "path": config_path.name,
                "digest": _sha256(config_path),
                "description": "SmolVLA validation config for the managed Python/PyTorch sidecar.",
            }
        ],
        "inputs": [
            {
                "name": "observation.images.camera1",
                "modality": "image",
                "dtype": "float32",
                "shape": [1, 3, 256, 256],
                "encoding": "rgb_nchw",
                "semantics": "observation.image",
            },
            {
                "name": "observation.images.camera2",
                "modality": "image",
                "dtype": "float32",
                "shape": [1, 3, 256, 256],
                "encoding": "rgb_nchw",
                "semantics": "observation.image",
            },
            {
                "name": "observation.images.camera3",
                "modality": "image",
                "dtype": "float32",
                "shape": [1, 3, 256, 256],
                "encoding": "rgb_nchw",
                "semantics": "observation.image",
            },
            {
                "name": "observation.state",
                "modality": "state",
                "dtype": "float32",
                "shape": [1, 6],
                "semantics": "observation.state",
            },
            {
                "name": "observation.language.tokens",
                "modality": "text",
                "dtype": "int64",
                "shape": [1, 48],
                "semantics": "prompt.tokens",
            },
            {
                "name": "observation.language.attention_mask",
                "modality": "text",
                "dtype": "bool",
                "shape": [1, 48],
                "semantics": "prompt.attention_mask",
            },
        ],
        "outputs": [
            {
                "name": "action.chunk",
                "dtype": "float32",
                "shape": [1, 50, 6],
                "semantics": "action.chunk",
                "control_loop": True,
            }
        ],
        "target_hardware": {
            "device_family": "jetson-orin",
            "memory_estimate_bytes": 6442450944,
        },
        "runtime_compatibility": {"min_runtime_version": "0.1.0"},
        "capability_requirements": {
            "deterministic_latency": True,
            "control_loop_integration": True,
        },
        "model_blocks": {
            "vla": {
                "control_frequency_hz": 30,
                "action_horizon_steps": 50,
                "action_chunk_size": 50,
                "input_modalities": ["image", "state", "text"],
                "action_dim": 6,
            }
        },
        "provenance": {
            "source": args.model_id,
            "notes": "Real SmolVLA policy loaded by LeRobot through the Python/PyTorch sidecar.",
        },
    }

    request_inputs: list[dict[str, Any]] = []
    for idx in range(1, 4):
        request_inputs.append(
            {
                "name": f"observation.images.camera{idx}",
                "tensor": {
                    "dtype": "float32",
                    "layout": "row_major",
                    "shape": [1, 3, 256, 256],
                },
                "payload_b64": _b64_f32(_make_image(idx)),
            }
        )
    request_inputs.append(
        {
            "name": "observation.state",
            "tensor": {"dtype": "float32", "layout": "row_major", "shape": [1, 6]},
            "payload_b64": _b64_f32([0.0, 0.05, -0.05, 0.1, -0.1, 0.2]),
        }
    )
    request_inputs.append(
        {
            "name": "observation.language.tokens",
            "tensor": {"dtype": "int64", "layout": "row_major", "shape": [1, 48]},
            "payload_b64": _b64_i64(tokens),
        }
    )
    request_inputs.append(
        {
            "name": "observation.language.attention_mask",
            "tensor": {"dtype": "bool", "layout": "row_major", "shape": [1, 48]},
            "payload_b64": base64.b64encode(bytes(attention)).decode("ascii"),
        }
    )

    request = {
        "schema_version": "0.1",
        "request_id": "validation-smolvla-real-1",
        "endpoint": "validation-smolvla-real",
        "metadata": {"action_chunk_id": "smolvla-chunk-1", "action_chunk_sequence": 1},
        "inputs": request_inputs,
    }
    (out_dir / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    (out_dir / "sample_infer.json").write_text(json.dumps(request, indent=2) + "\n", encoding="utf-8")
    print(f"created {out_dir}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
