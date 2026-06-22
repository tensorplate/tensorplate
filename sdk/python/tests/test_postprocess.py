"""Tests for YOLO-style detector output postprocessing."""

from __future__ import annotations

import pytest

from tensorplate.conventions import YOLO26_E2E_DETECTIONS
from tensorplate.errors import ProtocolError
from tensorplate.postprocess import decode_detections
from tensorplate.preprocess import LetterboxTransform
from tensorplate.tensors import DType, TensorOutput

_IDENTITY = LetterboxTransform(
    src_height=100,
    src_width=100,
    scale_x=1.0,
    scale_y=1.0,
    pad_x=0.0,
    pad_y=0.0,
    input_height=100,
    input_width=100,
)


def _yolo_output(*, transposed: bool = False) -> TensorOutput:
    import numpy

    classes, anchors = 2, 3
    grid = numpy.zeros((4 + classes, anchors), dtype=numpy.float32)
    grid[:, 0] = [50, 50, 20, 20, 0.1, 0.9]  # strong, class 1
    grid[:, 1] = [52, 52, 20, 20, 0.1, 0.8]  # overlaps anchor 0 -> NMS-suppressed
    grid[:, 2] = [10, 10, 5, 5, 0.05, 0.05]  # below score threshold
    if transposed:
        return TensorOutput("det", DType.FLOAT32, (1, anchors, 4 + classes), grid.T[None].tobytes())
    return TensorOutput("det", DType.FLOAT32, (1, 4 + classes, anchors), grid[None].tobytes())


def test_decode_basic_with_nms_and_labels() -> None:
    pytest.importorskip("numpy")
    detections = decode_detections(
        _yolo_output(), _IDENTITY, score_threshold=0.25, nms_threshold=0.5, labels=["a", "b"]
    )
    assert len(detections) == 1
    det = detections[0]
    assert det.class_id == 1
    assert det.label == "b"
    assert det.score == pytest.approx(0.9)
    assert det.box == pytest.approx((40.0, 40.0, 60.0, 60.0))


def test_decode_transposed_layout_matches_default() -> None:
    pytest.importorskip("numpy")
    default = decode_detections(_yolo_output(), _IDENTITY)
    transposed = decode_detections(_yolo_output(transposed=True), _IDENTITY, transposed=True)
    assert len(default) == len(transposed) == 1
    assert default[0].box == pytest.approx(transposed[0].box)


def test_decode_rejects_declared_layout_mismatch() -> None:
    pytest.importorskip("numpy")
    import numpy

    classes, anchors = 2, 8
    grid = numpy.zeros((4 + classes, anchors), dtype=numpy.float32)
    default = TensorOutput("det", DType.FLOAT32, (1, 4 + classes, anchors), grid[None].tobytes())
    transposed = TensorOutput(
        "det", DType.FLOAT32, (1, anchors, 4 + classes), grid.T[None].tobytes()
    )

    with pytest.raises(ProtocolError, match="transposed=True"):
        decode_detections(transposed, _IDENTITY)
    with pytest.raises(ProtocolError, match="remove transposed=True"):
        decode_detections(default, _IDENTITY, transposed=True)


def test_decode_maps_boxes_through_transform() -> None:
    pytest.importorskip("numpy")
    transform = LetterboxTransform(
        src_height=100,
        src_width=200,
        scale_x=2.0,
        scale_y=2.0,
        pad_x=0.0,
        pad_y=0.0,
        input_height=400,
        input_width=400,
    )
    det = decode_detections(_yolo_output(), transform, score_threshold=0.25)[0]
    assert det.box == pytest.approx((20.0, 20.0, 30.0, 30.0))


def test_decode_rejects_unsupported_contract() -> None:
    output = TensorOutput("det", DType.FLOAT32, (1, 6, 3), b"\x00" * (6 * 3 * 4))
    with pytest.raises(ValueError, match="contract"):
        decode_detections(output, _IDENTITY, contract="custom_head")


def test_decode_rejects_too_few_channels() -> None:
    pytest.importorskip("numpy")
    import numpy

    grid = numpy.zeros((1, 3, 4), dtype=numpy.float32)  # 3 channels < 5
    output = TensorOutput("det", DType.FLOAT32, (1, 3, 4), grid.tobytes())
    with pytest.raises(ProtocolError):
        decode_detections(output, _IDENTITY)


def test_decode_yolo26_e2e_filters_maps_labels_and_preserves_overlaps() -> None:
    pytest.importorskip("numpy")
    import numpy

    transform = LetterboxTransform(
        src_height=90,
        src_width=160,
        scale_x=2.0,
        scale_y=2.0,
        pad_x=0.0,
        pad_y=70.0,
        input_height=320,
        input_width=320,
    )
    rows = numpy.zeros((1, 300, 6), dtype=numpy.float32)
    rows[0, 0] = [120, 120, 200, 200, 0.9, 1]
    rows[0, 1] = [
        122,
        122,
        202,
        202,
        0.8,
        1,
    ]  # overlapping box, kept because YOLO26 E2E is NMS-free
    rows[0, 2] = [10, 10, 20, 20, 0.1, 0]
    output = TensorOutput("det", DType.FLOAT32, (1, 300, 6), rows.tobytes())

    detections = decode_detections(
        output,
        transform,
        score_threshold=0.25,
        nms_threshold=0.0,
        labels=["a", "b"],
        contract=YOLO26_E2E_DETECTIONS,
    )

    assert len(detections) == 2
    assert detections[0].class_id == 1
    assert detections[0].label == "b"
    assert detections[0].score == pytest.approx(0.9)
    assert detections[0].box == pytest.approx((60.0, 25.0, 100.0, 65.0))
    assert detections[1].score == pytest.approx(0.8)


@pytest.mark.parametrize(
    "shape",
    [
        (1, 6, 300),
        (2, 300, 6),
        (1, 301, 6),
    ],
)
def test_decode_yolo26_e2e_rejects_invalid_shapes(shape: tuple[int, ...]) -> None:
    pytest.importorskip("numpy")
    import numpy

    rows = numpy.zeros(shape, dtype=numpy.float32)
    output = TensorOutput("det", DType.FLOAT32, shape, rows.tobytes())
    with pytest.raises(ProtocolError):
        decode_detections(output, _IDENTITY, contract=YOLO26_E2E_DETECTIONS)


def test_decode_yolo26_e2e_rejects_non_float_dtype() -> None:
    pytest.importorskip("numpy")
    output = TensorOutput("det", DType.INT32, (1, 300, 6), b"\x00" * (300 * 6 * 4))
    with pytest.raises(ProtocolError, match="float16 or float32"):
        decode_detections(output, _IDENTITY, contract=YOLO26_E2E_DETECTIONS)


def test_decode_yolo26_e2e_drops_nonfinite_and_negative_rows() -> None:
    pytest.importorskip("numpy")
    import numpy

    rows = numpy.zeros((1, 300, 6), dtype=numpy.float32)
    rows[0, 0] = [10, 10, 20, 20, 0.9, 0]  # valid -> kept
    rows[0, 1] = [10, 10, 20, 20, numpy.nan, 0]  # NaN score -> dropped, no raise
    rows[0, 2] = [10, 10, 20, 20, 0.8, numpy.nan]  # NaN class id -> dropped, no raise
    rows[0, 3] = [10, 10, 20, 20, 0.7, -1]  # negative class id -> dropped
    output = TensorOutput("det", DType.FLOAT32, (1, 300, 6), rows.tobytes())

    detections = decode_detections(
        output, _IDENTITY, score_threshold=0.25, contract=YOLO26_E2E_DETECTIONS
    )

    assert len(detections) == 1
    assert detections[0].class_id == 0
    assert detections[0].score == pytest.approx(0.9)
