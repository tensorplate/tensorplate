# Detection with the TensorPlate SDK

`VisionClient.detect` composes three steps — client-side preprocessing,
`ServingClient.infer`, and detector postprocessing — into one call that
turns an image into a list of `Detection`s in source-image pixels. This
page documents the preprocessing options, the output contract it decodes,
and the postprocessing options. See [python.md](./python.md) for install
and the client API.

> The preprocessing and postprocessing helpers are a **client-side
> convenience** for calling a detector. They are not an in-runtime pre/post
> contract, and they require the `[vision]` extra (numpy + Pillow).

## Preprocessing

`detect` accepts an image as a path, `bytes`, `Path`, or an HWC `uint8`
ndarray, and prepares the input tensor with `PreprocessConfig`:

| Option | Default | Meaning |
| --- | --- | --- |
| `input_size` | `(640, 640)` | Target `(height, width)`. |
| `letterbox` | `True` | Resize preserving aspect ratio and pad; the pad/scale is recorded for source-pixel box mapping. |
| `channel_order` | `"rgb"` | Channel order of the input tensor. |
| `channels_first` | `True` | Emit NCHW (vs NHWC). |
| `scale` | `1/255` | Per-pixel scale applied after type conversion. |
| `mean` / `std` | `None` | Optional per-channel normalization. |
| `dtype` | `float32` | Input tensor dtype. |
| `pad_value` | `114` | Letterbox pad value. |
| `input_name` | `"images"` | Name of the produced input tensor. |

`detect` builds `PreprocessConfig(input_name=input_name)` for you; pass
`preprocess_config=` to override any of the above. The standalone
`preprocess(image, config)` returns the `TensorInput` and the
`LetterboxTransform` it used to map boxes back to source pixels.

## Output contracts

The `contract=` option selects the built-in detector output decoder.
`yolo_v8_single_output` remains the default for backward compatibility.

### `yolo_v8_single_output`

This contract expects a single output tensor shaped:

- `[1, 4 + C, N]` — the YOLOv8 layout (4 box coordinates + `C` class scores,
  across `N` candidates); or
- `[1, N, 4 + C]` — the transposed layout. Pass `transposed=True` when your
  detector emits this.

Box coordinates are center-x / center-y / width / height in the letterboxed
input space; postprocessing converts them to `(x1, y1, x2, y2)` and maps
them back to source pixels with the `LetterboxTransform`. Class-aware NMS is
applied with `nms_threshold`.

### `yolo26_e2e_detections`

This contract targets the default Ultralytics YOLO26 one-to-one / end-to-end
head. It expects one NMS-free output tensor shaped `[1, K, 6]`, with `K <= 300`
and columns:

```text
x1, y1, x2, y2, score, class_id
```

The box coordinates are already corner coordinates in the letterboxed input
space. Postprocessing filters by `score_threshold`, maps boxes back to source
pixels, and does **not** run NMS. `transposed` and `nms_threshold` are ignored
for this contract.

## Selecting the output tensor

When a response has more than one output, `detect` chooses the detection
tensor in this order:

1. `output_name=` if you pass it (explicit, and the recommended path);
2. the only output, when there is exactly one;
3. the single output whose `semantic_tag` is one of `detections.boxes`,
   `detections.scores`, or `detections.classes`.

If none of these resolve, `detect` raises `ProtocolError` asking you to
pass `output_name`.

> The v0.1 serving worker does not currently emit a `semantic_tag` from
> bundle metadata, so tag-based auto-selection is best-effort. **Explicit
> `output_name` is the primary, documented path** for multi-output models.

The `detections.*` tag names are exported as constants:

```python
from tensorplate import detections

detections.boxes    # "detections.boxes"
detections.scores   # "detections.scores"
detections.classes  # "detections.classes"
```

## Postprocessing options

| Option | Default | Meaning |
| --- | --- | --- |
| `score_threshold` | `0.25` | Drop detections below this confidence. |
| `nms_threshold` | `0.45` | IoU threshold for class-aware NMS on `yolo_v8_single_output`; ignored for `yolo26_e2e_detections`. |
| `labels` | `None` | Class names; when given, `Detection.label` is set from `labels[class_id]`. |
| `transposed` | `False` | Set for `[1, N, 4 + C]` YOLOv8 outputs; ignored for `yolo26_e2e_detections`. |
| `contract` | `"yolo_v8_single_output"` | Output contract to decode. |

The lower-level `decode_detections(output, transform, *, score_threshold,
nms_threshold, labels, transposed, contract)` is exported for callers that
run `ServingClient.infer` themselves and decode the result.

## Example

```python
from tensorplate import YOLO26_E2E_DETECTIONS, VisionClient

vc = VisionClient("http://127.0.0.1:18080")
labels = ["person", "bicycle", "car"]  # ...COCO etc.
for d in vc.detect("frame.jpg", endpoint="yolov8n", labels=labels, score_threshold=0.3):
    print(d.label, d.score, d.box)

for d in vc.detect(
    "frame.jpg",
    endpoint="yolo26n",
    labels=labels,
    contract=YOLO26_E2E_DETECTIONS,
):
    print(d.label, d.score, d.box)
```
