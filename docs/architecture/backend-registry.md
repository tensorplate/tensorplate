# Backend capability model and registry

> **Status:** v0.1.0 V01-E05-F01.
> **Source:** [`include/tensorplate/backend/capability.hpp`](../../include/tensorplate/backend/capability.hpp),
> [`include/tensorplate/backend/registry.hpp`](../../include/tensorplate/backend/registry.hpp),
> [`include/tensorplate/backend/builtin.hpp`](../../include/tensorplate/backend/builtin.hpp).

V01-E05-F01 introduces the vendor-neutral capability record and the
adapter registry. Every concrete adapter (TensorRT in V01-E05-F02,
LibTorch in V01-E05-F03, Python/PyTorch sidecar in V01-E05-F05, future
Vitis AI) registers a `BackendEntry` carrying:

1. a stable backend key string (e.g. `tensorrt`, `libtorch`, `python_pytorch`),
2. a `BackendCapability` value object that publishes precision support,
   shape support, async/generation/streaming/KV-cache flags, op-coverage
   percentage, and memory estimate/limit, and
3. an `ExecutionSessionFactory` closure that builds a fresh, unloaded
   `ExecutionSession` when the registry is asked to create one.

## Why a separate capability record?

A single `backend_name` string is not enough for the bundle pipeline,
status reporting, or the conformance harness:

- The agent's deploy transaction has to reject bundles whose declared
  `backend_hint` is missing on the device — and it has to do so
  *before* it stages anything.
- The agent has to reject bundles whose declared `precision_hint` is
  not supported by the backend. v0.1.0 deliberately does not silently
  downgrade FP16-only requests to FP32 (or any other path).
- `tensorplate status` and `tensorplate doctor` have to enumerate which
  adapters are compiled into this build and what each can run.
- The adapter conformance harness has to know whether to expect
  `Error::Code::Unsupported` from `infer_async`, because the public
  method shape is always present but adapter support is opt-in.

`BackendCapability` is plain data; it has no vendor SDK includes and no
raw hardware handles. It can be serialized via
`protocol/schemas/backend_capability.json` so the agent and the
serving worker can exchange capability records across process
boundaries without leaking adapter-specific types.

## Registry semantics

`BackendRegistry` is thread-safe and stores entries by value. Lookup,
registration, and deregistration take a mutex; the factory closure is
*invoked outside the mutex* so adapter initialization (which may open
files, spawn sidecars, or probe a GPU) cannot deadlock the registry.

Errors:

| Situation                                       | Error code      |
| ------------------------------------------------ | --------------- |
| Empty backend name on registration              | `config_invalid`|
| Null factory closure                            | `config_invalid`|
| Capability `backend_name` mismatches the entry  | `config_invalid`|
| Duplicate registration of the same key          | `internal`      |
| Lookup / `create_session` / `capability` miss   | `unsupported`   |
| Declared precision not in supported list        | `unsupported`   |

`validate_backend_hint(spec)` is the entry point the bundle pipeline
calls. It rejects unknown backends and rejects unsupported precisions;
it never picks a different backend on the caller's behalf and never
falls back at inference time. The error context carries
`backend=<name>, requested=<precision>` so deploy failures point at the
exact bundle field to fix.

`BackendRegistry::global()` is a process-wide instance used by
production code. Tests build local registries so they do not leak
state across unit tests.

## Built-in adapter registration

`register_builtin_backends(BackendRegistry&)` registers every adapter
whose CMake feature flag is set in the current build of `tp_runtime`:

| Flag                                | Adapter registered             |
| ----------------------------------- | ------------------------------ |
| `TP_ENABLE_TENSORRT`                | `tensorrt` (V01-E05-F02)       |
| `TP_ENABLE_LIBTORCH`                | `libtorch` (V01-E05-F03)       |
| `TP_ENABLE_PYTHON_PYTORCH_SIDECAR`  | `python_pytorch` (V01-E05-F05) |

Adapter registration is explicit rather than static-init based so tests
can construct an empty registry and bring up only the subset they need
to exercise. The flags are OFF by default in V01-E05-F01; each
subsequent feature flips its own flag on once its adapter implementation
lands.

## Non-goals

V01-E05-F01 explicitly does *not* implement:

- adapter initialization-time probing (the factory may surface
  `LoadFailed` itself, but the registry does not),
- capability serialization out of process (the JSON Schema is
  declared so the agent/IPC layers in V01-E07/V01-E08 can adopt it
  without revisiting this header),
- backend selection or scheduling decisions on top of capability data,
- runtime feature flag flipping outside the build system.
