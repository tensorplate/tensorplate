# `observability/`

`tensorplate-observability` — the Rust independent health monitor. Listens
for serving-worker heartbeats and reports `ready` / `degraded` / `failed` /
`no-heartbeat` state without sitting on the serving request path.

## Ownership

- **Layer:** safety / observability (process)
- **Language:** Rust
- **Cargo crate:** `tensorplate-observability` (binary)

## Dependency direction

```
serving_worker/  ──(heartbeat events)──>  observability/
agent/           ──(worker lifecycle events)──>  observability/
```

`observability/` does not depend on the serving worker's request path and
must not block inference. It receives events through the `protocol/` IPC
contracts.

## Rules

- Heartbeat checks use a monotonic clock. No wall-clock dependency.
- The observability service detects a wedged serving worker without
  requiring agent cooperation.
- Reference safe-state output is local; ROS 2 health-topic stubs are
  optional in v0.1.

Implementation lands in V01-E10.
