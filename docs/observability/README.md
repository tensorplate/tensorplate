# Observability Baseline (V01-E12)

The v0.1 observability baseline makes a single TensorPlate device
diagnosable **without** a hosted platform connection. The agent, serving
worker, runtime/adapters, Python/PyTorch sidecar, observability service,
and CLI all share one bounded telemetry surface:

| Concern                | v0.1 surface                                                                                          |
| ---------------------- | ----------------------------------------------------------------------------------------------------- |
| Structured logs        | [`log_event.json`](../../protocol/schemas/log_event.json) (see [log-schema.md](log-schema.md))        |
| Correlation IDs        | [`correlation-ids.md`](correlation-ids.md)                                                            |
| Failure reasons        | [`failure_reason.json`](../../protocol/schemas/failure_reason.json) + [`failure-reasons.md`](failure-reasons.md) |
| Metrics registry       | [`metric_event.json`](../../protocol/schemas/metric_event.json) + [`metrics.md`](metrics.md)          |
| Control-loop telemetry | [`control_loop_metrics.json`](../../protocol/schemas/control_loop_metrics.json) + [`control-loop.md`](control-loop.md) |
| Retention + sinks      | [`retention.md`](retention.md)                                                                        |
| Status projection      | [`observability_status.json`](../../protocol/schemas/observability_status.json) extended in V01-E12   |

All schemas are pinned to `schema_version="0.1"`; readers reject unknown
versions through
[`tensorplate_protocol::decode_with_version_check`](../../protocol/rust/src/lib.rs).

## Safety invariants

The v0.1 baseline preserves these constraints across every component:

- **Monotonic timing only.** Every log event, metric sample, and
  control-loop interval is timestamped from
  `std::chrono::steady_clock` (C++) or `std::time::Instant` (Rust).
  Wall-clock annotations are advisory; never used for ordering or
  freshness.
- **Bounded data.** Structured-log context entries are capped at
  [`MAX_LOG_CONTEXT_ENTRIES`](../../protocol/rust/src/log_event.rs);
  string values are truncated to
  [`MAX_LOG_CONTEXT_STRING_BYTES`](../../protocol/rust/src/log_event.rs).
  Metric labels are restricted to the v0.1 allowed-key list
  ([`ALLOWED_METRIC_LABEL_KEYS`](../../protocol/rust/src/metric_event.rs)).
- **Non-blocking sinks.** Producers never wait on a slow file, scrape,
  or stdout sink. The retention queue
  ([`DiagnosticsRetention`](../../observability/src/retention.rs)) drops
  the oldest event under pressure and bumps a typed counter.
- **No payload leakage.** Tensor payloads, request payloads, image
  bytes, environment dumps, and secrets are never permitted in logs or
  diagnostics. The bounded-context sanitiser rejects control bytes and
  NUL.
- **Local export only.** The metrics registry ships a file sink, a
  stdout sink, and an in-memory scrape buffer; no hosted relay is
  required.
- **Stable status names.** `ready / degraded / failed / no_heartbeat`
  are shared across V01-E08 (agent), V01-E09 (supervision), V01-E10
  (observability), V01-E11 (CLI), and V01-E12 (status projection).

## Component participation

| Producer                    | Logs | Metrics | Failure reasons | Control loop |
| --------------------------- | ---- | ------- | --------------- | ------------ |
| Agent (deploy, supervision) | yes  | yes     | yes             | no           |
| Serving worker              | yes  | yes     | yes             | yes (VLA)    |
| Runtime / adapter           | yes  | yes     | yes             | no           |
| Python/PyTorch sidecar      | yes  | no      | yes             | no           |
| Observability service      | yes  | yes     | yes             | aggregator   |
| CLI                         | yes  | no      | yes             | display only |

The C++ producers (runtime, serving worker, sidecar adapter) consume
the same JSON schemas via the cross-process IPC layer; the v0.1
implementation lives in `tensorplate-observability` (Rust) and the C++
emitters serialise into the same schemas before sending events across
the IPC boundary. The `runtime/src/telemetry/` C++ tree is reserved for
the C++ producer hooks that bind to these wire formats; they will be
filled in alongside the V01-E13 model-bundle baseline.
