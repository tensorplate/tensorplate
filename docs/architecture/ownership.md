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
                                              └──────────────────┘

  observability/ observes serving_worker/ heartbeats and agent/ events
  through protocol contracts. It is not on the data path.

  protocol/schemas/ is consumed as data by both planes.
  protocol/rust/   is consumed by agent/, cli/, observability/.
  include/tensorplate/ is consumed by runtime/, serving_worker/, and tests.
```

## Process and protocol boundary

C++ and Rust components communicate through versioned process or protocol
boundaries. v0.1 deliberately does not introduce in-process C++/Rust FFI.
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

## Forbidden dependencies

- `runtime/` must not depend on `serving_worker/`, `agent/`, `cli/`, or
  `observability/`.
- `agent/` must not link against `runtime/` or any C++ component.
- `cli/` must not talk to `serving_worker/` directly. All state-changing
  calls route through `agent/`.
- `observability/` must not depend on `agent/` to detect a wedged
  serving worker.
- No vendor SDK type (TensorRT, ONNX Runtime, CUDA) appears in
  `include/tensorplate/`.

## Review gates

- Changes under `include/tensorplate/` require tech lead approval.
- Changes to `ModelLoader` methods require written justification and tech
  lead review.
- New adapters require T3 contract test evidence before merge.
- New cross-layer dependencies require an update to this document.
