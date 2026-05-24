# SPDX-License-Identifier: Apache-2.0
"""LeRobot SmolVLA backend for on-device validation.

The imports for torch / transformers / lerobot stay inside lifecycle
methods so the default fixture backend remains dependency-free on host CI.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any

from tensorplate_pytorch_backend.backends.base import Backend, BackendError, NamedTensor
from tensorplate_pytorch_backend.protocol import (
    ERR_CONFIG_INVALID,
    ERR_INFERENCE_FAILED,
    ERR_LOAD_FAILED,
    ERR_NOT_READY,
    ERR_SHAPE_MISMATCH,
)


def _read_validation_config(model_spec: dict[str, Any]) -> dict[str, Any]:
    artifact_path = model_spec.get("artifact_path")
    if not isinstance(artifact_path, str) or not artifact_path:
        return {}
    path = Path(artifact_path)
    if path.suffix != ".json":
        return {}
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise BackendError(ERR_CONFIG_INVALID, f"SmolVLA config not found: {path}") from None
    except json.JSONDecodeError as exc:
        raise BackendError(ERR_CONFIG_INVALID, f"SmolVLA config JSON invalid: {exc}") from exc
    if not isinstance(raw, dict):
        raise BackendError(ERR_CONFIG_INVALID, "SmolVLA config must be a JSON object")
    return raw


def _string_config(config: dict[str, Any], key: str, default: str) -> str:
    value = config.get(key, os.environ.get(f"TP_SMOLVLA_{key.upper()}", default))
    if not isinstance(value, str) or not value:
        raise BackendError(ERR_CONFIG_INVALID, f"SmolVLA config `{key}` must be a non-empty string")
    return value


def _optional_int_config(config: dict[str, Any], key: str) -> int | None:
    value = config.get(key, os.environ.get(f"TP_SMOLVLA_{key.upper()}"))
    if value is None or value == "":
        return None
    try:
        parsed = int(value)
    except (TypeError, ValueError) as exc:
        raise BackendError(
            ERR_CONFIG_INVALID, f"SmolVLA config `{key}` must be an integer"
        ) from exc
    if parsed <= 0:
        raise BackendError(ERR_CONFIG_INVALID, f"SmolVLA config `{key}` must be positive")
    return parsed


class SmolVLABackend(Backend):
    """Runs a real LeRobot SmolVLA policy inside the managed sidecar."""

    def __init__(self) -> None:
        self._loaded = False
        self._policy: Any | None = None
        self._config: Any | None = None
        self._device: Any | None = None
        self._torch: Any | None = None
        self._np: Any | None = None
        self._obs_state_key = "observation.state"
        self._language_tokens_key = "observation.language.tokens"
        self._language_mask_key = "observation.language.attention_mask"
        self._default_task = "pick up the cube\n"

    @property
    def name(self) -> str:
        return "smolvla"

    def load(self, model_spec: dict[str, Any]) -> None:
        try:
            import numpy as np
            import torch
            from lerobot.configs.policies import PreTrainedConfig
            from lerobot.policies.smolvla.modeling_smolvla import SmolVLAPolicy
            from lerobot.utils.constants import (
                OBS_LANGUAGE_ATTENTION_MASK,
                OBS_LANGUAGE_TOKENS,
                OBS_STATE,
            )
            from transformers import AutoTokenizer
        except Exception as exc:
            raise BackendError(
                ERR_LOAD_FAILED,
                "SmolVLA dependencies are not importable",
                context=repr(exc),
            ) from exc

        validation_config = _read_validation_config(model_spec)
        model_id = _string_config(validation_config, "model_id", "lerobot/smolvla_base")
        cache_dir = _string_config(validation_config, "cache_dir", "/var/lib/tensorplate/hf-cache")
        device = _string_config(validation_config, "device", "cuda")
        num_steps = _optional_int_config(validation_config, "num_steps")
        self._default_task = _string_config(validation_config, "task", self._default_task)
        if not self._default_task.endswith("\n"):
            self._default_task = f"{self._default_task}\n"

        try:
            cfg = PreTrainedConfig.from_pretrained(model_id, cache_dir=cache_dir)
            cfg.device = device
            if num_steps is not None:
                cfg.num_steps = num_steps
            policy = SmolVLAPolicy.from_pretrained(model_id, config=cfg, cache_dir=cache_dir)
            tokenizer = AutoTokenizer.from_pretrained(
                cfg.vlm_model_name, cache_dir=cache_dir, padding_side="right"
            )
        except Exception as exc:
            raise BackendError(
                ERR_LOAD_FAILED, "failed to load SmolVLA policy", context=repr(exc)
            ) from exc

        self._torch = torch
        self._np = np
        self._policy = policy
        self._tokenizer = tokenizer
        self._config = cfg
        self._device = torch.device(cfg.device)
        self._obs_state_key = OBS_STATE
        self._language_tokens_key = OBS_LANGUAGE_TOKENS
        self._language_mask_key = OBS_LANGUAGE_ATTENTION_MASK
        self._loaded = True

    def prime(self) -> None:
        if not self._loaded:
            raise BackendError(ERR_NOT_READY, "SmolVLA backend not loaded")

    def infer(self, inputs: list[NamedTensor]) -> list[NamedTensor]:
        if not self._loaded or self._policy is None or self._config is None:
            raise BackendError(ERR_NOT_READY, "SmolVLA backend not loaded")
        try:
            batch = self._build_batch(inputs)
            with self._torch.inference_mode():
                actions = self._policy.predict_action_chunk(batch)
            if self._device.type == "cuda":
                self._torch.cuda.synchronize(self._device)
            actions = actions.detach().to("cpu", dtype=self._torch.float32).contiguous()
            payload = actions.numpy().tobytes(order="C")
        except BackendError:
            raise
        except Exception as exc:
            raise BackendError(
                ERR_INFERENCE_FAILED, "SmolVLA inference failed", context=repr(exc)
            ) from exc

        return [
            NamedTensor(
                name="action.chunk",
                tensor={"dtype": "float32", "shape": list(actions.shape)},
                payload=payload,
            )
        ]

    def infer_async(self, inputs: list[NamedTensor]) -> list[NamedTensor]:
        return self.infer(inputs)

    def cancel(self, request_id: str) -> None:
        _ = request_id

    def unload(self) -> None:
        self._policy = None
        self._config = None
        self._device = None
        self._loaded = False
        if self._torch is not None and self._torch.cuda.is_available():
            self._torch.cuda.empty_cache()

    def _build_batch(self, inputs: list[NamedTensor]) -> dict[str, Any]:
        by_name = {item.name: item for item in inputs}
        batch: dict[str, Any] = {}
        for image_key in self._config.image_features:
            tensor = by_name.get(image_key)
            if tensor is None:
                raise BackendError(ERR_SHAPE_MISMATCH, f"missing SmolVLA image input `{image_key}`")
            batch[image_key] = self._tensor_to_torch(tensor, expect_dtype={"float32", "uint8"})
            if batch[image_key].dtype == self._torch.uint8:
                batch[image_key] = batch[image_key].to(dtype=self._torch.float32) / 255.0
            else:
                batch[image_key] = batch[image_key].to(dtype=self._torch.float32)

        state = by_name.get(self._obs_state_key)
        if state is None:
            raise BackendError(
                ERR_SHAPE_MISMATCH,
                f"missing SmolVLA state input `{self._obs_state_key}`",
            )
        batch[self._obs_state_key] = self._tensor_to_torch(state, expect_dtype={"float32"}).to(
            dtype=self._torch.float32
        )

        tokens = by_name.get(self._language_tokens_key)
        mask = by_name.get(self._language_mask_key)
        if tokens is None or mask is None:
            encoded = self._tokenizer(
                [self._default_task],
                padding="max_length",
                max_length=self._config.tokenizer_max_length,
                return_tensors="pt",
            )
            batch[self._language_tokens_key] = encoded["input_ids"].to(self._device)
            batch[self._language_mask_key] = encoded["attention_mask"].to(
                device=self._device, dtype=self._torch.bool
            )
        else:
            batch[self._language_tokens_key] = self._tensor_to_torch(
                tokens, expect_dtype={"int64", "int32"}
            ).to(dtype=self._torch.int64)
            batch[self._language_mask_key] = self._tensor_to_torch(
                mask, expect_dtype={"bool", "uint8", "int32", "int64"}
            ).to(dtype=self._torch.bool)
        return batch

    def _tensor_to_torch(
        self, item: NamedTensor, *, expect_dtype: set[str]
    ) -> Any:  # noqa: ANN401 -- torch is imported lazily so return type stays Any
        dtype = item.tensor.get("dtype")
        shape = item.tensor.get("shape")
        if dtype not in expect_dtype:
            raise BackendError(ERR_SHAPE_MISMATCH, f"unexpected dtype for `{item.name}`: {dtype!r}")
        if not isinstance(shape, list) or not all(
            isinstance(dim, int) and dim > 0 for dim in shape
        ):
            raise BackendError(ERR_SHAPE_MISMATCH, f"invalid shape for `{item.name}`: {shape!r}")
        np_dtype = {
            "bool": self._np.bool_,
            "uint8": self._np.uint8,
            "int32": self._np.int32,
            "int64": self._np.int64,
            "float32": self._np.float32,
        }[dtype]
        arr = self._np.frombuffer(item.payload, dtype=np_dtype)
        expected = 1
        for dim in shape:
            expected *= dim
        if arr.size != expected:
            raise BackendError(
                ERR_SHAPE_MISMATCH,
                f"payload for `{item.name}` has {arr.size} elements; expected {expected}",
            )
        return self._torch.as_tensor(arr.reshape(shape), device=self._device)


__all__ = ["SmolVLABackend"]
