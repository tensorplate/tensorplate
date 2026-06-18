"""The core package and vision modules import without numpy / Pillow.

This test must run in an environment WITHOUT the vision extras installed
(as the default CI Python gate does): importing the package may not pull
numpy or Pillow at module-import time — only calling the vision helpers may.
"""

from __future__ import annotations

import tensorplate


def test_vision_surface_imports_without_numpy() -> None:
    # Reaching this line means `import tensorplate` (which imports the
    # preprocess/postprocess/conventions modules) succeeded with no numpy.
    assert tensorplate.detections.boxes == "detections.boxes"
    assert tensorplate.detections.scores == "detections.scores"
    assert tensorplate.detections.classes == "detections.classes"
    assert tensorplate.YOLO_V8_SINGLE_OUTPUT == "yolo_v8_single_output"
    for name in (
        "Detection",
        "LetterboxTransform",
        "PreprocessConfig",
        "decode_detections",
        "preprocess",
    ):
        assert name in tensorplate.__all__
        assert hasattr(tensorplate, name)
