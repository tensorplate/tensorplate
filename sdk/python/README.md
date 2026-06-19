# `sdk/python/`

`tensorplate-python` — the first-party Python SDK for calling deployed
TensorPlate detection and vision serving models over the v0.1 `/infer`
HTTP envelope.

The import package is `tensorplate`:

```python
import tensorplate
```

## Install

> PyPI publication is deferred for v0.1.3. Install the SDK from the signed
> wheel attached to the matching GitHub Release; `pip install
> tensorplate-python` becomes the primary path once the project is
> published. See the [SDK quickstart](../../docs/sdk/python.md#install) for
> the download-and-verify flow and the `[numpy]` / `[vision]` extras.

```bash
# v0.1.3: install from the signed release wheel
pip install "./tensorplate_python-<version>-py3-none-any.whl"
```

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
