# Agent Worker Supervision (V01-E09)

V01-E09 makes `tensorplate-agent` the owner of the
`tensorplate-serving` process lifecycle. The supervisor:

- launches one worker process for the desired active deployment,
- observes process liveness and serving readiness through separate
  channels,
- restarts isolated crashes with bounded exponential backoff,
- enters a terminal **crash-loop** state after a configured rolling
  threshold instead of restarting indefinitely,
- emits a bounded supervision-event stream consumed by the V01-E10
  observability service and the V01-E11 status command,
- coordinates with V01-E08 deploy / rollback transactions so a worker
  crash inside an in-flight candidate is reported as a candidate
  failure (the active deployment is preserved).

This document is the single source of truth for the supervisor's
contract with `Coordinator`, the observability service, and the CLI.

## Layering

```
   tensorplate-agent process
   ┌────────────────────────────────────────────────────────────┐
   │   ┌────────────────────┐                                   │
   │   │   Coordinator      │  deploy / rollback transaction    │
   │   │   (V01-E08)        │  ── notify_supervisor_promotion ─►│
   │   └────────────────────┘                                   │
   │                                                            │
   │   ┌────────────────────┐                                   │
   │   │ WorkerSupervisor   │  tick(now) state machine          │
   │   │ (V01-E09)          │  ── SupervisionEvent ──► sink ───►│  V01-E10
   │   └────────────────────┘                                   │
   │      │            │                                        │
   │      │            │  HTTP / loopback                       │
   │      ▼            ▼                                        │
   │   ┌─────────┐  ┌──────────┐                                │
   │   │ Worker  │  │ Readiness│                                │
   │   │ Process │  │  Probe   │                                │
   │   └─────────┘  └──────────┘                                │
   └────────────────────────────────────────────────────────────┘
              │
              ▼
   tensorplate-serving (V01-E07)
```

## State machine

The supervisor's supervision state advances through these phases. All
names are wire-stable: the V01-E11 CLI and the V01-E10 observability
service consume them through `SupervisionServingState` in
`protocol/schemas/supervision_event.json`.

```
no_active_deployment ── set_desired_active() ──► starting
                                                   │
                                                   ▼
                              ┌─── ready ◄──── running ────► degraded ──► failed
                              │      │           │                          │
                              │      │           ▼                          │
                              │      │      not_ready_timeout               │
                              │      ▼                                      │
                              │   exit_after_ready                          │
                              │      │                                      │
                              │      ▼                                      ▼
                              └──► awaiting_restart ───► (launch) ─► (loop)
                                          │
                                          │  threshold reached
                                          ▼
                                       crash_loop  (terminal)
                                          │
                                          │  recover_after_operator_action()
                                          ▼
                                       starting
```

Two terminal states require operator action to leave:

- `failed` — surfaced when an unrecoverable launch failure prevents the
  supervisor from owning a worker (missing binary, unsupported policy).
- `crash_loop` — surfaced when the rolling restart counter exceeds the
  configured `threshold` inside `window_ms`.

Operator-action recovery triggers (`recover_after_operator_action`):

- a fresh deploy through `Coordinator::deploy`,
- a rollback through `Coordinator::rollback`,
- a manual `recover_after_operator_action()` invocation through
  whichever future control surface lights it up (V01-E11 reserves the
  CLI verb).

## Restart policy

The default policy is bounded exponential backoff with a rolling-window
crash-loop detector. Knobs live under `supervision.restart_policy` in
the agent config schema (`config/schemas/agent.json`):

| Knob | Default | Effect |
| --- | --- | --- |
| `initial_delay_ms` | 500 | First backoff delay after an unexpected exit. |
| `multiplier_hundredths` | 200 (2.0x) | Geometric multiplier applied to the running delay. |
| `max_delay_ms` | 30 000 | Cap on the computed delay. |
| `window_ms` | 300 000 (5 min) | Rolling window inside which failures count toward the threshold. |
| `threshold` | 5 | Failures inside the window that trigger `crash_loop`. |
| `stable_reset_ms` | 120 000 (2 min) | Uninterrupted ready uptime after which the rolling counter resets. |

The scheduler uses monotonic `Instant` values for every comparison;
the wall-clock is **never** consulted, so DST jumps, NTP slews, and
operator clock adjustments cannot trigger spurious restarts or hide a
crash-loop.

Disabling the policy (`kind: "disabled"`) is reserved for benches and
the v0.1.0 host CI where the worker is the test subject. Disabled
policy keeps the supervisor in `stopped` after the first exit and
requires explicit `request_stop()` / `set_desired_active()` to make
progress.

## Failure classes

The supervisor classifies every failure so the observability service
and the V01-E08 coordinator can react appropriately:

| Class | Trigger | Affects active deployment? |
| --- | --- | --- |
| `ExitBeforeReady` | Worker exited before reaching ready. | No. The supervisor restarts; an in-flight prepare/warm fails the candidate. |
| `ExitAfterReady` | Worker exited after a ready transition. | No. Restart counters bump but active stays unchanged. |
| `NotReadyTimeout` | Readiness probe never returned `ready` inside `startup_timeout_ms`. | No. Supervisor force-terminates the process and schedules a restart. |
| `HealthFailed` | Worker reported `state == "failed"`. | No. Supervisor force-terminates and restarts. |
| `HealthDegraded` | Worker reported `state == "degraded"`. | No. Phase moves to `degraded`; supervisor keeps polling. |

`ExitBeforeReady` and `ExitAfterReady` are surfaced through
`TickOutcome::Fault` so the V01-E08 coordinator can map crashes during
an in-flight candidate to a typed candidate failure (`InferenceFailed`
or `Timeout` depending on the class).

## Supervision events

Every supervision state transition emits a `SupervisionEvent` defined
in `protocol/schemas/supervision_event.json` and
`tensorplate_protocol::supervision_event`. Event kinds are
union-stable:

- `worker_started`, `worker_ready`, `worker_exit`, `worker_not_ready`
- `restart_scheduled`, `worker_degraded`, `worker_failed`
- `crash_loop_entered`
- `worker_stopping`, `worker_stopped`

The sink is bounded. The default implementation
(`event::RingEventSink`) drops the **oldest** pending event when its
queue fills, bumps a typed drop counter the V01-E10 service exposes,
and never blocks the supervisor's `tick` loop. A missing or absent
consumer is explicitly part of the contract: the supervisor must
continue to launch / stop / restart workers even when the
observability service has not started.

## Coordinator integration

`Coordinator::with_supervisor(Arc<WorkerSupervisor>)` attaches a
supervisor to the V01-E08 coordinator. The hand-off is one-way:

- On every successful **promote** (`Coordinator::deploy` reaching
  `Active`, `Coordinator::rollback` reaching `RolledBack`), the
  coordinator calls
  `WorkerSupervisor::set_desired_active(Some(DesiredWorker { … }))`
  followed by `WorkerSupervisor::recover_after_operator_action()`.
- The supervisor never promotes a candidate. It only owns the lifecycle
  of the worker process serving the agent's already-promoted active
  deployment. Promotion remains the coordinator's sole responsibility.

This decoupling preserves the V01-E08 invariant that durable state is
mutated only through `StateStore::update`, while still letting the
supervisor restart the worker for the existing active deployment
without re-running the full deploy transaction.

## Status projection

`WorkerSupervisor::status()` returns a `SupervisionStatus` projection
suitable for the V01-E11 `tensorplate status` command and the V01-E10
observability service. Fields:

- `serving_state` — stable supervision phase name (above).
- `agent_state` — coarse `ready` / `degraded` / `failed` rollup.
- `desired_active` / `actual_active` — deployment ids.
- `backend` — backend hint published by the active bundle.
- `restart_count` / `crash_loop_threshold` / `crash_loop` — rolling
  policy state.
- `launch_sequence` — monotonically-increasing identifier for the
  current launch attempt.
- `last_failure_code` / `last_failure_message` — bounded diagnostic
  context for the most recent failure.
- `next_restart_delay_ms` — non-zero while
  `serving_state == awaiting_restart`.
- `stable_uptime_ms` — monotonic ms since the most recent
  `worker_ready` transition.

`SupervisionStatus` is exposed as
`tensorplate_agent::supervision::SupervisionStatus` for unit / CLI
consumers; the V01-E11 wire envelope projects these fields onto
`AgentStatus` once that feature lands.

## Testing

Unit tests live alongside each supervision module:

- `supervision::config::tests` — config validation.
- `supervision::process::tests` — mock process lifecycle.
- `supervision::readiness::tests` — mock probe + `wait_until_ready`.
- `supervision::policy::tests` — backoff + crash-loop arithmetic under
  a fake clock.
- `supervision::state::tests` — desired-state reconciliation.
- `supervision::event::tests` — bounded sink drop behavior.
- `supervision::supervisor::tests` — `tick()` state machine.

Integration tests under `agent/tests/`:

- `supervision_failure_injection.rs` — end-to-end matrix from V01-E09-F07.
- `supervision_coordination.rs` — coordinator / supervisor handoff
  through `Coordinator::with_supervisor`.

All supervision tests use `FakeClock` so backoff / crash-loop windows
fire deterministically with no real sleeps.
