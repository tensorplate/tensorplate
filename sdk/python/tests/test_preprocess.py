"""Tests for client-side image preprocessing."""

from __future__ import annotations

import pytest

from tensorplate.preprocess import LetterboxTransform, PreprocessConfig, preprocess

_TRANSFORM = LetterboxTransform(
    src_height=100,
    src_width=200,
    scale_x=3.2,
    scale_y=3.2,
    pad_x=0.0,
    pad_y=160.0,
    input_height=640,
    input_width=640,
)


def test_letterbox_transform_inverse_mapping() -> None:
    assert _TRANSFORM.map_box_to_source(0.0, 160.0, 640.0, 480.0) == pytest.approx(
        (0.0, 0.0, 200.0, 100.0)
    )


def test_letterbox_transform_clamps_to_source_bounds() -> None:
    x1, y1, x2, y2 = _TRANSFORM.map_box_to_source(-50.0, 0.0, 5000.0, 5000.0)
    assert (x1, y1) == (0.0, 0.0)
    assert x2 == pytest.approx(200.0)
    assert y2 == pytest.approx(100.0)


def test_preprocess_from_ndarray_letterboxes_to_nchw() -> None:
    pytest.importorskip("numpy")
    pytest.importorskip("PIL")
    import numpy

    image = numpy.zeros((100, 200, 3), dtype=numpy.uint8)
    tensor, transform = preprocess(image, PreprocessConfig(input_size=(640, 640)))

    assert tensor.dtype.value == "float32"
    assert tensor.shape == (1, 3, 640, 640)
    assert (transform.src_height, transform.src_width) == (100, 200)
    assert transform.scale_x == pytest.approx(3.2)
    assert transform.pad_y == pytest.approx(160.0)
    assert transform.map_box_to_source(0.0, 160.0, 640.0, 480.0) == pytest.approx(
        (0.0, 0.0, 200.0, 100.0)
    )


def test_preprocess_nhwc_layout_option() -> None:
    pytest.importorskip("numpy")
    pytest.importorskip("PIL")
    import numpy

    image = numpy.zeros((64, 64, 3), dtype=numpy.uint8)
    tensor, _ = preprocess(image, PreprocessConfig(input_size=(32, 32), channels_first=False))
    assert tensor.shape == (1, 32, 32, 3)


def test_preprocess_rejects_bad_channel_order() -> None:
    pytest.importorskip("numpy")
    import numpy

    image = numpy.zeros((8, 8, 3), dtype=numpy.uint8)
    with pytest.raises(ValueError, match="channel_order"):
        preprocess(image, PreprocessConfig(channel_order="rbg"))


def test_preprocess_rejects_non_uint8_ndarray() -> None:
    pytest.importorskip("numpy")
    import numpy

    image = numpy.zeros((8, 8, 3), dtype=numpy.float32)
    with pytest.raises(ValueError, match="uint8"):
        preprocess(image)
