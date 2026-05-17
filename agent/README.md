# `agent/`

`tensorplate-agent` — the Rust management-plane service. Owns desired state,
the deploy transaction state machine, bundle verification, and serving-worker
supervision.

## Ownership

- **Layer:** management plane (process)
- **Language:** Rust (stable toolchain pinned in `rust-toolchain.toml`)
- **Cargo crate:** `tensorplate-agent` (library `tensorplate_agent` + binary
  `tensorplate-agent`)

## Dependency direction

The agent is upstream of the serving worker and communicates with it through
versioned local IPC, not link-time dependencies:

```
cli/ ─────(local control API)─────>  agent/  ─────(worker control)─────>  serving_worker/
                                       │
                                       └──(events)──>  observability/
```

`agent/` depends on `protocol/rust/` for shared schemas. It must not link
against `runtime/` or any C++ component.

## V01-E08 contracts

Architecture and on-the-wire details live in
[`docs/architecture/agent.md`](../docs/architecture/agent.md). Quick map:

- Local control transport: Unix domain socket (default) speaking
  newline-delimited JSON. See
  [`protocol/schemas/agent_control.json`](../protocol/schemas/agent_control.json).
- Durable state file: `state.json` + `state.json.bak`. See
  [`protocol/schemas/agent_state.json`](../protocol/schemas/agent_state.json).
- Bundle envelope verified at deploy time:
  [`protocol/schemas/bundle_manifest.json`](../protocol/schemas/bundle_manifest.json).
- Agent → serving worker control:
  [`protocol/schemas/worker_control.json`](../protocol/schemas/worker_control.json).
- Agent runtime config:
  [`config/schemas/agent.json`](../config/schemas/agent.json).

## Rules

- Deploy and rollback use desired-state reconciliation, not command replay.
- Persistent state is durable across restarts; transactions resume from the
  last persisted phase.
- The agent never mutates the serving worker's data plane directly; it
  stages a verified bundle and asks the worker to prepare/warm/promote
  through the typed `WorkerControl` trait. `worker.mode=mock` uses the
  deterministic test implementation; `worker.mode=process` spawns and
  health-checks `tensorplate-serving`.
- Bundle verification rejects unknown / unavailable backends; the runtime
  never falls back heuristically.
- Failed candidates are quarantined; the active deployment is preserved.

## Running

```bash
cargo run -p tensorplate-agent -- --config /etc/tensorplate/agent.json
```

The agent applies startup recovery before opening the control socket, prints
its bound address on stderr, and runs until killed. v0.1.0 relies on systemd
/ supervisor to deliver SIGTERM for shutdown (V01-E14 ships the systemd unit).

## Tests

- Unit tests live in each module's `#[cfg(test)] mod tests` block.
- Integration tests live in `tests/` and drive the agent end-to-end against
  the in-tree `MockWorkerControl`. The UDS round-trip suite
  (`tests/control_api_uds.rs`) covers the full deploy / status / rollback
  flow through real socket connections.
