# `sdk/python/`

`tensorplate-python` — the first-party Python SDK for calling deployed
TensorPlate detection and vision serving models over the v0.1 `/infer`
HTTP envelope.

The import package is `tensorplate`:

```python
import tensorplate
```

## Install

```bash
pip install tensorplate-python            # core client
pip install "tensorplate-python[vision]"  # + numpy & Pillow for VisionClient.detect
```

The wheel + sdist are also attached to each signed GitHub Release for
checksum-verified or air-gapped installs. See the
[SDK quickstart](../../docs/sdk/python.md#install) for that flow and the
`[numpy]` / `[vision]` extras.

## Ownership

- **Layer:** client / user space (no runtime or serving-worker code)
- **Language:** Python (3.10+)
- **Distribution:** `tensorplate-python` (PEP 621 `pyproject.toml`)
- **Import package:** `tensorplate`

## Documentation

- [Quickstart and API reference](../../docs/sdk/python.md) —
  `ServingClient`, `VisionClient`, `Detection`, tensors, and errors.
- [Detection workflow](../../docs/sdk/detection.md) — preprocessing, the
  `yolo_v8_single_output` contract, and postprocessing.
- [Endpoint resolution](../../docs/sdk/endpoint-resolution.md) — CLI-parity
  precedence and URL canonicalization.
- [Examples](../../examples/vision_detection_sdk/) — single-image and
  user-space camera samples.

## Scope

The SDK is a client-side library: it calls already-deployed models and
does not change runtime serving. In-runtime camera/video ingest, a
DeepStream sink, and a streaming session API are out of scope and are
not provided here.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
