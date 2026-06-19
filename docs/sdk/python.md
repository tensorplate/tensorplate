# TensorPlate Python SDK

`tensorplate-python` (import package `tensorplate`) is the first-party
Python SDK for calling models you have **already deployed** with
TensorPlate, over the v0.1 serving `/infer` HTTP envelope. It is a
client-side library: it does not deploy, run, or change runtime serving,
and it provides no in-runtime camera/video ingest, DeepStream sink, or
streaming session API. Those remain separate, deferred runtime
capabilities (see [Scope and non-goals](#scope-and-non-goals)).

- **Distribution:** `tensorplate-python` (Apache-2.0, Python 3.10+)
- **Import package:** `tensorplate`
- **Wire contract:** the v0.1 serving envelope, `schema_version` `0.1`
  ([`protocol/schemas/serving_http_envelope.json`](../../protocol/schemas/serving_http_envelope.json))

## Install

```bash
pip install tensorplate-python            # core client (no third-party deps)
pip install "tensorplate-python[numpy]"   # + numpy for tensor array access
pip install "tensorplate-python[vision]"  # + numpy & Pillow for VisionClient.detect
```

`import tensorplate` and constructing a `ServingClient` with raw-bytes
tensors never import numpy or Pillow; the optional dependencies load lazily
only when you call an array or vision helper.

### Verified / air-gapped install

The same wheel + sdist are attached to each GitHub Release, cosign-signed and
covered by the release `SHA256SUMS`. Use this path to verify provenance or
install offline. Download the wheel and signature material:

```bash
export TP_VERSION=0.1.3
export TP_TAG="v${TP_VERSION}"
export TP_REPO=tensorplate/tensorplate
export TP_RELEASE_URL="https://github.com/${TP_REPO}/releases/download/${TP_TAG}"

curl -fL -O "${TP_RELEASE_URL}/tensorplate_python-${TP_VERSION}-py3-none-any.whl"
curl -fL -O "${TP_RELEASE_URL}/SHA256SUMS"
curl -fL -O "${TP_RELEASE_URL}/SHA256SUMS.cosign.bundle"
```

Verify the cosign signature over `SHA256SUMS` (authenticity) and then the
checksum (integrity) before installing — the same flow as the runtime
[external install](../install/external-install.md#verify-signature-and-checksums)
and [`SECURITY.md`](../../SECURITY.md):

```bash
cosign verify-blob \
  --bundle SHA256SUMS.cosign.bundle \
  --certificate-identity-regexp "^https://github.com/${TP_REPO}/\.github/workflows/release\.yml@refs/tags/v[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  SHA256SUMS
sha256sum -c SHA256SUMS 2>/dev/null | grep tensorplate_python
```

The signature must report `Verified OK` and the wheel must report `OK`.
Then install it (append `[vision]` for the detection helpers):

```bash
pip install "./tensorplate_python-${TP_VERSION}-py3-none-any.whl[vision]"
```

## Quickstart

Detect objects on an image against a deployed YOLO detector (requires the
`[vision]` extra):

```python
import tensorplate

client = tensorplate.VisionClient("http://127.0.0.1:18080")
for det in client.detect("frame.jpg", endpoint="yolov8n"):
    print(det.class_id, det.score, det.box)
```

`endpoint` is the **deployed model/endpoint name**; the constructor's URL
(or auto-resolution — see [Endpoint resolution](#endpoint-resolution)) is
the worker address. `box` is `(x1, y1, x2, y2)` in source-image pixels.

To call `/infer` directly with your own tensors, use `ServingClient`:

```python
from tensorplate import ServingClient, DType

client = ServingClient("http://127.0.0.1:18080")
# payload is your raw, little-endian tensor data as bytes.
tensor = client.tensor_input("images", payload, DType.UINT8, [1, 480, 640, 3])
result = client.infer("yolov8n", [tensor])

print(result.request_id)
for out in result.outputs:
    print(out.name, out.dtype, out.shape)
```

## API reference

### `ServingClient`

`ServingClient(serving_url=None, *, profile=None, config_path=None, timeout=30.0, discover=True)`

A client for one serving worker. With no `serving_url` the endpoint is
resolved with CLI-parity precedence (see
[endpoint-resolution.md](./endpoint-resolution.md)); `discover=False` skips
the agent-discovery tier; `timeout` is in seconds.

- `infer(endpoint, inputs, *, deadline_ms=None, correlation_id=None) -> InferResult`
  — POST a request to the resolved worker. `endpoint` (required) is the
  deployed model name; `inputs` is a sequence of `TensorInput`. Raises
  `ValueError` on an empty `endpoint`, empty `inputs`, a non-positive
  `deadline_ms`, or an empty `correlation_id`.
- `health() -> HealthSnapshot` — GET `/health`; returns the readiness
  snapshot even when the worker reports a degraded HTTP status.
- `tensor_input(name, data, dtype, shape, *, layout=Layout.ROW_MAJOR) -> TensorInput`
  (static) — build an input from raw `bytes` with no numpy dependency.
- `endpoint` — the `ResolvedEndpoint` this client targets.

`InferResult` exposes `request_id: str`, `outputs: tuple[TensorOutput, ...]`,
`correlation_id`, `timing`, and `output(name) -> TensorOutput`.

`TensorOutput` exposes `name`, `dtype`, `shape`, `data: bytes`, `layout`,
`semantic_tag`, and `to_numpy()` (requires the `[numpy]` extra; `bfloat16`
has no numpy mapping and raises). `TensorInput.from_numpy(name, array)`
builds an input from an ndarray (also `[numpy]`).

`HealthSnapshot` exposes `state`, `endpoint`, `backend`, `active_model_id`,
`last_error_code`, `last_error_message`, `queue_depth`, `in_flight`, and the
`is_ready` property (true only when `state == "ready"`).

### `VisionClient`

`VisionClient(serving_url=None, *, profile=None, config_path=None, timeout=30.0, discover=True, client=None)`

A detection-focused wrapper over `ServingClient` (pass an existing
`client=` to share one; `.serving` exposes it). Requires the `[vision]`
extra.

- `detect(image, *, endpoint, input_name="images", output_name=None, score_threshold=0.25, nms_threshold=0.45, labels=None, transposed=False, contract="yolo_v8_single_output", preprocess_config=None) -> list[Detection]`
  — preprocess `image` (a path, `bytes`, `Path`, or HWC `uint8` ndarray),
  call `infer`, and decode detections. `endpoint` is keyword-only and
  required. See [detection.md](./detection.md) for the contract, output
  selection, and pre/post options.

`Detection` is a frozen dataclass: `class_id: int`, `score: float`,
`box: (x1, y1, x2, y2)` in **source-image pixels**, and `label: str | None`
(set from `labels[class_id]` only when `labels` is given).

### Errors

Every SDK error derives from `TensorPlateError`, so one `except` covers the
whole surface:

| Exception | Raised when |
| --- | --- |
| `EndpointResolutionError` | A serving URL or CLI profile is malformed or unresolvable. |
| `TransportError` | The worker is unreachable. |
| `RequestTimeoutError` (a `TransportError`) | The request exceeds `timeout`. |
| `ProtocolError` | The response is not a valid v0.1 envelope. |
| `UnsupportedSchemaVersionError` (a `ProtocolError`) | The worker's `schema_version` is not `0.1`. |
| `ServingError` | The worker returned a typed `failure`. Carries `.code` (an `ErrorCode`), `.message`, `.context`, and `.request_id`. |

```python
from tensorplate import ServingClient, ServingError, TransportError

try:
    result = ServingClient().infer("yolov8n", inputs)
except ServingError as exc:
    print(exc.code, exc.message)
except TransportError:
    print("serving worker unreachable")
```

## Endpoint resolution

`ServingClient` and `VisionClient` resolve the worker URL exactly as
`tensorplate infer`: explicit URL → CLI profile `serving_url` →
agent-discovered active deployment → loopback `http://127.0.0.1:18080`. The
full precedence and URL canonicalization rules are in
[endpoint-resolution.md](./endpoint-resolution.md).

## Examples

Runnable, user-space samples live in
[`examples/vision_detection_sdk/`](../../examples/vision_detection_sdk/):
`yolo_detect.py` (single image) and `camera_infer.py` (a per-frame
`camera → SDK → /infer` reference loop). They are learning samples, not a
supported runtime surface.

## Scope and non-goals

v0.1.3 calls **already-deployed** models. The SDK and its examples
deliberately do **not** provide:

- in-runtime camera or video ingest;
- a DeepStream sink or any GStreamer integration;
- a streaming session API — the SDK is synchronous request/response;
- any change to runtime serving behavior or ownership.

These are separate, deferred capabilities tracked outside v0.1.3. The
client-side preprocessing and postprocessing helpers are a convenience for
calling a detector; they are not an in-runtime pre/post contract.
