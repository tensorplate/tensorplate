# Observability Service (V01-E10)

V01-E10 makes `tensorplate-observability` an independent process that
watches the serving worker's heartbeat and the agent's worker
supervision events, evaluates them against a monotonic clock, and emits
a local safe-state signal when the aggregate state becomes
`degraded` / `failed` / `no_heartbeat`. The service:

- runs independently of the serving request path and the V01-E08 deploy
  transaction path,
- detects a wedged serving worker without depending on the agent's
  supervision loop,
- exposes a minimal local status snapshot for the V01-E11 CLI and the
  release validation harness,
- optionally publishes a v0.1.0 ROS 2 health-topic stub
  (`diagnostic_msgs/msg/DiagnosticArray` on `/tensorplate/health`).

This document is the single source of truth for the observability
service's contract with the agent, the serving worker, and any
downstream consumer.

## Layering

```
   serving worker (V01-E07)        agent (V01-E08 / V01-E09)
        │                                 │
        │ HealthEvent / WorkerStatus      │ SupervisionEvent
        │ (heartbeat / ready /            │ (worker_started / worker_ready /
        │  degraded / failed /            │  worker_exit / worker_failed /
        │  missed_deadline / overload /   │  crash_loop_entered / ...)
        │  status metrics)                │
        ▼                                 ▼
   ┌─────────────────────────────────────────────────────────┐
   │   tensorplate-observability process                     │
   │                                                         │
   │   EventListener  ─►  HeartbeatEvaluator                 │
   │   (bounded in-process; UDS reserved)│                   │
   │                                     ▼                   │
   │                              Aggregator (state) ─►      │
   │                                 │                       │
   │                                 ├─► InMemorySafeStateSink
   │                                 │   (or file sink)      │
   │                                 │                       │
   │                                 ├─► SnapshotWriter      │
   │                                 │   (atomic-replace)    │
   │                                 │                       │
   │                                 └─► Ros2HealthPublisher │
   │                                     (DiagnosticArray)   │
   └─────────────────────────────────────────────────────────┘
```

All listeners, sinks, and the snapshot writer are bounded. A slow or
absent consumer never blocks heartbeat evaluation, and the observability
process never sits on the serving request path.

## State model

The aggregator produces one of four states. State names are wire-stable
and mirrored in `protocol/schemas/safe_state_event.json` and
`protocol/schemas/observability_status.json`:

| Observability state | Meaning                                                    | ROS 2 level |
| ------------------- | ---------------------------------------------------------- | ----------- |
| `ready`             | Heartbeat fresh; serving / agent state ready.              | `OK`        |
| `degraded`          | Worker not-ready, overload, missed-deadline, stale heart.  | `WARN`      |
| `failed`            | Failed serving state, crash-loop, explicit worker failure. | `ERROR`     |
| `no_heartbeat`      | Heartbeat missed past threshold (monotonic).               | `STALE`     |

### Precedence

When more than one signal applies at the same time the aggregator
selects in the following order:

1. `no_heartbeat` — the wedged-worker signal V01-E10 must surface even
   without agent cooperation.
2. `failed` — explicit failed signals from the serving worker or agent
   supervisor.
3. `degraded` — explicit degraded / overload / missed-deadline /
   not-ready signals, or stale heartbeat below the no-heartbeat
   threshold.
4. `ready` — everything else.

The reason an event was emitted is captured explicitly through
`SafeStateReason` (`transition`, `periodic`, `heartbeat_missing`,
`serving_failed`, `serving_degraded`, `crash_loop`, `worker_exit`,
`overload`, `recovery`) so consumers filter without parsing free-text.

## Monotonic heartbeat semantics

All freshness decisions use a `MonotonicClock`:

- `expected_interval_ms` (default `1000`) — interval the producer is
  expected to emit at.
- `grace_ms` (default `250`) — grace window before a missed heartbeat
  is counted.
- `missed_threshold` (default `3`) — number of consecutive missed
  heartbeats before the source flips to `no_heartbeat`.
- `recovery_heartbeats` (default `1`) — heartbeats required after a
  `no_heartbeat` transition before returning to `ready`.

Wall-clock changes never feed the evaluator; tests inject a `FakeClock`
to drive every threshold deterministically.

The production binary does not synthesize serving-worker heartbeats.
`Service::emit_internal_heartbeat()` is only for tests and deployments
that explicitly set `primary_source=internal`; the default
`primary_source=serving_worker` must be refreshed by worker events so an
absent or wedged worker can transition to `no_heartbeat`.

## Safe-state events

Whenever the aggregate state transitions to a non-ready state, the
aggregator emits a `SafeStateEvent` to the configured sink. With
`safe_state.periodic_ms` set, the aggregator also re-emits the current
state on every interval the state is not `ready` so consumers that
started late can synchronise. Each event carries the full
`agent_state` / `serving_state` / `active_deployment` / `backend` /
`missed_heartbeat_count` / `missed_deadline_rate` / `queue_depth` /
`last_error_code` / `monotonic_age_ms` context required to decide next
steps without a follow-up query.

The schema is `protocol/schemas/safe_state_event.json`.

## Status snapshot

The status snapshot mirrors `protocol/schemas/observability_status.json`
and surfaces:

- `observability_state`, `agent_state`, `serving_state`
- `active_deployment`, `backend`
- `missed_heartbeat_count`, `missed_deadline_rate`, `queue_depth`,
  `last_error_code`
- `last_event_sequence`, `last_heartbeat_age_ms`
- `safe_state_sink` / `ros2_publisher` / `listener` counters
- bounded `diagnostics` ring (recent transitions + recent errors)

File-backed snapshots use atomic-replace (`*.partial` -> rename) so
readers never observe partial records.

## ROS 2 health topic stub

When `ros2_health.enabled = true` the service publishes
`diagnostic_msgs/msg/DiagnosticArray` on `ros2_health.topic`
(default `/tensorplate/health`). Each message contains exactly one
`DiagnosticStatus` named `tensorplate/runtime` with the following
key-values:

- `agent_state`
- `serving_state`
- `observability_state`
- `active_deployment`
- `backend`
- `missed_heartbeat_count`
- `missed_deadline_rate`
- `queue_depth`
- `last_error_code`

The publisher emits on every state change AND on the configured
`interval_ms`. v0.1.0 ships the in-process mock backend so the stub
compiles and runs in environments without a ROS 2 distribution; the
native `rclrs`-backed publisher lands post-v0.1.0 and replaces the mock
behind the same `Ros2HealthPublisher` surface.

## Configuration

The schema lives at `config/schemas/observability.json`. Defaults are
local-only: no hosted-platform connectivity, no UDS binding, the ROS 2
publisher disabled. A minimal config can be a single
`{"schema_version": "0.1"}` document.

## Independence from the agent

The observability service deliberately treats agent supervision events
as *enrichment*. The heartbeat evaluator, the no-heartbeat transition,
and the safe-state sink all work without any supervision event flowing.
This is validated by:

- `service::tests::missing_heartbeat_transitions_to_no_heartbeat_without_agent_input`
- `observability_failure_injection::no_agent_input_proves_independent_heartbeat_detection`

The V01-E10 acceptance criterion — "Observability service does not
require the agent to detect a wedged serving worker" — is met by these
two tests.

## Non-goals

V01-E10 deliberately excludes:

- Hosted observability, OTLP, deep tracing — V01-E12.
- `tensorplate status` operator UX — V01-E11.
- The full ROS 2 serving package — never in v0.1.0.
- Persistent unbounded logs, request payloads, tensor payloads — never
  in v0.1.0.
- GPIO / hardware safe-state actuators — out of scope unless a target
  integration requires it later.
