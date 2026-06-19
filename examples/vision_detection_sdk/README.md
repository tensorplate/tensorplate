# `examples/vision_detection_sdk/`

Runnable, **user-space** samples that call an already-deployed TensorPlate
detector through the first-party Python SDK (`tensorplate.VisionClient`)
over the v0.1 serving `/infer` endpoint.

> These are sample / learning programs, **not** part of the supported
> runtime surface. Neither the SDK nor these examples provide in-runtime
> camera/video ingest, a DeepStream sink, or a streaming session API —
> those are separate, deferred runtime capabilities. The camera sample
> captures frames in *your* process and calls the SDK once per frame.

## Install

These samples need the `[vision]` extra (numpy + Pillow) plus OpenCV for the
camera sample:

```bash
pip install "tensorplate-python[vision]"   # numpy + Pillow (image decode + arrays)
pip install opencv-python                   # only for camera_infer.py
```

## `yolo_detect.py` — detect on a single image

```bash
python yolo_detect.py --serving-url http://127.0.0.1:18080 \
    --endpoint yolov8n --image frame.jpg --labels coco-labels.txt --json
```

Omit `--serving-url` to resolve the endpoint exactly as `tensorplate infer`
does (explicit URL → CLI profile → agent discovery → loopback default).
Pass `--transposed` if your detector emits `[1, N, 4 + C]` instead of
`[1, 4 + C, N]`, and `--output-name` to pick a specific output tensor.

## `camera_infer.py` — per-frame `camera → SDK → /infer` (sample)

```bash
python camera_infer.py --serving-url http://127.0.0.1:18080 \
    --endpoint yolov8n --source 0 --max-frames 100
```

`--source` is a camera index (e.g. `0`) or a video file path. This is
reference sample code for the v0.1.3 learning loop — capture happens
in-process and each frame is sent as one synchronous request. It is not a
production real-time perception pipeline.
