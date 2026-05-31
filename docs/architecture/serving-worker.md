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

## Loopback HTTP server

The HTTP server is a small in-tree implementation (see
`runtime/src/http/http_server.cpp`). Selection rationale:

- **Loopback by default.** The server refuses to bind anything
  outside the documented loopback set (`127.0.0.1`, `::1`,
  `localhost`) unless `allow_non_loopback = true` is set in config
  *and* the validator's test-only environment opt-in is present.
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
| POST   | `/infer` | `serving_http_envelope.InferRequest` | `InferResponseSuccess` (200) or `InferResponseFailure` (4xx/5xx). |
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

Validation errors map to typed `Error::Code` values:

- `config_invalid` (400)
- `shape_mismatch` (400)
- `unsupported` (415)
- `oom_error` (429)
- `timeout` (504)
- `not_ready` (503)
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
   in-flight scheduler accounting to reach zero.
5. `InferScheduler::shutdown` is called again to clear anything left.
6. `AsyncPolicyStore::cancel_all` releases retained input / completed
   buffers.
7. `ExecutionSession::unload` is called exactly once.
8. The controller transitions to `Stopped` and the dispatcher /
   evictor threads join.

ASAN-clean: the buffer manager reports zero active buffers after
shutdown in the V01-E07-F08 integration tests.

## Test surface

- Host-CI mock path: `deployment.use_mock_session = true`. No real
  backend is involved. Used by the F08 integration suite and the
  benchmarks.
- Real-adapter smoke tests are gated by the V01-E05 adapter feature
  flags (`TP_ENABLE_TENSORRT`, `TP_ENABLE_LIBTORCH`,
  `TP_ENABLE_PYTHON_PYTORCH_SIDECAR`).

See V01-E07-F08 fixtures for canonical end-to-end coverage.
