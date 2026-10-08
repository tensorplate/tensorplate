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
| `format_version` | Exactly `0.1` or `0.2`. Unknown majors, minors and alternate spellings are rejected.       |
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
compatibility. Format 0.1 parses them without semantic validation beyond
the class consistency check. Under format 0.2 the `speech` block is the
speech contract described in [Format 0.2](#format-02).

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

## Format 0.2

A manifest opts into format 0.2 with `"format_version": "0.2"`; the
envelope keeps `"schema_version": "0.1"`. A format 0.1 manifest that the
schema accepts parses as before, and the keys below mean nothing to it:
they stay in `BundleManifest.extra` like any other unknown key. (A
sequence-shaped `speech` block, which the schema never allowed, now
rejects.)

Under format 0.2 the parser decodes these fields from the manifest's own
text into `BundleManifest.profile` before anything else, and decodes them
strictly: an unknown key, a `model_blocks` key that is not a class slug, a
repeated key in any object, a present `null` where the field is not
nullable, an array where an object belongs, or a number that is not an
exact nonnegative integer (judged on its written form, so
`1.0000000000000001` rejects while `4096.0` is 4096) fails the bundle with
a `manifest_semantics` error naming the field. Format 0.2 manifests decode
only through the bundle parser; `decode_with_version_check` refuses them.

| Field | Meaning |
| --- | --- |
| `runner_profile` | Installed runner profile id, `lower_snake_case` (e.g. `faster_whisper`, `kokoro`); names a `runner_profiles[].id` in the backend descriptor, never a module or path. |
| `hardware_compatibility` | Platform support row ids the bundle declares, unique. |
| `compute_type` | Compute type the profile loads the model with; one of the descriptor's `compute_types` spellings (`float16`, `float32`, ...). |
| `support_level` | Requested claim: `production`, `preview` or `experimental`. Registry evidence grants support; the manifest cannot. |
| `warmup` | `fixtures` (1–16 `artifacts[].path` entries, so each is hashed), `repetitions` (1–100) and `timeout_ms` (1–600,000). |
| `pipeline_stages` | 1–16 ordered stages, each `{stage, ownership, observable?, interface?}`. A `runtime_owned` stage is one of `ingress`, `vad`, `preprocessing`, `backend`, `postprocessing`, `egress`; a `caller_owned` stage names the caller's span and may carry an `interface` label. `observable: false` marks a stage fused into another, which reports `not_observable`. Stage names are unique. |
| `memory_budget_by_domain` | Per-domain budgets under `shared_pool`, `guest_ram` and `device_vram`, each a line-item object of [`memory_budget_breakdown.json`](../../config/schemas/memory_budget_breakdown.json). A speech bundle declares `os_reserve_bytes: 0`; the row's admission configuration holds the OS reserve. |
| `memory_budget_breakdown_bytes` | The same line items summed across domains; when both are present it must equal the per-line sum. It is a reporting total, not an admission input. |
| `max_concurrent_sessions` | Declared upper bound, 1–2048. |
| `degraded_profile` | `null` for no quality-changing degradation, or a reserved profile id. |
| `base_model_ref` | The base bundle a variant derives from: `{name, version, manifest_digest}`. `name` and `version` are the base manifest's own. `manifest_digest` is the canonical digest the parser computes for the base manifest and `tensorplate-bundle-tool` prints (`sha256:` and 64 lowercase hex digits), whether or not the base declares the optional top-level `manifest_digest`; the deployment descriptor calls the same value `bundle_digest`. Declared together with `variant_identity`. |
| `variant_identity` | `{id, revision, variant_kind}`: a `lower_snake_case` id that stays the same across revisions of the variant, a revision of at most 64 bytes (alphanumeric segments joined by `.`, `_` or `-`), and one of the three kinds below. |

Manifest-local [deployment rules](compatibility.md#format-02-manifest-rules)
require the applicable fields, one matching class block, explicit speech
precision and unambiguous selectors. They refuse reserved classes and VLA modes
under format 0.2 while retaining format 0.1 behavior.

### Variant lineage

A bundle that is a variant of another declares both lineage fields. The
three kinds are a closed vocabulary:

| `variant_kind` | What the variant is |
| --- | --- |
| `speaker_embedding` | Input data for the base. |
| `adapter` | State applied on the base. |
| `full_checkpoint` | An independent set of weights derived from the base. |

Every kind is reserved. The schema accepts the declaration and the parser
validates it, and then refuses the bundle with
`bundle_r8_reserved_variant`: no variant bundle deploys in this release,
whatever its kind. The refusal ends manifest validation, so the manifest's
other rules are reported first and a variant's artifact files and digests are
never read. A voice a TTS bundle ships as one of its own hashed
artifacts and lists under `voices` is part of that bundle, not a variant of
it, and needs no lineage declaration.

Three further checks need facts no manifest holds, so the bundle parser
cannot make them: that the base is a bundle the target knows, that no other
bundle already declares the same variant id and revision (or the same id
with another kind) on that base, and that the variant asks for no more
support than the base holds on the target's platform support row. `BundleProfile::check_lineage` makes
them against base facts its caller supplies. Nothing in the agent supplies
them yet.

The schema validates the same fields in its `format_0_2` definition; its
description lists the checks readers make beyond it. A validator must be
given `config/schemas/memory_budget_breakdown.json` beside the manifest
schema, which references its line-item definition.

### Speech contract

Format 0.2's `model_blocks.speech` declares one task and serving mode and
everything a session may request from it:

```json
"speech": {
  "task":            "stt",                 // stt | tts
  "serving_mode":    "streaming",           // streaming | batch
  "languages":       ["en", "ar"],          // language tags, unique
  "input_audio_formats": [                  // STT only
    {"encoding": "pcm_s16le", "sample_rate_hz": 16000, "channels": 1},
    {"encoding": "mulaw",     "sample_rate_hz": 8000,  "channels": 1}
  ],
  "chunking": {"frame_ms_min": 20, "frame_ms_max": 320, "max_utterance_ms": 30000},
  "algorithm_profile":        {"id": "stt_vad_chunked", "digest": "sha256:..."},
  "quality_profile_digest":   "sha256:...",
  "benchmark_profile_digest": "sha256:..."
}
```

- STT declares `input_audio_formats`, which must include 16 kHz mono
  `pcm_s16le`; 8 kHz mono G.711 μ-law is the one other accepted format.
  It declares no voices and no output audio.
- TTS declares `voices` (`lower_snake_case`, e.g. `af_heart`) and
  `output_audio_formats`, of which 24 kHz mono `pcm_s16le` is the one
  accepted format. It declares no input audio.
- `chunking` is required. For STT it bounds frames to 20–320 ms and
  utterances to 30 s; for TTS it declares `max_segment_text_bytes` (up to
  4,096), `max_segment_phoneme_tokens` (up to 200), `max_segment_audio_ms`
  (up to 30,000), `max_synthesis_text_bytes` (up to 16,384) and
  `max_synthesis_audio_ms` (up to 120,000). A minimum never exceeds its
  maximum and a segment limit never exceeds its synthesis limit.
- Digests are `sha256:` followed by 64 lowercase hex digits.
- A streaming contract resolves to the `stt_streaming` or `tts_streaming`
  serving mode; there is no second mode selector. Per-session memory is
  declared only in the budget lines.

The format 0.2 fixtures in
[`test/models/bundles/v0_2/`](../../test/models/bundles/v0_2/README.md)
show both tasks, with the rejected variants beside them.

## Forward compatibility

The schema sets `additionalProperties: true` at the top level so future
v0.1.* minor additions land without breaking older readers. Unknown
fields are captured in the Rust `BundleManifest.extra` map; the verifier
preserves them across re-serialization.

A `format_version` major bump is required only for changes that break
the on-disk parse contract (e.g., switching artifact integrity to a new
canonicalization). v0.1.0 ships at `format_version: 0.1`.
