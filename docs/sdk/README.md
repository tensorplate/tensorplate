# `docs/sdk/`

Developer documentation for the first-party `tensorplate-python` Python
SDK (import package `tensorplate`) — a client-side library for calling
already-deployed TensorPlate detection and vision models over the v0.1
serving `/infer` HTTP envelope. The SDK does not deploy or run models and
adds no runtime serving capability.

| Document | What it covers |
| --- | --- |
| [`python.md`](./python.md) | Install (`pip install tensorplate-python` or the signed release wheel), the quickstart, and the API reference: `ServingClient`, `VisionClient`, `Detection`, the tensor value objects, the typed error hierarchy, and the scope boundaries. |
| [`detection.md`](./detection.md) | The detection workflow: preprocessing options, the `yolo_v8_single_output` contract, the `detections.*` output convention, and postprocessing options. |
| [`endpoint-resolution.md`](./endpoint-resolution.md) | How the SDK resolves a serving endpoint, matching `tensorplate infer` precedence and URL canonicalization. |
