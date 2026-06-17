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
pip install tensorplate-python
```

## Ownership

- **Layer:** client / user space (no runtime or serving-worker code)
- **Language:** Python (3.10+)
- **Distribution:** `tensorplate-python` (PEP 621 `pyproject.toml`)
- **Import package:** `tensorplate`

## Status

This package is under active development for the v0.1.3 release. The
current skeleton wires the packaging metadata, the `src/` layout, the
`py.typed` marker, and the public import surface — `ServingClient`,
`VisionClient`, `Detection`, and the `TensorPlateError` base exception —
as placeholders. The serving client, the vision detection helpers, the
examples, and the published distribution land in subsequent changes.

## Scope

The SDK is a client-side library: it calls already-deployed models and
does not change runtime serving. In-runtime camera/video ingest, a
DeepStream sink, and a streaming session API are out of scope and are
not provided here.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
