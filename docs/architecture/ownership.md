# Package Ownership and Dependency Direction

This document is the authoritative map of which top-level directory belongs
to which layer, what language it uses, and what it is allowed to depend on.
It is referenced from [`CONTRIBUTING.md`](../../CONTRIBUTING.md) and is
enforced at review time. Changes here require tech lead approval.

## Top-level packages

| Path | Layer | Language | Build artifact | Owner |
| --- | --- | --- | --- | --- |
| `include/tensorplate/` | runtime / public interface | C++20 | header-only | runtime tech lead |
| `runtime/` | runtime core (data plane) | C++20 | `tp_runtime` library | runtime tech lead |
| `serving_worker/` | data plane (process) | C++20 | `tp_serving_worker` binary | runtime tech lead |
| `backends/python_pytorch/` | data plane (out-of-process backend) | Python 3.10+ | `tensorplate-pytorch-backend` package | runtime tech lead + backend owner |
| `sdk/python/` | client / user space | Python 3.10+ | `tensorplate-python` package | SDK owner |
| `agent/` | management plane (process) | Rust | `tensorplate-agent` binary | agent owner |
| `cli/` | management plane (operator client) | Rust | `tensorplate-cli` binary | agent owner |
| `observability/` | safety / observability (process) | Rust | `tensorplate-observability` binary | safety owner |
| `protocol/schemas/` | cross-cutting contract | data | n/a | runtime tech lead + agent owner |
| `protocol/rust/` | cross-cutting contract | Rust | `tensorplate-protocol` library | agent owner |
| `config/schemas/` | cross-cutting contract | data | n/a | runtime tech lead + agent owner |
| `test/` | tests | C++20, Rust | test binaries | reviewers per area |
| `cmake/` | build system | CMake | n/a | runtime tech lead |
| `docs/` | documentation | Markdown | n/a | per-area authors |

## Dependency direction

Dependencies flow downward only. Upward dependencies are forbidden.

```
                   ┌───────────────┐         ┌──────────────────┐
                   │      cli      │────────▶│      agent       │
                   └───────────────┘         └─────────┬────────┘
                                                       │ versioned IPC
                                                       ▼
                                              ┌──────────────────┐
                                              │ serving_worker   │
                                              └─────────┬────────┘
                                                        │
                                                        ▼
                                              ┌──────────────────┐
                                              │     runtime      │
                                              └─────────┬────────┘
                                                        │
                                                        ▼
                                              ┌──────────────────┐
                                              │ adapter internals│
                                              │  (private; under │
                                              │   runtime/src/)  │
                                              └─────────┬────────┘
                                                        │ versioned IPC
                                                        ▼
                                              ┌──────────────────┐
                                              │ backends/        │
                                              │   python_pytorch │
                                              │  (out-of-process)│
                                              └──────────────────┘

  observability/ observes serving_worker/ heartbeats and agent/ events
  through protocol contracts. It is not on the data path.

  protocol/schemas/ is consumed as data by both planes and by
  backends/python_pytorch/ over IPC.
  protocol/rust/   is consumed by agent/, cli/, observability/.
  include/tensorplate/ is consumed by runtime/, serving_worker/, and tests.
```

## Process and protocol boundary

C++ and Rust components communicate through versioned process or protocol
boundaries. v0.1.0 deliberately does not introduce in-process C++/Rust FFI.
If a future milestone requires it, a narrow C ABI with explicit ownership
must be designed; STL types, vendor SDK types, Rust-owned memory, and
`BufferRef` internals must not cross that boundary.

## Allowed dependencies

| Package | May depend on |
| --- | --- |
| `include/tensorplate/` | C++ standard library only |
| `runtime/` | `include/tensorplate/`, approved third-party C++ via vcpkg |
| `serving_worker/` | `runtime/`, `include/tensorplate/`, protocol schemas |
| `agent/` | `protocol/rust/`, approved third-party Rust crates |
| `cli/` | `protocol/rust/`, approved third-party Rust crates |
| `observability/` | `protocol/rust/`, approved third-party Rust crates |
| `protocol/rust/` | approved third-party Rust crates only |
| `backends/python_pytorch/` | `protocol/schemas/` (IPC contract), approved Python wheels (PyTorch, etc., system-provided) |
| `sdk/python/` | standard library and approved third-party Python wheels for client-side HTTP, serialization, and vision helpers |

## Forbidden dependencies

- `runtime/` must not depend on `serving_worker/`, `agent/`, `cli/`, or
  `observability/`.
- `agent/` must not link against `runtime/` or any C++ component.
- `cli/` must not talk to `serving_worker/` directly. All state-changing
  calls route through `agent/`.
- `observability/` must not depend on `agent/` to detect a wedged
  serving worker.
- No vendor SDK type (TensorRT, PyTorch/LibTorch, CUDA) appears in
  `include/tensorplate/`.
- `backends/python_pytorch/` must not import any C++ runtime module or
  link against `runtime/`, `serving_worker/`, `agent/`, `cli/`, or
  `observability/`. It speaks the runtime over the IPC contract under
  `protocol/schemas/` only.

## Review gates

- Changes under `include/tensorplate/` require tech lead approval.
- Changes to `ExecutionSession` methods require written justification and tech
  lead review.
- New adapters require T3 contract test evidence before merge.
- New cross-layer dependencies require an update to this document.
