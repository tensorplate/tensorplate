"""The core package and vision modules import without numpy / Pillow.

This test must run in an environment WITHOUT the vision extras installed
(as the default CI Python gate does): importing the package may not pull
numpy or Pillow at module-import time — only calling the vision helpers may.
Importing it may not load the speech or stream packages or grpc either, installed or not.
"""

from __future__ import annotations

import subprocess
import sys

import tensorplate


def test_vision_surface_imports_without_numpy() -> None:
    # Reaching this line means `import tensorplate` (which imports the
    # preprocess/postprocess/conventions modules) succeeded with no numpy.
    assert tensorplate.detections.boxes == "detections.boxes"
    assert tensorplate.detections.scores == "detections.scores"
    assert tensorplate.detections.classes == "detections.classes"
    assert tensorplate.YOLO_V8_SINGLE_OUTPUT == "yolo_v8_single_output"
    assert tensorplate.YOLO26_E2E_DETECTIONS == "yolo26_e2e_detections"
    for name in (
        "Detection",
        "LetterboxTransform",
        "PreprocessConfig",
        "YOLO26_E2E_DETECTIONS",
        "decode_detections",
        "preprocess",
    ):
        assert name in tensorplate.__all__
        assert hasattr(tensorplate, name)


def test_core_import_loads_no_speech_or_stream_package_and_none_of_their_dependencies() -> None:
    probe = (
        "import sys, tensorplate; "
        "print(sorted(m for m in sys.modules if m.split('.')[0] in ('grpc', 'google') "
        "or m.startswith(('tensorplate.speech', 'tensorplate.stream', 'tensorplate._extras'))))"
    )
    result = subprocess.run(
        [sys.executable, "-c", probe], capture_output=True, text=True, check=True
    )
    assert result.stdout.strip() == "[]"
