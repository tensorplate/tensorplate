# Serving worker

This document is the authoritative description of the v0.1.0
`tensorplate-serving` process. It is referenced from
[`CONTRIBUTING.md`](../../CONTRIBUTING.md) and tested by the V01-E07
test tree.

## Process responsibility

`tensorplate-serving` is the data-plane process. It does **not** do
desired-state management, deploy transactions, or worker
supervision; the agent (V01-E08) owns those concerns and starts the
worker with a validated config. The worker:

1. Loads its config, validates it, and wires up runtime components
   in one explicit composition root (V01-E07-F01).
2. Opens a loopback-only HTTP/1.1 listener (V01-E07-F02).
3. Routes `/infer`, `/policy/infer`, `/policy/result/<id>`,
   `/policy/cancel/<id>`, `/health`, and `/metrics` requests
   (V01-E07-F02..F06).
4. Connects each accepted request to the V01-E06 scheduler, the
   V01-E04 execution session, the V01-E03 buffer plane, and the
   V01-E05 adapter through their public interfaces only
   (V01-E07-F05).
5. Publishes serving state, scheduler accounting, buffer accounting,
   and latency histograms locally (V01-E07-F06).
6. Drains and exits deterministically on SIGTERM / SIGINT or on an
   agent-driven shutdown call (V01-E07-F07).

The worker is loopback-only by default. Setting `bind.allow_non_loopback`
in the config is rejected by the validator unless the test-only
environment variable `TP_E2E_ALLOW_NON_LOOPBACK=1` is set. Production
deployments rely on the agent for any remote exposure decisions.

## Composition root

The `ServingWorker::create(config)` factory builds, in this order:

1. `BufferManager` (V01-E03), sized to `config.buffer.capacity_bytes`.
2. `BackendRegistry` populated by `register_builtin_backends` unless
   the caller supplied a registry (tests do).
3. `ExecutionSession`. When `deployment.use_mock_session` is true,
   the in-process `MockServingSession` is constructed; otherwise the
   registry resolves `deployment.model.backend_hint` and the worker
   calls `load` + `prime`.
4. `InferScheduler` through `make_scheduler` (V01-E06), with the
   system steady-clock and a scheduler-event sink that mirrors
   metrics and health.
5. `AsyncPolicyStore`, `ServingPipeline`, `RequestRouter`, and the
   `HttpServer`.

Component construction is deterministic. If any step fails, the
factory returns the typed error from the failing layer and no
listener is opened. The binary's exit codes are:

| Code | Reason |
| ---- | ------ |
| 0    | Normal shutdown after start. |
| 64   | Configuration parse / validation failure. |
| 65   | Component build / session load failure. |
| 66   | Listener bind / accept failure. |
| 70   | Internal error. |

A worker that exits with 64 or 65 ends its stderr with one JSON line, the
startup failure record, written whether or not `enable_stderr_logs` is
set:

```json
{"component":"serving","fields":{"code":"unsupported","message":"..."},"level":"error","message":"worker startup failed","ts_ns":0}
```

`fields.code` is the typed error of the step that failed, in its wire
spelling; when a sidecar runner refuses a load it is the runner's code and
message unchanged. If the record itself cannot be built the worker writes
the plain line `worker startup failed` instead. The agent reads this line to answer the deploy that
started the worker (see [agent.md](agent.md#serving-worker-handoff-v01-e08-f05)).

## Loopback HTTP server

The HTTP server is a small in-tree implementation (see
`runtime/src/http/http_server.cpp`). Selection rationale:

- **Loopback by default.** The server refuses to bind anything
  outside the documented loopback set (`127.0.0.1`, `::1`,
  `localhost`) unless `allow_non_loopback = true` is set in config
  *and* the validator's test-only environment opt-in is present.
  The listener honors the address family the host literal names: an
  IPv4 literal binds an `AF_INET` socket, `::1` binds an `AF_INET6`
  socket (IPv6-only), and `localhost` maps to the IPv4 loopback.
- **Request limits.** `max_body_bytes`, `max_header_bytes`, and
  `request_timeout` are enforced inline by the parser. Oversized
  requests return 413 before any buffer-plane allocation is
  attempted.
- **Graceful shutdown.** The server has a dedicated `stop()` method
  that closes the listening socket and joins the worker pool; the
  composition root drains the scheduler after the server stops.
- **Testability.** The server can bind an ephemeral port and surface
  the assigned port through `bound_port()`. Tests connect without
  racing on a fixed port.
- **Dependency weight.** Nothing beyond POSIX sockets and
  `nlohmann::json`.

The server does *not* implement keep-alive, HTTP/2, TLS, multipart,
SSE, or chunked transfer encoding. Adding any of those is a
deliberate v0.2+ decision.

## Route contract

| Method | Path | Body | Response |
| ------ | ---- | ---- | -------- |
| POST   | `/infer` | `serving_http_envelope.InferRequest` | `InferResponseSuccess` (200) or `InferResponseFailure` (4xx/5xx, or 200 when the backend itself reports the failure). |
| POST   | `/policy/infer` | `serving_http_envelope.InferRequest` | `AsyncAccepted` (202) with `result_url` and `cancel_url`, or 501 when the resolved backend lacks `supports_async`. |
| GET    | `/policy/result/<request_id>` | _empty_ | `AsyncResult` (200) — `status` discriminates `pending`/`in_flight`/`completed`/`cancelled`/`stale`/`failed`/`expired`, or 501 when the resolved backend lacks `supports_async`. |
| POST   | `/policy/cancel/<request_id>` | _empty_ | `AsyncCancelResponse` (200 if cancelled, 404 otherwise), or 501 when the resolved backend lacks `supports_async`. |
| GET    | `/health` | _empty_ | `serving_health` (200 ready/degraded, 503 otherwise). |
| GET    | `/metrics` | _empty_ | Prometheus 0.0.4 text body or `serving_metrics` JSON. |

Every response carries the `x-correlation-id` header. Clients that
do not supply one receive a server-generated `cid-<hex>` value.

Routes that match a known path but a different method return 405;
unknown paths return 404. Loopback binding is enforced server-side.

## Request normalization

`/infer` and `/policy/infer` share the same decoder
(`tensorplate::serving::decode_infer_request`). The decoder validates
the envelope shape *before* any buffer-plane allocation:

1. Required fields (`request_id`, `endpoint`, `inputs`) must be
   present and non-empty.
2. Per-input dtype / shape / layout must be parseable.
3. Base64 payload must decode to at least `byte_offset + byte_size`
   bytes.
4. Metadata strings, if present, must be non-empty.

Validation errors map to typed `Error::Code` values. A typed error that
ends a request before or instead of an inference result is answered with
the status its code maps to below; a few routes answer a specific
condition with a fixed status instead (413 for an oversized payload, 501
when the backend lacks async support, 404 for an unknown async request
id), and a failure the backend itself reports comes back as a 200
`InferResponseFailure` carrying its code:

- `config_invalid` (400)
- `shape_mismatch` (400)
- `unsupported` (415)
- `oom_error` / `resource_exhausted` (429)
- `timeout` (504)
- `not_ready` / `unavailable` (503)
- `cancelled` (499, Client Closed Request)
- `inference_failed` / `load_failed` / `internal` (500)

Only after structural validation passes do the input payloads cross
into `BufferManager` via `build_named_inputs`; the buffer plane owns
the bytes from that point forward.

## LeRobot-compatible async path

The async-policy routes implement the LeRobot PolicyServer-compatible
shape directly. The request envelope is identical to `/infer`; the
response shape is `AsyncAccepted` instead of `InferResponseSuccess`.
Clients poll `result_url` until `status` is one of `completed`,
`cancelled`, `stale`, `failed`, or `expired`.

The route family is enabled only when the resolved backend capability
advertises `supports_async=true`. Sync-only real adapters, including
the v0.1.0 Python/PyTorch sidecar, return 501 before request buffers
are retained or scheduler entries are admitted. This prevents a client
from receiving cancellation acknowledgement while the backend keeps
executing work that it cannot cancel.

Stale-request behavior: when an incoming `/policy/infer` request
carries `metadata.stale_after_sequence = N`, the router tags every
async entry whose `action_chunk_sequence <= N` as `stale` and
dispatches a `cancel(StaleSequence)` to the scheduler. The scheduler
releases the buffers of any queued requests; in-flight requests
suppress their result publishing.

`/policy/cancel/<id>` flips the entry to `cancelled` and dispatches
`cancel(ClientRequest)`.

`/policy/result/<id>` releases the entry's buffers as soon as a
completed result is delivered; subsequent reads return 404.

## Health and metrics

`HealthState` is a thread-safe state container updated by the
composition root, by the scheduler event sink, and by the shutdown
controller. The schema mirrors `protocol/schemas/serving_health.json`.

`ServingMetrics` is a bounded counter / histogram bag with four
labels: `endpoint`, `model_class`, `model_name`, `backend`. The
Prometheus exposition format is the default; JSON mode mirrors
`protocol/schemas/serving_metrics.json`.

`/metrics` refreshes the scheduler gauges from `InferScheduler::metrics()`
on every request: `scheduler_queue_depth`, `scheduler_in_flight` (the
logical in-flight count under its protocol 0.1 name),
`scheduler_in_flight_logical` (the same value),
`scheduler_in_flight_physical` and `scheduler_in_flight_physical_cancelled`;
the Prometheus names add the `tensorplate_serving_` prefix. A request
cancelled while the dispatcher is inside `infer` leaves the logical count at
once and the physical count only after `infer` returns and the pipeline
reports its completion. `/health` reports `in_flight` as the logical count.
[`scheduler.md`](scheduler.md#metrics-and-events) defines the counts.

Latency histograms use the same bucket boundaries everywhere:
`0.5, 1, 2, 5, 10, 25, 50, 100, 250, 1000, 5000, +Inf` ms.

## Graceful shutdown

Shutdown flows through `ShutdownController` (Running → Stopping →
Draining → Stopped):

1. Composition root flips the controller to `Stopping`. The router
   and pipeline refuse new admission with `not_ready` / 503; the
   HTTP server keeps serving in-flight responses.
2. The composition root stops the HTTP listener.
3. If `shutdown.cancel_queued_immediately` is true, `InferScheduler::shutdown`
   cancels every queued request and releases their input buffers.
4. The composition root waits up to `shutdown.drain_deadline` for
   the scheduler's queue depth and logical in-flight count to reach zero.
5. `InferScheduler::shutdown` is called again to clear anything left.
6. `AsyncPolicyStore::cancel_all` releases retained input / completed
   buffers.
7. `ExecutionSession::unload` is called exactly once.
8. The controller transitions to `Stopped` and the dispatcher /
   evictor threads join.

ASAN-clean: the buffer manager reports zero active buffers after
shutdown in the V01-E07-F08 integration tests.

## Logical sessions

A logical session is one client stream pinned to the deployment
generation its worker serves. Every session of a worker shares the
worker's single loaded backend: opening one never loads weights or
starts a process. `include/tensorplate/serving/session.hpp` defines its
lifecycle as `LogicalSessionMachine`, a pure state machine. It holds no
timers, clocks, queues, threads or I/O. Its owner applies one session's
events in order and carries out the effects of each accepted event, in
ascending `LogicalSessionEffect` order, before applying the next. The
serving worker does not create logical sessions yet; this is the
lifecycle the streaming serving modes build on.

`SessionManager` owns those machines, their count reservations and their
input credit. It opens only for the worker's generation, admits at most
the configured count (2,048 by default), and offers an admission check
for a separate memory quota. An optional bounded initialization step
runs after slot reservation and before `emit_ready`; failure emits one
terminal error and holds the slot for physical cleanup. Its sink applies
each machine's effects in order; the manager serializes event
application and sink calls across client and timer threads. A slot
remains held after cancel, failure or drain until `release_acknowledged`
produces `release_slot`. `stop_admission_and_drain` closes admission,
starts each live session's drain, and waits for `drain_completed`
followed by physical release before closure. The manager does not reload
a backend or schedule a physical job.

Each session is opened with `SessionBudgets`
(`runtime/src/serving/session/credits.hpp`), derived from the byte rate
of the audio format negotiated at open. A rate of zero is refused, as is
one whose window would exceed the largest audio payload of a backend
job:

| Budget | Audio in, transcripts out | Text in, audio out |
|---|---|---|
| Input credit | 1 second of accepted, unconsumed audio | 2 waiting segments of 8 KiB combined, plus 1 being executed or delivered |
| Output PCM | none | 2 seconds |
| Output control and transcript metadata | 16 KiB, the last 1 KiB for lifecycle messages only | the same |

**Input credit.** `accept_input` applies `data` for one audio chunk or
text segment and charges its bytes in the same step, so input is charged
only when the state accepts it; a drain ignores input without charging
it. Input above the credit fails the session with
`input_credit_exceeded`. Audio credit returns as the pipeline consumes
it (`release_audio_input`). A text segment's waiting credit returns when
it takes the single active slot (`start_text_segment`), and the slot
stays occupied until the caller finishes the segment
(`finish_text_segment`), which it does only once the segment's output
has been delivered or discarded, not when synthesis ends: at most three
accepted segments exist at once. A release the credit cannot match is a
defect in the caller and fails the session.

**Output queue.** Every server message of a stream passes through its
`BoundedOutputQueue`, which the manager creates at open and hands to the
sink with each transition. A producer of task output offers an item;
when its budget has no room the offer answers `full`, the producer keeps
the item and pauses until output drains, so task output is never dropped
while the session lives. Task output may fill the metadata budget only
up to its last 1 KiB, which is kept for lifecycle messages so that a
backlog of transcripts cannot keep a reply or `CancelAccepted` out. A
lifecycle reply is answered `full` only when the peer has left the whole
metadata budget unread; the sink, which cannot wait, does not queue that
reply. For a live or draining session the no-progress limit below is
already running; a cancelled session ends with its terminal outcome. The
stream assigns a message its sequence only when the transport takes it,
so an unsent partial hypothesis can still be replaced in place by its
newer revision, and a final supersedes the unsent partial of its
utterance; a final is never replaced. A lifecycle reply whose newest
instance says everything can carry a key and is replaced the same way,
which keeps such replies from accumulating. An item stays charged from
the offer until the transport confirms its delivery, so what the
transport holds counts against the same budget. On `suppress_output` the
manager discards the unsent task output before the sink runs and the
queue refuses task output from then on, while lifecycle messages still
pass. The terminal outcome is accepted once, even when the budget is
full, and nothing is accepted after it. The queue outlives the session's
slot, because lifecycle messages and the terminal outcome are still
delivered after release.

**No-progress limit.** The queue keeps a stall clock: it starts when
output first awaits delivery, moves only when a delivery is confirmed,
and stops when nothing awaits delivery. Offering more output, a ping or
a status request does not move it. A live or draining session whose
clock reaches 5 seconds is aborted with `slow_consumer` by the same
timer thread that enforces the limits below.

**Status.** Each transition carries a `LogicalSessionStatus`: the state,
the accepted input items still owned, and the use of the input credit
and of both output budgets. Output use counts every undelivered byte,
queued or held by the transport. `SessionManager::status` returns the
same for a live session.

**Tombstones.** When a slot is released the manager keeps the session's
key, generation, final state and end cause (its code, and at most 64
bytes of its reason) for 60 seconds, at most 1,024 per worker with the
oldest dropped first. A call naming such a session is refused with
`session_ended`, so a report that raced the release can be told from an
`unknown_session`.

**Finalize deadline.** A session may owe a finalization or a drain only
for a bounded time, set by what it takes as input: 10 seconds for audio
input, where a transcript is finished, and 120 seconds for text input,
where accepted segments are synthesized and delivered (two waiting and
one running, at most 30 seconds of audio each, at the pace of playback).
The deadline starts when an accepted event first leaves the session in
`finalizing` or `draining`, and stops when the session is `active` again
or `drain_completed` is applied. Nothing in between moves it: not a
delivery, a further automatic endpoint, a client Finalize taking over,
or a half-close during a finalization. A session that reaches it is
aborted with `finalize_timeout`, through the cancel path like the other
timeouts. The no-progress limit above is separate and ends a stalled
reader sooner. Once a drain has completed, the session waits for the
backend to acknowledge release without this deadline; bounding that wait
belongs to backend cleanup.

**Generation of later messages.** `open` refuses a generation other than
the worker's. For a later client message or a backend report,
`check_generation` compares the generation it names with the session's:
a different one, zero for an absent field included, fails the session
with `stale_generation` and the caller discards the message. After the
outcome is fixed the check is still refused but changes nothing.

`test/unit/fixtures/session_lifecycle.json` holds the deadline rows and
step-by-step lifecycle cases (deadlines, generation, segment completion,
the ways a session closes, expiry) that the unit tests replay against
the manager; a transport binding can replay the same cases.

`SessionLimits` defaults to 60 seconds idle, a 10-second client heartbeat
cadence, 30 seconds liveness, and a 60-minute absolute duration. Data
and client control refresh idle activity; only `ping` refreshes heartbeat
liveness. Draining suspends idle and heartbeat expiry so accepted work
can finish after the client write-half-closes; the absolute duration
and the finalize deadline still apply. A dedicated timer thread finds due sessions using the
injected monotonic `SchedulerClock`, independent of the serving
worker's existing request evictor. Unit tests advance
`FakeSchedulerClock` without sleeping. The current HTTP composition
root does not instantiate `SessionManager`; the streaming transport
binding will supply its effect sink, choose each session's budgets and
drive its output queue.

States: `opening`, `active`, `finalizing`, `draining`,
`cancel_requested`, and the terminal `closed` and `failed`. Events come
from the client (`open`, `data`, `finalize`, `cancel`, `half_close`,
`ping`, `status_request`) and from the owner: admission (`admitted`),
the pipeline and backend (`automatic_endpoint`, `finalize_completed`,
`drain_completed`, `backend_reset`, `release_acknowledged`), timers,
pressure and worker control (`drain`, `abort`, `fail`). Effects, in
execution order: `suppress_output`, `emit_cancel_accepted`,
`request_cleanup`, `emit_ready`, `accept_input`, `emit_reply`,
`start_finalize`, `start_drain`, `release_slot`, `emit_terminal`.

The machine distinguishes ten configurations: the public states, with
`finalizing` split by whether a client Finalize or an automatic endpoint
started it (only the latter still accepts input), `draining` split by
whether release was requested, and `failed` split by whether release was
acknowledged. The table below is the machine's complete transition
function; the unit tests hold it to the same table in
`test/mocks/session_transition_fixtures.hpp`.

Configurations: O `opening`, A `active`, FF finalizing after a client
Finalize, FE finalizing after an automatic endpoint, DW draining with
work in progress, DR draining with release requested, CR
`cancel_requested`, CL `closed`, FH failed holding its reservation, FR
failed and released. Effects: Rdy `emit_ready`, In `accept_input`, Rep
`emit_reply`, Fin `start_finalize`, Drn `start_drain`, Cln
`request_cleanup`, Sup `suppress_output`, Ack `emit_cancel_accepted`,
Rel `release_slot`, Term `emit_terminal`. `=` stays in the same
configuration, followed by its effects if any; `X` refuses.

| | open | data | finalize | cancel | half_close | ping | status_request | admitted | automatic_endpoint | finalize_completed | drain_completed | drain | abort | fail | backend_reset | release_acknowledged |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| O | X | X | X | CR Sup Cln | X | X | X | A Rdy | X | X | X | CR Sup Cln | CR Sup Cln | FH Sup Cln Term | FH Sup Cln Term | X |
| A | X | = In | FF Fin | CR Sup Ack Cln | DW Drn | = Rep | = Rep | X | FE Fin | X | X | DW Drn | CR Sup Cln | FH Sup Cln Term | FH Sup Cln Term | X |
| FF | X | X | = | CR Sup Ack Cln | DW Drn | = Rep | = Rep | X | = Fin | A | X | DW Drn | CR Sup Cln | FH Sup Cln Term | FH Sup Cln Term | X |
| FE | X | = In | FF Fin | CR Sup Ack Cln | DW Drn | = Rep | = Rep | X | = Fin | A | X | DW Drn | CR Sup Cln | FH Sup Cln Term | FH Sup Cln Term | X |
| DW | X | = | = | CR Sup Ack Cln | = | = Rep | = Rep | X | = Fin | = | DR Cln | = | CR Sup Cln | FH Sup Cln Term | FH Sup Cln Term | X |
| DR | X | = | = | CR Sup Ack | = | = Rep | = Rep | X | X | X | X | = | CR Sup | FH Sup Term | FH Sup Term | CL Rel Term |
| CR | X | X | X | = | = | X | X | = | = | = | = | = | = | = | FH Term | CL Rel Term |
| CL | X | X | X | = | = | X | X | = | = | = | = | = | = | = | = | = |
| FH | X | X | X | = | = | X | X | = | = | = | = | = | = | = | = | FR Rel |
| FR | X | X | X | = | = | X | X | = | = | = | = | = | = | = | = | = |

Rules the table encodes:

- **Refusals.** A refused event changes nothing. A client message the
  state does not permit, including a client request after the outcome is
  fixed, is a protocol violation: `not_ready` with context
  `illegal_transition`. An owner report that its producer cannot emit in
  that state is a defect in the owner: `internal` with context
  `unexpected_report`. The owner answers a refusal by applying `fail`
  with the refusal as its cause; `fail` is ignored once cancellation or a
  terminal outcome has fixed the result, so doing so is always safe.
- **Accepted without effect.** Events that can legitimately arrive late
  or race a state change, for example: a repeated cancel, half-close,
  drain, abort or fail, or a Finalize repeated while finalizing; input or
  a Finalize racing a drain; late pipeline reports after cancellation or
  failure; a duplicate release acknowledgement.
- **Finalization.** The owner applies `finalize_completed` before it
  publishes the message that completes the client's Finalize, so the
  client's next input finds the session active.
- **Exactly one terminal outcome.** `emit_terminal` is produced once per
  session. `closed` writes it after physical release; `failed` writes it
  on entry and keeps its reservation until release is acknowledged.
  Nothing leaves `closed` or `failed`, and no task output follows
  `suppress_output`.
- **Cancellation is not cleanup.** `emit_cancel_accepted` answers a
  client Cancel after Ready, while the outcome is still open, at once;
  the reservation is returned (`release_slot`) only after the backend
  acknowledges physical release. A Cancel before Ready ends the session
  without an acknowledgement, and a Cancel after a worker abort or a
  failure is answered by the terminal outcome alone. A backend reset
  while cancellation waits for release fails the session.
- **Generation binding.** `LogicalSessionMachine::open` binds a session
  to the worker's generation and refuses a mismatched or absent
  generation (`not_ready`, `stale_generation`); a worker without a
  generation is `config_invalid` (`invalid_generation`). The owner checks
  every later message or backend report with `check_generation`,
  discards a stale one and applies `fail`.

How sessions end, and the code their terminal outcome carries:

| Situation | Owner applies | Ends | Code (reason) |
|---|---|---|---|
| Client Cancel | `cancel` | `closed` | `cancelled` |
| Client half-close | `half_close`, then `drain_completed` and `release_acknowledged` | `closed` | none |
| Input above the session's credit | `accept_input` (the manager applies `fail`) | `failed` | `resource_exhausted` (`input_credit_exceeded`) |
| Input item without bytes | `accept_input` (the manager applies `fail`) | `failed` | `config_invalid` (`empty_input`) |
| Credit release that does not match what is held (a defect) | a release call (the manager applies `fail`) | `failed` | `internal` (`input_release_mismatch`, `segment_stage_violation` or `wrong_input_kind`) |
| Output undelivered past the no-progress limit | the manager's timer applies `abort` | `closed` | `resource_exhausted` (`slow_consumer`) |
| Backend process reset or reaped | `backend_reset` | `failed` | `unavailable` (`backend_reset`) |
| Deployment generation retiring | `drain`, then `abort` at the deadline | `closed` | `unavailable` (`deployment_retired`) |
| Worker shutting down | `drain`, then `abort` at the deadline | `closed` | `unavailable` (`worker_shutdown`) |
| Stale generation in a later message | `fail` | `failed` | `not_ready` (`stale_generation`) |
| Protocol violation | `fail` | `failed` | `not_ready` (`illegal_transition`) |
| Owner report the state does not permit (a defect) | `fail` | `failed` | `internal` (`unexpected_report`) |

The reason strings are the failure reasons in
[`failure-reasons.md`](../observability/failure-reasons.md) where one
exists. The owner keeps the end cause: the error carried by the most
recent accepted event that moved the session into `draining`,
`cancel_requested` or `failed`, which can only move from a drain to a
cancel to a failure.

## Test surface

- Host-CI mock path: `deployment.use_mock_session = true`. No real
  backend is involved. Used by the F08 integration suite and the
  benchmarks.
- Real-adapter smoke tests are gated by the V01-E05 adapter feature
  flags (`TP_ENABLE_TENSORRT`, `TP_ENABLE_LIBTORCH`,
  `TP_ENABLE_PYTHON_PYTORCH_SIDECAR`).

See V01-E07-F08 fixtures for canonical end-to-end coverage.
