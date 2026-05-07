# `runtime/`

Core C++ inference runtime. Owns the `ModelLoader` / `ExecutionSession`
NVI implementation, scheduler, buffer plane, and adapter registry.

## Ownership

- **Layer:** runtime core (data plane)
- **Language:** C++20
- **CMake target:** `tp_runtime` (alias `tp::runtime`)
- **Public headers:** [`include/tensorplate/`](../include/tensorplate/)

## Layout

- `src/` — runtime implementation translation units.
- `include/` — internal (non-public) headers shared inside the runtime layer.

## Dependency direction

```
serving_worker/  ─┐
                  ├──>  runtime/  ──>  adapter internals (private)
agent/ (IPC) ─────┘
```

`runtime/` depends only on the standard library and approved third-party
libraries declared in `vcpkg.json`. It must not depend on `serving_worker/`,
`agent/`, `cli/`, or `observability/`.

## Rules

- Hardware-boundary operations return `Result<T>`. Exceptions are not thrown
  at or below the `ModelLoader` interface.
- Cross-layer payloads use `BufferRef` and `TensorView`.
- Hardware resources are owned through RAII wrappers private to adapter code.
- No backend names, device paths, or magic numbers are hardcoded.

V01-E01-F01 only creates the skeleton. Implementation lands in V01-E02
through V01-E06.
