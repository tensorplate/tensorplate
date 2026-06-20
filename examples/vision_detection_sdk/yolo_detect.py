#!/usr/bin/env python3
"""Run YOLO object detection through the TensorPlate Python SDK.

User-space sample: it calls an already-deployed detector via the v0.1
serving ``/infer`` endpoint using ``tensorplate.VisionClient``. It is NOT
in-runtime ingest, a DeepStream sink, or a streaming API.

    pip install "tensorplate-python[vision]"
    python yolo_detect.py --serving-url http://127.0.0.1:18080 \
        --endpoint yolov8n --image frame.jpg --labels coco-labels.txt
"""

from __future__ import annotations

import argparse
import json
import sys

import tensorplate


def _load_labels(path: str | None) -> list[str] | None:
    if path is None:
        return None
    with open(path, encoding="utf-8") as handle:
        return [line.strip() for line in handle if line.strip()]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--image", required=True, help="Path to the input image.")
    parser.add_argument(
        "--endpoint", required=True, help="Deployed model / endpoint name."
    )
    parser.add_argument(
        "--serving-url",
        default=None,
        help="Serving URL; omit to resolve like `tensorplate infer`.",
    )
    parser.add_argument(
        "--input-size", type=int, default=640, help="Square model input size."
    )
    parser.add_argument("--score-threshold", type=float, default=0.25)
    parser.add_argument("--nms-threshold", type=float, default=0.45)
    parser.add_argument(
        "--transposed", action="store_true", help="Model output is [1, N, 4+C]."
    )
    parser.add_argument("--contract", default=tensorplate.YOLO_V8_SINGLE_OUTPUT)
    parser.add_argument(
        "--output-name", default=None, help="Detection output tensor name."
    )
    parser.add_argument(
        "--labels", default=None, help="Class-label file (one label per line)."
    )
    parser.add_argument("--json", action="store_true", help="Emit detections as JSON.")
    args = parser.parse_args(argv)

    client = tensorplate.VisionClient(
        args.serving_url, discover=args.serving_url is None
    )
    config = tensorplate.PreprocessConfig(input_size=(args.input_size, args.input_size))
    detections = client.detect(
        args.image,
        endpoint=args.endpoint,
        output_name=args.output_name,
        score_threshold=args.score_threshold,
        nms_threshold=args.nms_threshold,
        labels=_load_labels(args.labels),
        transposed=args.transposed,
        contract=args.contract,
        preprocess_config=config,
    )

    if args.json:
        print(
            json.dumps(
                [
                    {
                        "label": det.label,
                        "class_id": det.class_id,
                        "score": round(det.score, 4),
                        "box": [round(value, 2) for value in det.box],
                    }
                    for det in detections
                ]
            )
        )
    else:
        print(f"{len(detections)} detection(s) against {args.endpoint!r}:")
        for det in detections:
            name = det.label if det.label is not None else f"class {det.class_id}"
            x1, y1, x2, y2 = (round(value, 1) for value in det.box)
            print(f"  {name}: score={det.score:.3f} box=({x1}, {y1}, {x2}, {y2})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
