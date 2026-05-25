# Non-GPU Lifecycle Compatibility Review (V01-E04-F07-T02)

**Status:** v0.1.0 paper exercise — sign-off precedes V01-E05 adapter implementation
**Reviewed contracts:** `tensorplate::ExecutionSession`,
`tensorplate::ModelSpec`, `tensorplate::InferRequest`,
`tensorplate::InferResult`, `tensorplate::NamedInput`,
`tensorplate::NamedOutput`, `tensorplate::BufferRef`,
`tensorplate::TensorView`, `tensorplate::SessionEvent`

This document is the V01-E04-F07-T02 deliverable: a focused review of
the public `ExecutionSession` lifecycle, value objects, and event
taxonomy against a future Xilinx/AMD Kria adapter using Vitis AI and
DPU execution. The intent is to catch NVIDIA-specific assumptions
while they are still cheap to fix and **before** V01-E05 adapter work
freezes the interface.

The conformance suite in
[`test/contract/execution_session_conformance.hpp`](../../test/contract/execution_session_conformance.hpp)
and the public-header vendor-hygiene check in
[`test/unit/execution_session_interface_test.cpp`](../../test/unit/execution_session_interface_test.cpp)
are the mechanical proofs that follow from this review.

## Mapping checklist (status: green)

| Concern | Mapping in v0.1.0 public contract | Result |
| --- | --- | --- |
| `.xmodel` artifact discovery | `ModelSpec::artifact_path` is a free-form string; backend_hint string is free-form (e.g. `"vitis_ai"`). No CUDA / TensorRT assumption in the type. | OK |
| Bundle precision metadata | `ModelSpec::precision_hint` includes `Int8` and `Int4`; Vitis AI INT8 calibration metadata fits the existing precision schema (extended through the bundle format bundle schema, not the runtime). | OK |
| DPU runner instantiation | `do_prime` runs after `do_load`; the adapter performs runner instantiation, bitstream load, and fixed-shape binding inside `do_prime`. The NVI wrapper does not assume `prime` is a no-op. | OK |
| Fixed-shape execution | `TensorView::shape()` is fully specified; adapters with fixed-shape requirements reject mismatched shapes via `Error::Code::ShapeMismatch` and the NVI wrapper rejects mismatched tensor windows before adapter dispatch. | OK |
| Op-coverage / fallback | Capability publication lives at the adapter boundary (V01-E05). The V01-E04 contract does not assume any op-coverage policy. | OK |
| Adapter-owned memory copies | `BufferRef` does not expose raw pointers; an adapter that requires copies into DPU-compatible memory does the copy internally without changing `BufferRef` semantics. | OK |
| Backend health and readiness | `do_load` / `do_prime` return typed `Result<void>`; the NVI wrapper transitions to `Failed` (or `Loaded` on `ConfigInvalid`) without assuming GPU-only semantics. | OK |
| Streaming / generation / KV-cache | Not in the V01-E04 contract; deferred to V01-E05 capability publication and V0.2 generation primitives. | OK |
| `infer_async` shape | Wrapper returns typed `Unsupported` without adapter execution dispatch unless an adapter reports native async support. Vitis AI / DPU can remain unsupported in v0.1.0. | OK |
| Latency stamping | `std::chrono::steady_clock` is monotonic across all silicon; no CUDA-event dependency. | OK |
| Event records | `SessionEvent` fields are bounded and adapter-neutral (`backend_name`, `model_id`, `request_id`, `Error::Code`, `duration`, `state_after`). No CUDA / TensorRT type appears anywhere in the record. | OK |

## Public-header vendor hygiene

The V01-E04-F01 public header
([`include/tensorplate/core/execution_session.hpp`](../../include/tensorplate/core/execution_session.hpp))
is checked at compile time against the following macros to enforce
that the header pulls in **no** vendor SDK:

- `CUDA_VERSION`, `__CUDACC__` — CUDA runtime / Thrust
- `NV_TENSORRT_MAJOR`, `TRT_VERSION` — TensorRT
- `TORCH_VERSION`, `TORCH_API` — LibTorch / PyTorch
- `VITIS_AI_LIBRARY_VERSION`, `VART_API` — Vitis AI runtime / VART

The guard lives in
[`test/unit/execution_session_interface_test.cpp`](../../test/unit/execution_session_interface_test.cpp).
A header change that accidentally introduces any of these tokens will
fail the T1 build.

## Review questions (answered)

> Can `.xmodel` and any associated metadata be represented as bundle
> artifacts without changing the bundle envelope?

Yes — V01-E02 `ModelSpec` carries `artifact_path` + `backend_hint`
strings and an enum `precision_hint`. Vitis AI INT8 calibration
metadata is bundle-format payload, not a runtime concern; the bundle format
bundle schema reserves space for it without revising `ModelSpec`.

> Does the `load` / `prime` / `infer` / `infer_async` / `unload`
> lifecycle cover Vitis AI runner lifetime and any required DPU setup?

Yes — `do_load` covers artifact discovery and runtime setup, `do_prime`
covers DPU runner instantiation, fixed-shape binding, and readiness
checks. The lifecycle state machine
([`docs/architecture/execution-session.md`](execution-session.md))
treats prime as a separate, observable step, which matches Vitis AI's
two-phase boot.

> Are fixed-shape and op-coverage constraints expressible as normalized
> capability flags?

V01-E04 does not own capability publication; that contract lands in
V01-E05-F05 (Backend Adapter Baseline -> capability publication). The
V01-E04 interface does not block expressing fixed-shape or op-coverage
constraints as capability flags later.

> Does the precision schema cover Vitis AI's INT8-oriented quantization
> and calibration requirements?

`PrecisionHint::Int8` and `PrecisionHint::Int4` are already in
[`include/tensorplate/core/model_spec.hpp`](../../include/tensorplate/core/model_spec.hpp).
Calibration metadata is bundle-level, not runtime-level.

> Can DPU-compatible memory needs be handled through adapter-owned
> copies or buffer-plane extensions without changing public `BufferRef`
> semantics?

Yes — `BufferRef` exposes only an opaque id, a byte size, and an
ownership tag. Adapters that require DPU-compatible memory perform an
internal copy from manager-owned storage. Zero-copy is an optimization,
not a correctness dependency; the buffer-plane ownership document
([`buffer-plane.md`](buffer-plane.md)) already records this.

> Does any public runtime header need a Vitis AI, XRT, DPU, or Xilinx
> SDK type?

No. The macro guard above proves this mechanically at build time.

## Sign-off

The V01-E04 interface is compatible with a future Kria/Vitis AI
adapter. No blocking interface fixes are required before V01-E05
adapter implementation. Any v0.1.0 interface revision discovered during
real adapter work routes through the normal "Changes to
`ExecutionSession` methods require written justification and tech lead
review" gate.
