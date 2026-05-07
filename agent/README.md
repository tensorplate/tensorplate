# `agent/`

`tensorplate-agent` — the Rust management-plane service. Owns desired state,
the deploy transaction state machine, bundle verification, and serving-worker
supervision.

## Ownership

- **Layer:** management plane (process)
- **Language:** Rust (stable toolchain pinned in `rust-toolchain.toml` in a
  later milestone)
- **Cargo crate:** `tensorplate-agent` (binary)

## Dependency direction

The agent is upstream of the serving worker and communicates with it through
versioned local IPC, not link-time dependencies:

```
cli/ ─────(local control API)─────>  agent/  ─────(versioned IPC)─────>  serving_worker/
                                       │
                                       └──(events)──>  observability/
```

`agent/` depends on `protocol/rust/` for shared schemas. It must not link
against `runtime/` or any C++ component.

## Rules

- Deploy and rollback use desired-state reconciliation, not command replay.
- Persistent state is durable across restarts; transactions resume from the
  last persisted phase.
- The agent never mutates the serving worker's data plane directly; it
  starts, stops, and supervises the worker process.

V01-E01-F01 creates the skeleton. The deploy transaction lands in V01-E08
and worker supervision in V01-E09.
