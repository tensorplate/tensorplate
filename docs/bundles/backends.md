# Backend Hints, Artifacts, and Capabilities

**Status:** v0.1.0 (V01-E13-F05)
**Schema:** [`protocol/schemas/bundle_manifest.json`](../../protocol/schemas/bundle_manifest.json)
**Backend registry:** [`docs/architecture/backend-registry.md`](../architecture/backend-registry.md)

The bundle author declares the execution backend through `backend_hint`.
**The runtime validates and honors the declared backend; it does not pick
heuristically and does not fall back at inference time.** This document
records the recognized backend slugs, their artifact representations, and
how runtime capabilities are normalized.

---

## Recognized backend hints

| Slug              | v0.1.0 status      | Notes                                                                   |
| ----------------- | ------------------ | ----------------------------------------------------------------------- |
| `tensorrt`        | Implemented        | Jetson Orin vision validation path.                                     |
| `libtorch`        | Implemented        | PyTorch graphs that export cleanly into native C++.                     |
| `python_pytorch`  | Implemented        | Required SmolVLA path through the managed Python sidecar.               |
| `vitis_ai`        | **Reserved**       | Schema slot for the future Kria/Vitis AI adapter. Parser accepts; deploy compat rejects on Jetson devices because the backend is unavailable. |
| `onnxruntime`     | **Reserved**       | Schema slot for ONNX Runtime adapter.                                   |
| `mock`            | Test-only          | Used by host CI; never published on a real device.                      |

The parser uses [`RECOGNIZED_BACKEND_HINTS`](../../protocol/rust/src/bundle_manifest.rs) to reject typos before
they reach the deploy verifier. The agent's `available_backends` config
owns *runtime* availability; an unknown-but-spelled-correctly backend
parses cleanly and is rejected as
[`UnavailableBackend`](../../protocol/rust/src/bundle.rs) by the compat
evaluator.

### Extension policy

Adding a new backend hint is an additive change:

1. Update `RECOGNIZED_BACKEND_HINTS` in `protocol/rust/src/bundle_manifest.rs`.
2. Update the JSON Schema description (the actual enum is open by design
   because the agent config decides availability).
3. Add the new artifact kind under [`ArtifactKind`](../../protocol/rust/src/bundle_manifest.rs) and the schema's `artifacts[].kind`
   enum.
4. Document the backend in this file.

No format-major bump is required, since the backend hint, artifact kind,
and capability descriptors are already open extension surfaces.

---

## Artifact kinds

| Slug                   | Typical extensions  | Backend           |
| ---------------------- | ------------------- | ----------------- |
| `tensorrt_engine`      | `.engine`            | `tensorrt`        |
| `libtorch_state`       | `.pt`, `.pth`        | `libtorch`        |
| `python_pytorch_entry` | `.py`, `.safetensors`| `python_pytorch`  |
| `onnx`                 | `.onnx`              | `onnxruntime`     |
| `vitis_xmodel`         | `.xmodel`            | `vitis_ai`        |
| `weights`              | `.safetensors`, `.bin` | sidecar role     |
| `tokenizer`            | varies               | language bundles  |
| `calibration`          | varies               | Jetson INT8 / Vitis |
| `auxiliary`            | any                  | optional assets   |

The optional `artifacts[].kind` field lets a bundle author state the
representation explicitly. Bundle parsers also infer the kind from the
filename extension when `kind` is omitted (see
[`artifact_kind_for_path`](../../protocol/rust/src/bundle.rs)); the
inference is best-effort and only used by the backend/artifact-kind
cross-check inside the compat evaluator.

---

## Runtime capabilities

`capability_requirements` lists what the bundle expects the chosen
backend to support. The agent looks up each `true` flag in its
[`backend_capabilities`](../../config/schemas/agent.json) config. Missing
capabilities are reported as
[`CompatibilityViolation::UnsupportedCapability`](../../protocol/rust/src/bundle.rs)
before staging.

| Flag                       | What it means                                                                  |
| -------------------------- | ------------------------------------------------------------------------------ |
| `async`                    | Backend supports `ExecutionSession::infer_async`.                              |
| `streaming`                | Backend can stream incremental outputs.                                        |
| `generation`               | Backend supports autoregressive generation (reserved; v0.1.0 always false).    |
| `kv_cache`                 | Backend owns KV cache lifecycle (reserved).                                    |
| `fixed_shape`              | Backend requires shapes to be fixed at load time.                              |
| `deterministic_latency`    | Backend guarantees deterministic latency under nominal load.                   |
| `control_loop_integration` | Backend cooperates with deadline-aware scheduling for control loops.           |
| `op_coverage_limits`       | Bounded array of op-coverage notes (diagnostic only).                          |
| `memory_estimate_bytes`    | Backend-aware memory estimate override.                                        |

### No heuristic fallback

The runtime does not attempt LibTorch first and fall back to Python; it
does not infer a backend at request time. If the declared backend is
unavailable, the deploy fails before staging. This is verified by
[`tests/bundle_conformance.rs`](../../protocol/rust/tests/bundle_conformance.rs).

---

## Precision

The bundle's effective precision profile comes from one of:

- `precision_hint` (top-level enum, legacy from V01-E08)
- `precision.profile` (mirror of the hint inside the structured block)

The structured block carries vendor metadata under `precision.jetson`
and `precision.vitis_ai`. Backends publish their supported precision
profiles through `BackendProfile::supported_precision`; the compat
evaluator rejects a bundle whose declared profile is not in that list.
The agent fills this profile from `backend_capabilities[*].supported_precision`
in its runtime config.

---

## Vitis-shaped bundles in v0.1.0

A bundle that declares `backend_hint: vitis_ai` and ships an `.xmodel`
artifact parses cleanly. On a Jetson device, the agent's
`available_backends` config does not include `vitis_ai`, so the compat
evaluator returns
[`CompatibilityViolation::UnavailableBackend`](../../protocol/rust/src/bundle.rs).
This is the v0.1.0 design intent: prove the schema supports Kria silicon
without claiming the runtime can execute it.

The synthetic Vitis fixture lives at
[`test/models/bundles/v01_e13/vitis_synthetic/`](../../test/models/bundles/v01_e13/vitis_synthetic/).
It is parser/design-review only — never staged on a v0.1.0 device.
