"""Detector output-decoding conventions and supported output contracts.

These names and identifiers are pure constants (no numpy) so the core
package imports without the optional vision dependencies.
"""

from __future__ import annotations


class detections:
    """Semantic-tag names for opportunistic detector-output decode.

    A serving worker that tags its output tensors with these names lets
    the SDK select the boxes/scores/classes tensors without the caller
    supplying explicit output-name mapping. Tag emission is best-effort;
    explicit mapping remains the primary, documented path.
    """

    boxes = "detections.boxes"
    scores = "detections.scores"
    classes = "detections.classes"


#: Supported built-in detector output contract: a single tensor shaped
#: ``[1, 4 + C, N]`` (YOLOv8-style; 4 box coords + C class scores over N
#: anchors), or its transpose ``[1, N, 4 + C]`` when the caller declares
#: it. Other heads (YOLOv5 objectness, exporter-side NMS, masks,
#: keypoints) remain application-side postprocessing in v0.1.3.
YOLO_V8_SINGLE_OUTPUT = "yolo_v8_single_output"
