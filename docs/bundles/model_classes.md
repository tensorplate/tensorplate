# Model Classes and IO Schema

**Status:** v0.1.0 bundle format
**Schema:** [`protocol/schemas/bundle_manifest.json`](../../protocol/schemas/bundle_manifest.json)

The manifest's `model_class` slug picks one of the v0.1.0 model
classes. Every class uses the same named-input / named-output schema,
so vision (one input) and SmolVLA (multiple inputs + named action
chunk) run through the same `InferRequest` / `InferResult` value
objects in the runtime.

| Class       | v0.1.0 status         | Notes                                                                  |
| ----------- | --------------------- | ---------------------------------------------------------------------- |
| `vision`    | Validated (TensorRT)  | n=1 input is the same shape as the general schema.                     |
| `vla`       | Validated (python_pytorch) | Multi-input + named action chunk; `vla` block carries control metadata. |
| `language`  | Parsed (reserved)     | Tokenizer + generation_config reserved for v0.2 generation runtime.    |
| `speech`    | Parsed                | Reserved task/sample-rate/feature_extractor.                           |
| `embedding` | Parsed                | Reserved dim/metric/normalize.                                         |
| `custom`    | Parsed (free-form)    | Any combination of blocks is allowed.                                  |

## Generalized inputs / outputs

```text
inputs[]:  { name, modality?, dtype, shape, layout?, encoding?, optional?, semantics? }
outputs[]: { name,            dtype, shape, layout?, semantics?, control_loop? }
```

- `shape` axes must be `-1` (dynamic) or a positive integer.
- Duplicate input or output names raise typed validation errors.
- `encoding` is bounded to 64 bytes (e.g. `rgb24`, `pcm_s16le`).
- `semantics` is bounded to 64 bytes (e.g. `observation.state`,
  `action.chunk`).

The same schema describes:

- Single-input vision (`inputs[0]` = the n=1 case).
- SmolVLA multi-input (`observation.image`, `observation.state`,
  optional `prompt`) + named action chunk output (`control_loop: true`).
- Reserved language inputs (tokens) and outputs (logits).

No model-class-specific request types exist in the serving layer.

## Per-class blocks

`model_blocks` carries optional class-specific metadata. The parser
enforces that a populated block matches the declared `model_class`;
`custom` is the only class that accepts any combination of blocks.

### Reserved `language` block

The roadmap requires that the v0.1.0 schema reserve the tokenizer +
generation_config fields so the future generation runtime can land
without a format-version bump. The reserved schema is:

```json
"language": {
  "tokenizer": {
    "reference":          "spiece.model",
    "kind":               "sentencepiece",
    "revision_or_digest": "sha256:..."
  },
  "context_length_tokens": 4096,
  "generation_config": {
    "max_new_tokens":   0,
    "temperature":      0.0,
    "top_p":            1.0,
    "top_k":            0,
    "stop_sequences":   [],
    "seed":             0,
    "streaming":        false
  }
}
```

Empty / default values are valid. Tokenizer kind enums are
`sentencepiece`, `tiktoken`, `huggingface`, `byte_level_bpe`, or
`custom`. The parser rejects empty tokenizer references but does not
require generation runtime support.

### `vla` block

```json
"vla": {
  "control_frequency_hz": 30,
  "action_horizon_steps": 16,
  "action_chunk_size":    1,
  "input_modalities":     ["image", "state"],
  "action_dim":           14
}
```

V01-E12 telemetry consumes the control frequency to bound rolling
control-loop jitter aggregation; release validation uses the SmolVLA
fixture as the reference VLA bundle.

### `vision`, `speech`, `embedding`, `custom`

All optional. See [`manifest.md`](manifest.md#model-class-blocks).

## Mapping to runtime types

| Bundle field   | Runtime mapping (V01-E02)         |
| -------------- | --------------------------------- |
| `model_class`  | `ModelSpec::model_class`          |
| `inputs[]`     | `InferRequest::inputs[]` (`NamedInput`) |
| `outputs[]`    | `InferResult::outputs[]` (`NamedOutput`) |
| `precision_hint` | `ModelSpec::precision_hint`     |
| `backend_hint` | `ModelSpec::backend_hint`         |

The parser exposes the manifest's inputs/outputs as part of the
`BundleDescriptor`; the agent uses them for shape/dtype debugging
during deploy, and V01-E12 telemetry uses the `semantics` and
`control_loop` flags to bound metric labels.
