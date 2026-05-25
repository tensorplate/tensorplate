# Bundle Manifest

**Status:** v0.1.0 bundle format
**Schema:** [`protocol/schemas/bundle_manifest.json`](../../protocol/schemas/bundle_manifest.json)
**Rust mirror:** [`protocol/rust/src/bundle_manifest.rs`](../../protocol/rust/src/bundle_manifest.rs)

The manifest is the only required structured file inside a bundle. Every
field below is interpreted by the parser; bundle authors should treat fields
as the contract between authoring tools and the runtime.

```text
{
  "schema_version":   "0.1",         // pinned for v0.1
  "format_version":   "0.1",         // bundle on-disk format, MAJOR.MINOR
  "name":             "...",
  "version":          "...",
  "model_class":      "vision" | "speech" | "language" | "vla" | "embedding" | "custom",
  "backend_hint":     "tensorrt" | "libtorch" | "python_pytorch" | ...,
  "precision_hint":   "auto" | "fp32" | "fp16" | "bfloat16" | "int8" | "int4",
  "artifacts":        [ { role, kind?, path, digest, byte_size?, description? }, ... ],
  "inputs":           [ { name, modality?, dtype, shape, layout?, encoding?, optional?, semantics? }, ... ],
  "outputs":          [ { name, dtype, shape, layout?, semantics?, control_loop? }, ... ],
  "target_hardware":  { device_family, min_memory_bytes?, memory_estimate_bytes? },
  "runtime_compatibility": { min_runtime_version?, max_runtime_version? },
  "capability_requirements": { async?, streaming?, generation?, kv_cache?, fixed_shape?,
                               deterministic_latency?, control_loop_integration?,
                               op_coverage_limits?, memory_estimate_bytes? },
  "precision": { profile?, jetson?, vitis_ai? },
  "model_blocks": { vision?, speech?, language?, vla?, embedding?, custom? },
  "manifest_digest":  "sha256:...",  // optional self-digest
  "signature":        { algorithm, key_id?, value },   // optional
  "provenance":       { builder?, build_url?, source_commit?, build_timestamp?, sbom? }
}
```

---

## Required fields

| Field            | Notes                                                                                    |
| ---------------- | ---------------------------------------------------------------------------------------- |
| `schema_version` | Locked to `0.1` for v0.1. Unknown values are rejected with a typed error.                |
| `format_version` | `MAJOR.MINOR`. The agent accepts the runtime's major and rejects unknown future majors.   |
| `name`           | Non-empty. Used in logs, status output, and the bundle `id` (`<name>@<version>`).        |
| `version`        | Non-empty. Bundle-author-declared version string.                                        |
| `model_class`    | One of the six classes (see [model classes](#model-classes)).                            |
| `backend_hint`   | One of the recognized values (see [backend hints](backends.md)).                         |
| `artifacts`      | Non-empty. Exactly one entry must have role `model`.                                     |

## Optional fields

The remaining fields are optional. The verifier ignores `null` and missing
values rather than rejecting them, so bundles can grow incrementally.

---

## Model classes

bundle format owns the model-class taxonomy. v0.1.0 *validates* vision and VLA;
the other classes parse cleanly so future releases can land their runtime
without a bundle-format bump.

| Class       | v0.1.0 status               | Notes                                                                  |
| ----------- | --------------------------- | ---------------------------------------------------------------------- |
| `vision`    | Validated (TensorRT)        | Single named input is the `n = 1` case of the general input schema.    |
| `vla`       | Validated (python_pytorch)  | SmolVLA uses named multi-input + named action output + `vla` block.    |
| `language`  | Parsed (reserved fields)    | Tokenizer + generation_config metadata reserved for v0.2.              |
| `speech`    | Parsed                      | Schema reserves task/sample-rate fields.                               |
| `embedding` | Parsed                      | Schema reserves dim/metric/normalize fields.                           |
| `custom`    | Parsed                      | Free-form metadata under `model_blocks.custom`.                        |

## Generalized inputs and outputs

`inputs[]` and `outputs[]` are arrays of named bindings. Each entry carries:

- `name` (required, unique within the array)
- `dtype` (one of `float32`, `float16`, `bfloat16`, `int64`, `int32`, `int16`, `int8`, `uint8`, `bool`)
- `shape` (per-axis extents; `-1` marks a dynamic axis)
- `layout` (`row_major` (default) or `col_major`)
- `modality` (inputs only: `image`, `video`, `audio`, `text`, `tokens`, `tensor`, `state`, `control`, `custom`)
- `encoding` (inputs only: bounded ≤ 64 bytes, e.g. `rgb24`)
- `semantics` (bounded ≤ 64 bytes — `observation.state`, `prompt`, `action.chunk`)
- `optional` (inputs only)
- `control_loop` (outputs only — true for VLA action chunks; telemetry uses this to label control-loop jitter)

Duplicate input or output names are rejected. `shape` entries must be `-1`
or positive integers; zero and negative-other-than-`-1` are rejected.

Vision and VLA bundles use the same `inputs[]` / `outputs[]` shape; vision
is the n=1 case. No model-class-specific request types exist in the
serving layer.

---

## Model-class blocks

`model_blocks` is an optional object whose only allowed keys are the
class slugs. The parser enforces that a populated block matches the
declared `model_class` — e.g., a vision bundle cannot ship a `language`
block. The `custom` model class is the only class that accepts any
combination of blocks.

### Reserved language block

The `language` block is **parsed but not exercised** in v0.1.0. It exists
so a future v0.2 generation runtime can land without a bundle-format
bump. Reserved fields:

```json
"language": {
  "tokenizer": {
    "reference":          "spiece.model",      // required when tokenizer is set
    "kind":               "sentencepiece",     // sentencepiece | tiktoken | huggingface | byte_level_bpe | custom
    "revision_or_digest": "sha256:..."         // optional
  },
  "context_length_tokens": 4096,
  "generation_config": {
    "max_new_tokens":   128,
    "temperature":      0.7,
    "top_p":            0.95,
    "top_k":            50,
    "stop_sequences":   ["</s>"],
    "seed":             42,
    "streaming":        false
  }
}
```

Empty/default `generation_config` is valid (the runtime simply does
nothing with it in v0.1.0). The parser rejects a language manifest whose
tokenizer reference is empty, and rejects `language` blocks attached to
non-language non-custom classes.

### VLA block

`vla` carries control-loop metadata consumed by V01-E12 telemetry and
release validation:

```json
"vla": {
  "control_frequency_hz": 30,
  "action_horizon_steps": 16,
  "action_chunk_size":    1,
  "input_modalities":     ["image", "state"],
  "action_dim":           14
}
```

### Vision block

`vision` carries optional image-pipeline metadata used by fixtures:

```json
"vision": {
  "task":           "detection",
  "input_size":     { "height": 640, "width": 640 },
  "color_space":    "rgb",
  "normalization":  { "mean": [0.485, 0.456, 0.406], "std": [0.229, 0.224, 0.225] }
}
```

### Other blocks

`speech`, `embedding`, and `custom` blocks exist for forward
compatibility. v0.1.0 parses them without semantic validation beyond the
class consistency check.

---

## Runtime capabilities

`capability_requirements` lists the backend-published capability flags the
bundle declares it needs. The agent looks up each `true` flag in the
configured backend capability map; missing capabilities raise
[`UnsupportedCapability`](../../agent/src/error.rs).

| Flag                          | Meaning                                                                       |
| ----------------------------- | ----------------------------------------------------------------------------- |
| `async`                       | Backend supports `infer_async`.                                                |
| `streaming`                   | Backend can stream incremental outputs.                                        |
| `generation`                  | Backend supports autoregressive generation (reserved; v0.1.0 always false).    |
| `kv_cache`                    | Backend owns KV cache lifecycle (reserved; v0.1.0 always false).               |
| `fixed_shape`                 | Backend requires shapes to be fixed at load time (Vitis / engine-based paths). |
| `deterministic_latency`       | Backend can guarantee deterministic latency under nominal load.                |
| `control_loop_integration`    | Backend cooperates with control-loop deadlines & jitter telemetry.             |
| `op_coverage_limits`          | Bounded array of op-coverage notes (diagnostic only in v0.1.0).                |
| `memory_estimate_bytes`       | Backend-aware override for the top-level memory estimate.                      |

---

## Precision metadata

`precision_hint` keeps the legacy coarse profile (`auto`/`fp16`/`int8`/...).
`precision` is the v0.1.0 addition that carries vendor-specific metadata
without exposing SDK types:

```json
"precision": {
  "profile": "fp16",
  "jetson":  {
    "supported_profiles":     ["fp32", "fp16", "int8"],
    "tensorrt_engine_profile": "orin-nano-fp16"
  },
  "vitis_ai": {
    "quantize_strategy":           "calibration",  // post_training | calibration | qat
    "calibration_dataset_digest":  "sha256:...",
    "calibration_sample_count":    512,
    "dpu_arch":                    "DPUCZDX8G_ISA0"
  }
}
```

The verifier rejects malformed Vitis-style digests with a typed error.
Jetson and Vitis fields stay independent: a Jetson bundle never has to
fill the Vitis subobject and vice versa.

---

## Integrity metadata

See [integrity.md](integrity.md) for the canonicalization rules, manifest
digest semantics, and optional signature/provenance handling. The short
form: every required artifact must publish a `sha256:hex` digest, and
`manifest_digest` is the sha256 of the canonical manifest with that field
stripped.

---

## Forward compatibility

The schema sets `additionalProperties: true` at the top level so future
v0.1.* minor additions land without breaking older readers. Unknown
fields are captured in the Rust `BundleManifest.extra` map; the verifier
preserves them across re-serialization.

A `format_version` major bump is required only for changes that break
the on-disk parse contract (e.g., switching artifact integrity to a new
canonicalization). v0.1.0 ships at `format_version: 0.1`.
