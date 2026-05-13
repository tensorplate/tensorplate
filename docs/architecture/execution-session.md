# Execution Session

This document is the authoritative description of the public execution-session
contract used by every TensorPlate backend adapter. It is referenced from
[`CONTRIBUTING.md`](../../CONTRIBUTING.md) and tested by the V01-E04 test
tree. Changes here require tech lead approval.

## Canonical name decision (V01-E04-F01-T01)

The canonical public C++ type is **`tensorplate::ExecutionSession`**.

Context:

- The v0.1.0 architecture and roadmap
  ([`tensorplate-internals/planning/v0.1.0/tensorplate-oss-v0.1-architecture-and-roadmap.md`](../../tensorplate-internals/planning/v0.1.0/tensorplate-oss-v0.1-architecture-and-roadmap.md))
  refers to the public lifecycle interface as `ExecutionSession`.
- An older draft of the implementation guidelines
  (`tensorplate-internals/guidelines/implementation-guidlines.md`) referred to
  the same concept as `ModelLoader`. That alternate spelling is **not**
  introduced as a real type, an alias, or a compatibility shim. The
  guideline document will be updated; until it is, treat every mention of
  `ModelLoader` in the implementation guidelines as a synonym for the public
  `ExecutionSession`.

There is exactly one canonical public method set:

```
load            // load the model artifact
prime           // adapter readiness / fixed-shape binding / warmup
infer           // synchronous inference
infer_async     // method shape always present; may return typed Unsupported
unload          // release session-owned state
is_ready        // observer
backend_name    // observer
```

No second interface contract is introduced. Any future renaming requires
tech lead approval and a written justification per the "Changes to
`ExecutionSession` methods require written justification and tech lead
review" rule in [`ownership.md`](ownership.md).

## Non-virtual interface (NVI) pattern

The public lifecycle methods are non-virtual wrappers. Adapters override
**protected** `do_*` implementation methods, not the public methods.

```text
ExecutionSession                  (public, non-virtual)
   load        ─► do_load         (protected, pure virtual)
   prime       ─► do_prime        (protected, virtual; default no-op)
   infer       ─► do_infer        (protected, pure virtual)
   infer_async ─► do_infer_async  (protected, virtual; native async only)
   unload      ─► do_unload       (protected, virtual; default no-op)
```

The NVI wrapper is responsible — before delegating to the adapter — for:

1. **Readiness gates.** Reject lifecycle calls that violate the state
   machine (e.g. `infer` before `prime` returns `Error::Code::NotReady`).
2. **Request validation.** Reject malformed `InferRequest`s, requests with
   released or missing input buffers, and tensor byte windows that do not
   fit inside their owning buffers.
3. **Monotonic latency stamping.** Stamp `execution_latency` around every
   call to `do_infer`, on both success and failure paths, using
   `std::chrono::steady_clock`.
4. **Output validation.** Reject adapter-published outputs that carry
   released buffers, out-of-bounds tensor windows, or duplicate names.
   Partial outputs are released through the buffer plane on failure.
5. **Event emission.** Emit paired start/end events for every lifecycle
   call, plus failure events for validation rejections and adapter
   failures.

Adapters therefore cannot bypass timing, validation, or event hooks. The
guarantees apply uniformly to TensorRT, LibTorch, the Python/PyTorch
sidecar, and a future Vitis AI adapter.

## Lifecycle state machine

```text
       Unloaded ──load──► Loaded ──prime──► Ready
           ▲                │                  │
           │                │                  │
           └─────unload─────┴─unload─◄─unload──┘
                                                ▲
                                                │
                                              infer
                                                │
                                                ▼
                                              Ready
```

Failure state:

- A failed `load` leaves the session in `Failed` with the typed error
  recorded. The state can be recovered to `Unloaded` only by `unload`.
- A failed `prime` returns to `Loaded` if the adapter reports a
  recoverable error (e.g. `ConfigInvalid`); otherwise the session
  transitions to `Failed`.
- A failed `infer` does not transition state; the session remains `Ready`
  and the typed error is returned in the `InferResult`.

State name mapping (stable, lowercase, snake_case):

| C++ enum | Wire string | Notes |
| --- | --- | --- |
| `SessionState::Unloaded` | `unloaded` | Initial state. |
| `SessionState::Loaded` | `loaded` | Model loaded, not yet primed. |
| `SessionState::Ready` | `ready` | Primed; `infer` is permitted. |
| `SessionState::Failed` | `failed` | Last lifecycle call failed; recover with `unload`. |

These names are reusable by the future worker-status schema without
revision.

## Async method shape (V01-E04-F05)

`infer_async` is always present on the public interface from v0.1.0. The
wrapper checks readiness and validates the request first. If the adapter
does not report native async support, the wrapper returns
`Error::Code::Unsupported`, emits `UnsupportedAsync`, and does not
dispatch to adapter execution. Adapters that do implement native async
opt in through the protected capability hook and then receive the same
timing and event hooks as the sync path.

The async return type is `Result<AsyncInferHandle>`. The handle carries
the originating `request_id` plus a session-scoped monotonically
increasing `async_id` so that later scheduler/cancellation work can
identify in-flight async requests without modifying the public interface.
It does not assume threads, CUDA streams, Python futures, or
backend-specific handles.

## Non-GPU compatibility (V01-E04-F07-T02)

The public `load` and `prime` methods are general enough to represent:

- **TensorRT** — engine load + execution-context creation + binding setup.
- **LibTorch** — `torch::jit::load` + module warmup.
- **Python/PyTorch sidecar** — sidecar fork + `LoadModel` + `Prime` IPC
  round-trips.
- **Vitis AI / Kria (future)** — `.xmodel` discovery, DPU runner
  instantiation, fixed-shape binding, INT8 calibration metadata, and
  readiness checks.

No public method or value object assumes CUDA, TensorRT, PyTorch/LibTorch,
Vitis AI, XRT, DPU, or any other vendor SDK type.

## Event taxonomy (V01-E04-F06)

Session events are emitted by the NVI wrapper through a small
`SessionEventSink` interface. Events are fire-and-forget: a slow or
throwing sink cannot block or corrupt session state, because emission is
wrapped in a `try { ... } catch (...) {}` and runs outside the inference
hot path's critical section.

| Event kind | Wire string | Emitted when |
| --- | --- | --- |
| `LoadStart` / `LoadEnd` / `LoadFailed` | `load_start` / `load_end` / `load_failed` | `load` wrapper enters / completes / fails |
| `PrimeStart` / `PrimeEnd` / `PrimeFailed` | `prime_start` / `prime_end` / `prime_failed` | `prime` wrapper enters / completes / fails |
| `InferStart` / `InferEnd` / `InferFailed` | `infer_start` / `infer_end` / `infer_failed` | `infer` wrapper enters / completes / fails |
| `InferAsyncStart` / `InferAsyncEnd` / `InferAsyncFailed` | `infer_async_start` / `infer_async_end` / `infer_async_failed` | `infer_async` wrapper enters / completes / fails |
| `UnloadStart` / `UnloadEnd` / `UnloadFailed` | `unload_start` / `unload_end` / `unload_failed` | `unload` wrapper enters / completes / fails |
| `ValidationFailed` | `validation_failed` | NVI rejects request before dispatch |
| `UnsupportedAsync` | `unsupported_async` | `infer_async` returns wrapper-level Unsupported |

Each event carries bounded fields: `kind`, `backend_name`, optional
`model_id`, optional `request_id`, optional `error_code`, monotonic
`duration`, and `state_after`. No raw payload bytes appear in any event.

## Review gates

- Changes under `include/tensorplate/core/execution_session.hpp` require
  tech lead approval.
- New public virtual methods on `ExecutionSession` are not allowed without
  tech lead sign-off and an updated mock conformance harness.
- New adapters must satisfy the mock conformance test suite under
  `test/contract/` before they merge.
