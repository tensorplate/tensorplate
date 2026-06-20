"""YOLO-style detector output decoding, NMS, and the ``Detection`` type.

``Detection`` and the module's imports are numpy-free so the core package
imports without the vision extras; the decode/NMS routines import numpy
lazily.
"""

from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass
from typing import TYPE_CHECKING

from tensorplate.conventions import YOLO26_E2E_DETECTIONS, YOLO_V8_SINGLE_OUTPUT
from tensorplate.errors import ProtocolError
from tensorplate.tensors import DType, TensorOutput

if TYPE_CHECKING:
    import numpy as np

    from tensorplate.preprocess import LetterboxTransform


@dataclass(frozen=True)
class Detection:
    """A single object detection in source-image pixel space."""

    class_id: int
    score: float
    box: tuple[float, float, float, float]  # (x1, y1, x2, y2)
    label: str | None = None


def decode_detections(
    output: TensorOutput,
    transform: LetterboxTransform,
    *,
    score_threshold: float = 0.25,
    nms_threshold: float = 0.45,
    labels: Sequence[str] | None = None,
    transposed: bool = False,
    contract: str = YOLO_V8_SINGLE_OUTPUT,
) -> list[Detection]:
    """Decode a supported YOLO-style output tensor into source-pixel detections.

    ``yolo_v8_single_output`` expects ``[1, 4 + C, N]`` (or
    ``[1, N, 4 + C]`` when ``transposed``): 4 center-format box coordinates
    followed by C per-class scores. It applies score thresholding and
    class-aware NMS.

    ``yolo26_e2e_detections`` expects NMS-free ``[1, K, 6]`` with columns
    x1, y1, x2, y2, score, class_id. It applies only score thresholding.
    """
    if contract == YOLO26_E2E_DETECTIONS:
        return _decode_yolo26_e2e(output, transform, score_threshold=score_threshold, labels=labels)
    if contract != YOLO_V8_SINGLE_OUTPUT:
        raise ValueError(
            f"unsupported output contract {contract!r}; supported contracts are "
            f"{YOLO_V8_SINGLE_OUTPUT!r} and {YOLO26_E2E_DETECTIONS!r}"
        )
    import numpy as np

    array = output.to_numpy().astype("float32")
    if array.ndim != 3 or array.shape[0] != 1:
        raise ProtocolError(
            f"detector output must be [1, *, *] for {contract!r}, got shape {tuple(array.shape)}"
        )
    if _looks_like_wrong_layout(array, transposed):
        expected = "[1, N, 4 + C]" if transposed else "[1, 4 + C, N]"
        hint = "remove transposed=True" if transposed else "pass transposed=True"
        raise ProtocolError(
            f"detector output shape {tuple(array.shape)} does not match declared layout "
            f"{expected}; {hint} if the tensor uses the opposite layout"
        )
    grid = array[0].T if transposed else array[0]
    if grid.shape[0] < 5:
        raise ProtocolError(
            f"detector output has {int(grid.shape[0])} channels; need 4 box + >=1 class score "
            "(is the layout transposed? pass transposed=True)"
        )

    boxes = grid[:4, :]
    class_scores = grid[4:, :]
    scores = class_scores.max(axis=0)
    class_ids = class_scores.argmax(axis=0)
    keep = scores >= score_threshold
    if not bool(keep.any()):
        return []

    cx, cy, w, h = boxes[0, keep], boxes[1, keep], boxes[2, keep], boxes[3, keep]
    xyxy = np.stack([cx - w / 2, cy - h / 2, cx + w / 2, cy + h / 2], axis=1)
    kept_scores = scores[keep]
    kept_classes = class_ids[keep]

    detections: list[Detection] = []
    for idx in _class_aware_nms(xyxy, kept_scores, kept_classes, nms_threshold):
        box = transform.map_box_to_source(
            float(xyxy[idx, 0]), float(xyxy[idx, 1]), float(xyxy[idx, 2]), float(xyxy[idx, 3])
        )
        class_id = int(kept_classes[idx])
        label = labels[class_id] if labels is not None and 0 <= class_id < len(labels) else None
        detections.append(
            Detection(class_id=class_id, score=float(kept_scores[idx]), box=box, label=label)
        )
    return detections


def _decode_yolo26_e2e(
    output: TensorOutput,
    transform: LetterboxTransform,
    *,
    score_threshold: float,
    labels: Sequence[str] | None,
) -> list[Detection]:
    if output.dtype not in (DType.FLOAT16, DType.FLOAT32):
        raise ProtocolError(
            f"detector output for {YOLO26_E2E_DETECTIONS!r} must be float16 or float32, "
            f"got {output.dtype.value!r}"
        )
    try:
        array = output.to_numpy().astype("float32")
    except (RuntimeError, ValueError) as exc:
        raise ProtocolError(
            f"could not convert detector output for {YOLO26_E2E_DETECTIONS!r} to float32"
        ) from exc
    if array.ndim != 3 or array.shape[0] != 1 or array.shape[2] != 6:
        raise ProtocolError(
            f"detector output must be [1, K, 6] for {YOLO26_E2E_DETECTIONS!r}, "
            f"got shape {tuple(array.shape)}"
        )
    if array.shape[1] > 300:
        raise ProtocolError(
            f"detector output has {int(array.shape[1])} rows for {YOLO26_E2E_DETECTIONS!r}; "
            "expected K <= 300"
        )

    detections: list[Detection] = []
    for row in array[0]:
        score = float(row[4])
        if score < score_threshold:
            continue
        class_id = int(row[5])
        box = transform.map_box_to_source(
            float(row[0]), float(row[1]), float(row[2]), float(row[3])
        )
        label = labels[class_id] if labels is not None and 0 <= class_id < len(labels) else None
        detections.append(Detection(class_id=class_id, score=score, box=box, label=label))
    return detections


def _looks_like_wrong_layout(array: np.ndarray, transposed: bool) -> bool:
    """Return true for the common unambiguous YOLO transpose mismatch."""
    rows = int(array.shape[1])
    cols = int(array.shape[2])
    if transposed:
        return cols > rows and rows >= 5
    return rows > cols and cols >= 5


def _class_aware_nms(
    boxes: np.ndarray, scores: np.ndarray, class_ids: np.ndarray, threshold: float
) -> list[int]:
    import numpy as np

    selected: list[int] = []
    for cls in np.unique(class_ids):
        members = np.nonzero(class_ids == cls)[0]
        order = members[np.argsort(scores[members])[::-1]]
        while order.size > 0:
            best = int(order[0])
            selected.append(best)
            if order.size == 1:
                break
            rest = order[1:]
            order = rest[_iou(boxes[best], boxes[rest]) <= threshold]
    return selected


def _iou(box: np.ndarray, others: np.ndarray) -> np.ndarray:
    import numpy as np

    x1 = np.maximum(box[0], others[:, 0])
    y1 = np.maximum(box[1], others[:, 1])
    x2 = np.minimum(box[2], others[:, 2])
    y2 = np.minimum(box[3], others[:, 3])
    inter = np.clip(x2 - x1, 0.0, None) * np.clip(y2 - y1, 0.0, None)
    area_box = max(0.0, float(box[2] - box[0])) * max(0.0, float(box[3] - box[1]))
    area_others = np.clip(others[:, 2] - others[:, 0], 0.0, None) * np.clip(
        others[:, 3] - others[:, 1], 0.0, None
    )
    return np.asarray(inter / np.maximum(area_box + area_others - inter, 1e-9))
