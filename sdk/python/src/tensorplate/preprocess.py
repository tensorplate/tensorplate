"""Client-side image preprocessing for detector models.

Decodes an image (path, bytes, or HWC uint8 ndarray), letterboxes it to
the model's input size, normalizes, orders channels, and lays it out as an
NCHW float32 input tensor. A :class:`LetterboxTransform` records the scale
and padding so detections map back to source-image pixels.

numpy and Pillow (the ``tensorplate-python[vision]`` extra) are imported
lazily so the core package stays importable without them.
"""

from __future__ import annotations

import io
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING

from tensorplate.tensors import DType, Layout, TensorInput

if TYPE_CHECKING:
    import numpy as np


def _clamp(value: float, low: float, high: float) -> float:
    return max(low, min(high, value))


@dataclass(frozen=True)
class LetterboxTransform:
    """Maps model-input pixel coordinates back to source-image pixels."""

    src_height: int
    src_width: int
    scale_x: float
    scale_y: float
    pad_x: float
    pad_y: float
    input_height: int
    input_width: int

    def map_box_to_source(
        self, x1: float, y1: float, x2: float, y2: float
    ) -> tuple[float, float, float, float]:
        """Map a box from model-input pixels to source pixels (clamped to bounds)."""
        return (
            _clamp((x1 - self.pad_x) / self.scale_x, 0.0, float(self.src_width)),
            _clamp((y1 - self.pad_y) / self.scale_y, 0.0, float(self.src_height)),
            _clamp((x2 - self.pad_x) / self.scale_x, 0.0, float(self.src_width)),
            _clamp((y2 - self.pad_y) / self.scale_y, 0.0, float(self.src_height)),
        )


@dataclass(frozen=True)
class PreprocessConfig:
    """Preprocessing options. Defaults target YOLO-style NCHW float32 input."""

    input_size: tuple[int, int] = (640, 640)  # (height, width)
    letterbox: bool = True
    channel_order: str = "rgb"  # "rgb" or "bgr"
    channels_first: bool = True  # NCHW when True, else NHWC
    scale: float = 1.0 / 255.0
    mean: tuple[float, ...] | None = None
    std: tuple[float, ...] | None = None
    dtype: DType = DType.FLOAT32
    pad_value: int = 114
    input_name: str = "images"


def preprocess(
    image: str | bytes | Path | np.ndarray,
    config: PreprocessConfig | None = None,
) -> tuple[TensorInput, LetterboxTransform]:
    """Preprocess ``image`` into a model input tensor and a back-mapping transform.

    ``image`` may be a filesystem path, encoded image bytes, or an HWC
    uint8 ndarray (assumed RGB). Decoding path/bytes requires Pillow.
    """
    import numpy as np

    cfg = config if config is not None else PreprocessConfig()
    if cfg.channel_order not in ("rgb", "bgr"):
        raise ValueError(f"channel_order must be 'rgb' or 'bgr', got {cfg.channel_order!r}")

    source = _decode_to_hwc_rgb_uint8(image)
    input_height, input_width = cfg.input_size
    resized, scale_x, scale_y, pad_x, pad_y = _resize(source, cfg)
    transform = LetterboxTransform(
        src_height=int(source.shape[0]),
        src_width=int(source.shape[1]),
        scale_x=scale_x,
        scale_y=scale_y,
        pad_x=pad_x,
        pad_y=pad_y,
        input_height=input_height,
        input_width=input_width,
    )

    array = resized.astype("float32")
    if cfg.channel_order == "bgr":
        array = array[:, :, ::-1]
    array = array * cfg.scale
    if cfg.mean is not None:
        array = array - np.asarray(cfg.mean, dtype="float32")
    if cfg.std is not None:
        array = array / np.asarray(cfg.std, dtype="float32")
    if cfg.channels_first:
        array = np.transpose(array, (2, 0, 1))
    array = array[np.newaxis, ...]

    tensor = TensorInput.from_numpy(cfg.input_name, array, dtype=cfg.dtype, layout=Layout.ROW_MAJOR)
    return tensor, transform


def _decode_to_hwc_rgb_uint8(image: str | bytes | Path | np.ndarray) -> np.ndarray:
    if isinstance(image, (str, Path)):
        return _decode_pil(image)
    if isinstance(image, (bytes, bytearray)):
        return _decode_pil(io.BytesIO(bytes(image)))
    import numpy as np

    array = np.asarray(image)
    if array.ndim != 3 or array.shape[2] != 3:
        raise ValueError(
            f"ndarray image must be HWC with 3 channels, got shape {tuple(array.shape)}"
        )
    if array.dtype != np.uint8:
        raise ValueError(f"ndarray image must have dtype uint8, got {array.dtype}")
    return np.ascontiguousarray(array)


def _decode_pil(source: str | Path | io.BytesIO) -> np.ndarray:
    try:
        from PIL import Image
    except ModuleNotFoundError as exc:  # pragma: no cover - exercised only without Pillow
        raise RuntimeError(
            "image decoding requires Pillow; install `tensorplate-python[vision]`"
        ) from exc
    import numpy as np

    with Image.open(source) as handle:
        return np.asarray(handle.convert("RGB"))


def _resize(
    source: np.ndarray, cfg: PreprocessConfig
) -> tuple[np.ndarray, float, float, float, float]:
    import numpy as np
    from PIL import Image

    src_height, src_width = int(source.shape[0]), int(source.shape[1])
    input_height, input_width = cfg.input_size
    pil = Image.fromarray(source)
    if cfg.letterbox:
        scale = min(input_width / src_width, input_height / src_height)
        new_width = max(1, round(src_width * scale))
        new_height = max(1, round(src_height * scale))
        resized = np.asarray(pil.resize((new_width, new_height), Image.Resampling.BILINEAR))
        canvas = np.full((input_height, input_width, 3), cfg.pad_value, dtype="uint8")
        pad_x = (input_width - new_width) // 2
        pad_y = (input_height - new_height) // 2
        canvas[pad_y : pad_y + new_height, pad_x : pad_x + new_width] = resized
        return canvas, float(scale), float(scale), float(pad_x), float(pad_y)
    resized = np.asarray(pil.resize((input_width, input_height), Image.Resampling.BILINEAR))
    return resized, input_width / src_width, input_height / src_height, 0.0, 0.0
