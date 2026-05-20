# Structured Log Schema (V01-E12-F01)

The structured log schema is the single envelope used by every v0.1
TensorPlate component for operator-visible events. Producers do not
build component-specific log shapes; they tag events with
[`LogComponent`](../../protocol/rust/src/log_event.rs) and a stable
event name and let the bounded-context sanitiser handle the rest.

Wire format: [`protocol/schemas/log_event.json`](../../protocol/schemas/log_event.json).
Rust mirror: [`protocol::log_event`](../../protocol/rust/src/log_event.rs).

## Required fields

| Field                    | Purpose                                                              |
| ------------------------ | -------------------------------------------------------------------- |
| `schema_version`         | Pinned to `"0.1"`; readers reject other values.                      |
| `component`              | One of `agent`, `serving_worker`, `runtime`, `adapter`, `python_pytorch_sidecar`, `observability`, `cli`. |
| `event`                  | Stable lowercase dot-separated name, e.g. `deploy.warmup_complete`.  |
| `level`                  | `trace / debug / info / warn / error`.                              |
| `monotonic_timestamp_ns` | Monotonic timestamp; sampled from `Instant::now()` or `steady_clock::now()`. |

## Optional context fields

| Field             | When set                                                                |
| ----------------- | ----------------------------------------------------------------------- |
| `correlation_id`  | When the event participates in deploy / inference / supervision threading. |
| `request_id`      | Serving ingress events.                                                 |
| `transaction_id`  | Agent deploy transactions.                                              |
| `deployment_id`   | Any event with an active / candidate deployment.                        |
| `model_name`      | Bundle-named model.                                                     |
| `model_class`     | One of `vision`, `speech`, `language`, `vla`, `embedding`, `custom`.    |
| `backend`         | Bounded registry name (`python_pytorch`, `libtorch`, `tensorrt`, `mock`). |
| `error_code`      | Maps to `protocol/schemas/error.json`.                                  |
| `failure_reason`  | Maps to `protocol/schemas/failure_reason.json`.                         |
| `duration_ms`     | Monotonic duration covered by the event.                                |
| `context`         | Bounded structured context map (≤ 16 entries, ≤ 256-byte strings).      |

## Event catalog

The v0.1 catalog seeds these event names per component. Producers extend
the catalog by appending; existing names are stable for the life of
v0.1.

### Agent

`deploy.received`, `deploy.bundle_validated`, `deploy.bundle_invalid`,
`deploy.staged`, `deploy.capacity_checked`, `deploy.warmup_started`,
`deploy.warmup_complete`, `deploy.promotion_started`,
`deploy.promotion_complete`, `deploy.failed`, `deploy.rolled_back`,
`agent.startup`, `agent.shutdown`, `recovery.attempt`.

### Serving worker / runtime / adapter

`serving.startup`, `serving.config_loaded`, `request.accepted`,
`request.rejected`, `scheduler.admitted`, `infer.ok`, `infer.timeout`,
`infer.failed`, `infer.cancelled`, `adapter.load`, `adapter.prime`,
`adapter.unsupported`, `adapter.unload`, `serving.shutdown`,
`serving.health_changed`.

### Python/PyTorch sidecar

`sidecar.startup`, `sidecar.load`, `sidecar.prime`, `sidecar.infer`,
`sidecar.cancel`, `sidecar.timeout`, `sidecar.health_check`,
`sidecar.unload`, `sidecar.process_exit`.

### Observability service

`observability.heartbeat_accepted`, `observability.missed_heartbeat`,
`observability.state_transition`, `observability.safe_state_emitted`,
`observability.export_failure`, `observability.sink_backpressure`.

### CLI

`cli.doctor.check`, `cli.deploy.submitted`, `cli.deploy.completed`,
`cli.rollback.submitted`, `cli.status.read`, `cli.logs.tail`,
`cli.infer.completed`.

## Producer behaviour

Producers go through
[`LogEmitter`](../../observability/src/log_emitter.rs) (Rust) or the
equivalent C++ wrapper. The emitter:

1. Tags every event with the configured component and the supplied
   level.
2. Stamps `monotonic_timestamp_ns` from the supplied
   [`MonotonicClock`](../../observability/src/clock.rs) (or
   `steady_clock` in C++).
3. Runs `LogEvent::insert_context` so oversize strings, NUL bytes, and
   control-character "secrets" are sanitised at emit time.
4. Drops events that violate the bounded-context policy with a typed
   counter; the producer is never blocked.
5. Forwards the event to
   [`DiagnosticsRetention`](../../observability/src/retention.rs).

## Reader behaviour

Readers reject unknown schema versions with a typed
[`DecodeError::UnsupportedSchemaVersion`](../../protocol/rust/src/lib.rs).
The CLI `tensorplate logs` reader applies the same sanitiser before
rendering so a malformed file never panics the operator session.
