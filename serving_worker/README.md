# `serving_worker/`

`tensorplate-serving` — the C++ data-plane process. Hosts the local HTTP
endpoint, request router, and the runtime stack defined in [`runtime/`](../runtime/).

## Ownership

- **Layer:** data plane (process)
- **Language:** C++20
- **CMake target:** `tp_serving_worker` (binary; alias `tp::serving_worker`)
- **Supervised by:** `tensorplate-agent`

## Dependency direction

```
serving_worker/  ──>  runtime/  ──>  adapter internals
```

`serving_worker/` may depend on `runtime/` and on protocol types under
[`protocol/`](../protocol/). It must not depend on `agent/`, `cli/`, or
`observability/` (those communicate via versioned IPC and HTTP, not link-time
dependencies).

## Layout

- `src/` — process entrypoint, HTTP routing, request normalization, lifecycle.
- `include/` — internal headers shared inside the worker.

## Rules

- Loopback-only by default. Public network exposure requires explicit config.
- Graceful shutdown: stop admission, drain or cancel in-flight work,
  release `BufferRef` handles deterministically.
- No vendor SDK types leak above the runtime adapter boundary.

V01-E01-F01 creates the skeleton. The HTTP endpoint and runtime wiring land
in V01-E07.
