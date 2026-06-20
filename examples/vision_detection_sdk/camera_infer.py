#!/usr/bin/env python3
"""Reference ``camera -> SDK -> /infer`` loop (user-space SAMPLE).

Captures frames from a camera or video file in THIS process with OpenCV
and runs detection on each frame via ``tensorplate.VisionClient``.

This is sample / learning code, NOT supported in-runtime ingest, a
DeepStream sink, or a production streaming contract — those are separate,
deferred runtime capabilities. Frame capture happens in your process; the
SDK only makes one synchronous ``/infer`` call per frame.

    pip install "tensorplate-python[vision]" opencv-python
    python camera_infer.py --serving-url http://127.0.0.1:18080 \
        --endpoint yolov8n --source 0 --max-frames 100
"""

from __future__ import annotations

import argparse
import sys


def _require_cv2() -> object:
    try:
        import cv2
    except ModuleNotFoundError:
        sys.stderr.write("this sample needs OpenCV: pip install opencv-python\n")
        raise SystemExit(2) from None
    return cv2


def run(args: argparse.Namespace) -> int:
    import tensorplate

    cv2 = _require_cv2()
    source: object = int(args.source) if args.source.isdigit() else args.source
    capture = cv2.VideoCapture(source)
    if not capture.isOpened():
        sys.stderr.write(f"could not open video source {args.source!r}\n")
        return 1

    client = tensorplate.VisionClient(
        args.serving_url, discover=args.serving_url is None
    )
    config = tensorplate.PreprocessConfig(input_size=(args.input_size, args.input_size))
    processed = 0
    try:
        while args.max_frames <= 0 or processed < args.max_frames:
            ok, frame_bgr = capture.read()
            if not ok:
                break
            frame_rgb = cv2.cvtColor(frame_bgr, cv2.COLOR_BGR2RGB)
            detections = client.detect(
                frame_rgb,
                endpoint=args.endpoint,
                score_threshold=args.score_threshold,
                transposed=args.transposed,
                contract=args.contract,
                preprocess_config=config,
            )
            processed += 1
            print(f"frame {processed}: {len(detections)} detection(s)")
    finally:
        capture.release()
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument(
        "--endpoint", required=True, help="Deployed model / endpoint name."
    )
    parser.add_argument(
        "--serving-url", default=None, help="Serving URL; omit to resolve like the CLI."
    )
    parser.add_argument(
        "--source", default="0", help="Camera index (e.g. 0) or video file path."
    )
    parser.add_argument(
        "--max-frames",
        type=int,
        default=100,
        help="Stop after N frames (<= 0 means unbounded).",
    )
    parser.add_argument(
        "--input-size", type=int, default=640, help="Square model input size."
    )
    parser.add_argument("--score-threshold", type=float, default=0.25)
    parser.add_argument(
        "--transposed", action="store_true", help="Model output is [1, N, 4+C]."
    )
    parser.add_argument("--contract", default="yolo_v8_single_output")
    return run(parser.parse_args(argv))


if __name__ == "__main__":
    sys.exit(main())
