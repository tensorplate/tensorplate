# `observability/`

`tensorplate-observability` — the Rust independent health monitor. The
service ingests `HealthEvent` heartbeats from the serving worker and
`SupervisionEvent` transitions from `tensorplate-agent`, evaluates
heartbeat freshness against a monotonic clock, and emits a local
safe-state event whenever the aggregate state becomes
`degraded` / `failed` / `no_heartbeat`. When configured, it also
publishes a v0.1.0 ROS 2 health topic stub
(`diagnostic_msgs/msg/DiagnosticArray` on `/tensorplate/health`).

## Ownership

- **Layer:** safety / observability (process)
- **Language:** Rust
- **Cargo crate:** `tensorplate-observability` (library + binary)

## Dependency direction

```
serving_worker/  ──(heartbeat events)──>  observability/
agent/           ──(supervision events)──>  observability/
observability/   ──(safe-state event)──>   bounded sink (in-mem / file)
observability/   ──(DiagnosticArray)──>    optional ROS 2 publisher
```

The observability service does not depend on the serving worker's
request path and must not block inference. It receives events through
the `protocol/` contracts only and never reaches into the agent's
durable state.

## Rules

- Heartbeat checks use a monotonic clock. No wall-clock dependency.
- The observability service detects a wedged serving worker without
  requiring agent cooperation.
- Reference safe-state output is local; the ROS 2 health-topic stub is
  optional and disabled by default.
- The listener, safe-state sink, snapshot writer, and ROS 2 publisher
  are all bounded; a slow or absent consumer cannot block heartbeat
  evaluation.

## Components

| Module      | Responsibility                                                |
| ----------- | ------------------------------------------------------------- |
| `config`    | Validated `ObservabilityConfig`; mirrors `config/schemas/observability.json`. |
| `error`     | Typed `ObservabilityError` mapped to stable `ErrorCode`s.     |
| `clock`     | Monotonic clock abstraction + `FakeClock` for tests.          |
| `listener`  | Bounded local event ingestion, schema validation, sequence handling. |
| `heartbeat` | Monotonic heartbeat evaluator and no-heartbeat detector.      |
| `state`     | Aggregator + safe-state event definition.                     |
| `sink`      | In-memory ring + file safe-state sinks.                       |
| `snapshot`  | Versioned status snapshot writer + bounded diagnostics ring.  |
| `ros2`      | `DiagnosticArray` ROS 2 publisher stub.                       |
| `service`   | Composition root. `Service::tick` drives the pipeline.        |

## Running

```
tensorplate-observability --config /etc/tensorplate/observability.json
```

The default config is local-only and never binds a socket, so
`tensorplate-observability --version` is safe to invoke from CI without
any side effects.

## Documentation

The full architecture write-up lives in
`docs/architecture/observability.md` (V01-E10).
