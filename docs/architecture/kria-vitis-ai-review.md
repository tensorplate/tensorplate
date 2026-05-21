# Kria / Vitis AI adapter design review

> **Status:** v0.1.0 paper exercise (V01-E05-F07). No Vitis AI
> implementation is in scope for v0.1.0.
> **Goal:** confirm that a future Xilinx/AMD Kria adapter using
> Vitis AI and DPU execution can plug into the published
> `tensorplate::ExecutionSession`, `BackendCapability`,
> `BackendRegistry`, `ModelSpec`, `BufferRef`, `TensorView`, and
> sidecar protocol contracts **without public-interface revision**.
> If this review uncovers a required public change, the freeze of
> v0.1.0 interfaces is blocked until that change lands.

## How this review was conducted

I read each of the published v0.1.0 contracts that a Kria/Vitis AI
adapter would have to touch and matched them against the operations
the Vitis AI runtime (DPU-PYNQ, `vai_runtime`, the `xir` graph
representation) and the Xilinx XRT bring-up demand from an adapter.
The artifacts I cross-checked are:

- `include/tensorplate/core/execution_session.hpp` — NVI lifecycle.
- `include/tensorplate/backend/capability.hpp` — capability record.
- `include/tensorplate/backend/registry.hpp` — adapter registration.
- `include/tensorplate/core/model_spec.hpp` — bundle identity and
  `backend_hint`.
- `include/tensorplate/buffer/buffer_ref.hpp` and
  `include/tensorplate/buffer/tensor_view.hpp` — payload ownership.
- `protocol/schemas/python_pytorch_ipc.json` — sidecar contract (not
  required for Vitis AI; included only to confirm it does not leak
  into the public C++ interface).

## Mapping checklist

| Concern | Required mapping | v0.1.0 status |
| ------- | ---------------- | ------------- |
| Bundle artifact | `tensorplate-model` bundle must accept a `.xmodel` artifact path under `ModelSpec::artifact_path`, alongside an optional Vitis-style INT8 calibration metadata blob. | `ModelSpec::artifact_path` is a free-form path string and `backend_hint` is a free-form string today (validated by the registry, not by enum). Capability-record fields cover precision and target compatibility notes. **No public change required.** |
| Bundle schema | `model_class`, named `inputs`, named `outputs`, `runtime_capabilities`, and `precision` must be enough without leaking Vitis types into `protocol/schemas/`. | Confirmed by the existing schema; the V01-E13 reserved-`language`-block precedent shows we extend by adding sibling blocks, not by changing the envelope. **No public change required.** |
| `load` semantics | Adapter must be able to deserialize the `.xmodel`, instantiate the Vitis AI graph runner, and report `LoadFailed` / `ConfigInvalid` / `Unsupported` / `OOMError` on a typed error. | `ExecutionSession::do_load(const ModelSpec&)` accepts the spec by reference and returns `Result<void>`. No CUDA / TensorRT / Python assumption appears in the wrapper. **No public change required.** |
| `prime` semantics | DPU runner instantiation, fixed-shape binding setup, and warmup happen here. The session must transition to `Ready` only when binding succeeds. | `do_prime()` is exactly this slot; it is non-virtual at the NVI level and the wrapper transitions to `Ready` only when the protected `do_prime` returns success. **No public change required.** |
| `infer` semantics | Vitis AI executes synchronously with fixed-shape inputs; we must copy from `BufferRef` -> DPU-compatible memory before launching the runner. | `do_infer(const InferRequest&)` returns `Result<std::vector<NamedOutput>>`. The NVI wrapper validates shape before dispatch using `TensorView::byte_size()`. Adapters that need their own memory copy (TensorRT and a future Vitis AI adapter) just call `BufferManager::view(buf, tv)` and write the bytes into their own staging buffer. **No public change required.** |
| `infer_async` | Vitis AI runners support job-id based async execution; mapping it onto `infer_async` + `AsyncInferHandle` is straightforward (the handle's `async_id` can hold the Vitis job id). Adapters that do not implement native async return typed `Unsupported`, which the conformance suite already exercises. | `AsyncInferHandle` carries `request_id` plus a monotonic session-scoped `async_id`; no Vitis-specific field is required. **No public change required.** |
| `unload` | Tear down the runner, release DPU buffers, and detach the bitstream if the adapter set one. | `do_unload()` is a protected hook; the NVI wrapper transitions back to `Unloaded` on success and to `Failed` on adapter failure. **No public change required.** |
| Adapter internals | XIR graph object, `vart::Runner`, DPU bitstream / overlay, scratch tensors, and any XRT BO handles must stay private to the adapter's `runtime/src/adapters/vitis_ai/` directory. | The existing adapters (TensorRT, LibTorch, Python sidecar) already prove this pattern: the internal `*_session.hpp` lives next to the `.cpp` and no SDK type appears on a public header. **No public change required.** |
| Capability flags | The adapter must publish `shape_support = Fixed`, an INT8 precision profile, the op-coverage / unsupported-op set, and an optional memory estimate. | `BackendCapability` carries `shape_support` (`Fixed` / `Dynamic` / `RangeBounded`), supported precisions, `op_coverage_score_pct`, and memory estimate / limit. **No public change required.** |
| Precision | `PrecisionHint::Int8` is the v0.1.0 Vitis AI workhorse; INT4 is reserved for v0.2 quantization profiles. | `PrecisionHint::Int8` is already part of the wire enum; the bundle pipeline rejects unsupported precisions with `Error::Code::Unsupported`. **No public change required.** |
| Buffer plane | A Vitis AI adapter typically copies from host-resident `BufferRef` bytes into DPU-compatible memory before launching the runner. The host `BufferRef` semantics (Owned / Borrowed / Released; deterministic single release) remain valid. | `BufferRef` is opaque and never exposes a pointer; the adapter accesses bytes only through `BufferManager::view` (input) and `BufferManager::data` (output). The same code path the Python sidecar adapter uses for payload marshaling will work for Vitis AI; future zero-copy via XRT BOs is a buffer-plane optimization, not a public-interface requirement. **No public change required.** |
| Scheduler | Deadline-aware admission stays backend-neutral; deterministic-latency advantages surface as capability data, not as a special scheduler API. | The scheduler arrives in V01-E06; its public interface (per the roadmap) accepts the same `InferRequest` value object and consults `BackendCapability` via the registry. **No public change required, recheck after V01-E06.** |
| Observability | Vitis AI latency, deadline misses, fallback, readiness, and health events use the V01-E04 `SessionEvent` taxonomy. Backend-specific diagnostics stay bounded inside the adapter. | `SessionEventKind` covers `load_start/end/failed`, `prime_*`, `infer_*`, `unload_*`, `validation_failed`, and `unsupported_async`; nothing in the event record is GPU-specific. **No public change required.** |
| Sidecar protocol | Vitis AI executes in-process in C++; the Python sidecar protocol is not involved. | The sidecar protocol's `backend_hint: python_pytorch` is the only path that loads the sidecar adapter; a future `backend_hint: vitis_ai` resolves to a separate factory through the registry. **No public change required.** |

## Review questions and answers

> *Can `.xmodel` and any associated metadata be represented as bundle
> artifacts without changing the bundle envelope?*

Yes. `ModelSpec::artifact_path` is a free-form path. Additional Vitis
metadata (calibration / quantization profile) belongs in a new bundle
sibling block whose addition does not require revising the envelope,
analogous to the `language` block already reserved in V01-E13.

> *Does the `load` / `prime` / `infer` / `infer_async` / `unload`
> lifecycle cover Vitis AI runner lifetime and any required DPU setup?*

Yes. The protected `do_*` hooks accept the spec or request by const
reference and return typed `Result<...>`. No method shape assumes CUDA
or TensorRT.

> *Are fixed-shape and op-coverage constraints expressible as
> normalized capability flags?*

Yes. `BackendCapability` exposes `shape_support = Fixed`,
`op_coverage_score_pct` (0..100), and a free-form
`target_compatibility_notes` vector for "this layer requires DPU
overlay X" guidance the agent's doctor can surface.

> *Does the precision schema cover Vitis AI's INT8-oriented quantization
> and calibration requirements?*

Yes. `PrecisionHint::Int8` is the operating profile. Calibration
metadata is bundle-side metadata referenced through the manifest, not
part of the runtime capability record.

> *Can DPU-compatible memory needs be handled through adapter-owned
> copies or buffer-plane extensions without changing public
> `BufferRef` semantics?*

Yes. `BufferRef` is opaque; the buffer plane mediates access via
`BufferManager::view` (read) and `BufferManager::data` (write).
Zero-copy via XRT BOs is a future buffer-plane optimization (post
v0.2) and does not require a public-interface revision.

> *Does any public runtime header need a Vitis AI, XRT, DPU, or Xilinx
> SDK type?*

No. The pattern proven by the V01-E05-F02 / F03 / F05 adapters is
that all SDK types live in `runtime/src/adapters/<name>/`; the
public surface only sees `ExecutionSession`, `BackendCapability`, and
the value objects from V01-E02 / V01-E03.

## Findings and blockers

**No interface blockers identified for v0.1.0 freeze.** Every concern
above maps cleanly onto an already-published interface. The only
work a future Kria/Vitis AI adapter requires is:

1. A new directory `runtime/src/adapters/vitis_ai/` mirroring the
   TensorRT / LibTorch / Python-pytorch layout (each adapter has a
   `_session.hpp` internal header, a `_session.cpp` implementation,
   and a registration function dispatched by
   `runtime/src/backend/builtin.cpp` under a new
   `TP_ENABLE_VITIS_AI` build flag).
2. A bundle sibling block for Vitis-style calibration metadata, added
   through the V01-E13 schema-evolution rules (no envelope change).
3. T1 unit tests asserting registration, capability publication, and
   the no-SDK `Unsupported` path (mirroring V01-E05-F02 / F03).
4. T4 hardware-in-loop validation on a Kria K26 / K24 board (out of
   scope for v0.1.0 OSS).

## Sign-off

The v0.1.0 public interfaces — `ExecutionSession` (V01-E04),
`BackendCapability` and `BackendRegistry` (V01-E05-F01), `BufferRef`
and `TensorView` (V01-E03), `ModelSpec` (V01-E02), the event taxonomy
(V01-E04-F06), and the bundle envelope (V01-E13) — are sufficient for
a future Kria/Vitis AI adapter. No required interface change is
identified.

The V01-E05 Epic acceptance criterion *"Kria/Vitis AI design review
records that a future adapter can plug in without public interface
revision, or identifies required v0.1.0 interface fixes before
freeze"* is met by this document.

When the Vitis AI implementation lands (post-v0.1.0, see the roadmap
charter under `v0.4+` in
`tensorplate-internals/planning/v0.1.0/tensorplate-oss-v0.1-architecture-and-roadmap.md`),
this document gets revisited and any deviation from the assumptions
above gets recorded as an addendum here.

---

## V01-E13 schema review addendum

> **Status:** schema portion of the Kria/Vitis AI review completed in
> V01-E13-F08.

V01-E13 introduces the v0.1.0 manifest authoring surface. The schema
portion of this review answers the explicit Kria/Vitis questions from
the roadmap checklist:

- **`.xmodel` as a bundle artifact.** The manifest's `artifacts[].kind`
  enum reserves `vitis_xmodel`, the `artifacts[].path` rules accept the
  `.xmodel` extension, and `artifact_kind_for_path` maps the extension
  to the same slug. The synthetic Vitis fixture
  (`test/models/bundles/v01_e13/vitis_synthetic/`) proves the envelope
  carries an `.xmodel` plus calibration metadata without schema
  revision.
- **Fixed-shape and op-coverage constraints.** The
  `capability_requirements.fixed_shape` and
  `capability_requirements.op_coverage_limits` fields are part of the
  v0.1.0 capability schema. The conformance suite asserts that a bundle
  declaring `fixed_shape: true` only deploys when the configured
  backend publishes that flag.
- **DPU lifecycle assumptions.** `ExecutionSession::load`/`prime`/
  `unload` are sufficient; the manifest's `precision.vitis_ai.dpu_arch`
  field is a bounded, opaque DPU architecture identifier (no XRT or
  Xilinx SDK type leaks into the schema).
- **Vitis INT8 calibration / quantization metadata.** The
  `precision.vitis_ai` block carries `quantize_strategy` (post-training
  / calibration / qat), `calibration_dataset_digest`,
  `calibration_sample_count`, and the opaque `dpu_arch`. None of these
  fields force a vendor-shaped data model onto the Jetson precision
  metadata, which lives in `precision.jetson`.
- **Reserved language block.** Schema-side precedent for class-specific
  reserved blocks is established by `model_blocks.language`. Future
  Vitis-class-specific blocks (if any) follow the same pattern without
  bumping the format version.

### V01-E13 sign-off

The bundle schema, parser, and integrity contract carry every Vitis
metadata field a future adapter would need. No public schema change is
required; the only outstanding work is the Vitis AI runtime adapter
itself, which is intentionally out of scope for v0.1.0.

The V01-E13 Epic acceptance criterion *"A Vitis AI `.xmodel` artifact
can be represented without schema revision"* is met by this addendum,
the schema, the Rust types, the synthetic fixture, and the conformance
suite. The interface-freeze gate inherits this sign-off.
