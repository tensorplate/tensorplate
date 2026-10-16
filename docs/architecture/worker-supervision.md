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

The supervisor runs only where a configuration enables `supervision`, and
no packaged configuration does. The workers of a `worker.mode: process`
agent are restarted and stopped by its member registry instead; see
[Supervision by the member registry](#supervision-by-the-member-registry).

## Device pinning

`DesiredWorker.device_index` optionally selects one accelerator for a
worker. `WorkerProcess::launch` receives the pin as a launch parameter.
The process launcher sets `CUDA_VISIBLE_DEVICES` after rebuilding the
environment from the allowlist, so an explicit pin overrides an inherited
value. With `None`, the allowlist continues to govern the environment.

The running worker's handle retains its launch-time pin. Adding, changing,
or removing that pin through `set_desired_active` stops the old worker
before launching a replacement, even for the same deployment ID. An
unchanged request preserves the running worker. If desired placement
changes again while the worker is stopping, the replacement uses the
latest request. Crash restarts also use the current desired pin.

The coordinator and restart recovery currently request `None`: device
allocation and multi-device execution remain deferred.

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
- `desired_device_index` / `actual_device_index` — requested and
  launch-time accelerator pins. With the corresponding deployment ID
  present, an absent pin means unpinned. Without that ID, no worker is
  requested or running. The actual pin remains tied to the launched
  worker through a placement change and clears when the worker leaves
  actual state; the desired pin remains visible during launch or restart.
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
`tensorplate_agent::supervision::SupervisionStatus` internally and is
projected onto `AgentStatus.supervision` in
`protocol/schemas/agent_control.json` so CLI, health, and status callers
can report crash-loop state without reaching into the supervisor directly.

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

## Supervision by the member registry

In `worker.mode: process` the member registry supervises the workers it
starts: the agent's loop runs its supervision step every 100 ms and logs
what it did. `docs/architecture/agent.md` ("Member registry") is the full
account. The restart policy above does not apply to it; the registry
reuses this module's clock trait and nothing else.

A serving worker is taken out of service when its process exits, when its
control channel has failed (ten seconds without an answered ledger poll,
see below), or, if a restart started it, when it is not ready within
`worker.warm_timeout_ms`. Nothing else stops one: a slow `/health` answer
or slow inference does not, and `/health` is not polled once the worker is
ready.

Its successor is a new generation on the same port, started 1, 2 and 4
seconds after the stop for the first, second and third restart inside a
sliding five-minute window. A member that stops with three restarts inside
the last five minutes is failed: nothing is started for it until a deploy
or a rollback is promoted, and status reports the failure as `last_error`
with `agent_state: degraded`.

The failed state is not written to the state file. An agent restart starts
the recorded deployment again with all three restarts available, and
status reports the member as failed until then.

A start whose restore of the recorded deployment does not come up does not
end the agent: the registry starts it again as a serving member that
stopped, with the same delays and the same limit.

Before a restart the registry compares the digest of the staged manifest
with the bundle digest the member was started under; when they differ the
member is failed at once and nothing is started. A staged manifest that
cannot be read counts as a restart that could not be launched.

No restart starts while a deploy holds the member, from the moment its
`prepare` begins until its candidate is promoted or fails, its `warm`
gives up, or its transaction returns without promoting it. A transaction
that hands the registry a generation (a deploy of a format 0.2 bundle, and
a rollback or a restore of a record that names a deployment descriptor)
holds the member earlier, from before it takes that generation
(`WorkerControl::hold`). A promoted generation is therefore never older
than the one it replaces; [agent.md](agent.md#member-registry) gives the
order that makes it so.

On SIGTERM or SIGINT the agent asks every worker, serving and candidate, to
drain, kills what is still running at the one 10-second stop deadline they
share, stops its listener and exits 0. No worker is started once the stop
has begun, and a deploy in flight is left in flight for the next start's
recovery; its caller is answered status `error` with the code
`unavailable`. A second signal during the stop does nothing.

## Runtime control client

`agent::control_channel::ControlChannel` takes an owned Unix socket and a
`MemberRef`. A dedicated standard-library thread polls `LedgerStatus` once per
second. It shares neither the main-loop transaction path nor the bundle verifier
or memory collector. `RuntimeCommand` exposes only the six runtime operations;
the client assigns fresh correlation IDs and validates each request before
queueing it. Frames are bounded at 65,536 bytes including the newline. Replies
must validate against protocol 0.1 and match operation, transaction, correlation
and member generation. Malformed frames, wrong members, EOF and partial writes
close the transport; contact detection continues after closure.

There are at most 16 queued commands and one active exchange. A due ledger poll
takes precedence: queued commands without a remaining execution window receive
backpressure or expiry. An issued command's deadline is also bounded by the next
poll. An expired read preserves partial framing; a later reply to that older
correlation is drained without refreshing contact.

`ControlChannel::submit` sends a command once, with a one-second budget that
includes queue time; it never sends it again, and its caller must treat a
timeout as an unknown outcome. `ControlChannel::caller()` returns a
`ControlCaller`, which other threads use to send a command with a budget of
their own (`RuntimeCall::call`; the member registry gives a fence or an
activate 3 seconds). An attempt not answered when its gap before the next poll
ends is sent again after that poll, as a new request with a new correlation
ID: the schema lets a repeat keep its ID only while no other request was sent
since, and the poll was. A command still queued when its budget passes is never
sent. Sending a fence or an activate again is safe because the worker's answer
to each depends only on its admission state. The call returns the first paired
response whatever its status, and a transport failure at once.

`ContactMonitor` emits typed `OutOfContact` after three missed polls and
`MemberFailed` at ten seconds without a valid paired ledger response. A worker
error answer proves contact but supplies no usable ledger; it sets `last_error`
to `Rejected`. A reply before failure resets the miss count and can emit `ContactRestored`;
failure is terminal for that channel. `ChannelSnapshot` is a bounded watch value
with the current contact state, latest ledger and its receipt time, last error,
and the latest transition plus its revision. Slow consumers may skip intermediate
transitions; they must act on the current state and must not use a retained
ledger as proof of current contact or ignore `last_error` when using it. A newly
created channel has no ledger yet and does not establish readiness. The client performs no kill, reap, restart or
durable state write. Those actions belong to the member registry.

`worker::spawn_with_control` consumes a `Command`, creates a close-on-exec
`UnixStream::pair`, and moves the child end through `Command::stdin`. The standard
library handles descriptor duplication to fd 0 without an unsafe pre-exec hook.
Dropping the command immediately after spawn closes the parent's child-end copy;
the returned agent end remains open until the client is dropped. Dropping the
client signals and joins its thread, then closes the socket. The serving worker
relocates fd 0 to a private close-on-exec descriptor before it starts a thread
or a sidecar, but only when its config names `deployment.generation`: a
launcher that passes the socket renders the generation in the same config, and
a config with a generation and no socket on fd 0 is a worker that refuses to
start. Linux tests check that exactly the agent end remains in the
parent and worker exit produces EOF.

The agent's member registry (`docs/architecture/agent.md`, "Member registry")
starts every `process`-mode worker through this helper and client and renders
the generation in the same config. `WorkerSupervisor` does neither: a worker
it starts has no control channel. A configuration that enables `supervision`
beside `worker.mode: process` is refused at start, since each would start
its own worker. Both send SIGTERM the same way, directly to the worker's
process id.
The serving worker's side is described in
`docs/architecture/serving-worker.md` ("Control channel"); descriptor isolation
between sibling workers remains an integration check. The synthetic peer tests exercise the published golden
frames and a blocked backend; `agent/tests/deploy_load_failure.rs` carries an
opt-in test of the registry against a built `tensorplate-serving`, and
tests that restart a killed stand-in worker and stop the agent with SIGTERM.
