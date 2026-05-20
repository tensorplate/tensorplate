# Metrics Registry and Local Export (V01-E12-F04)

The v0.1 metrics registry is local-only by default. Producers register
counters, gauges, and histograms identified by
`(name, kind, unit, labels)`; the registry refuses to expand its label
cardinality beyond the bounded v0.1 keys, and the exporter ships file,
stdout, and in-memory scrape sinks without any platform connection.

Wire format: [`protocol/schemas/metric_event.json`](../../protocol/schemas/metric_event.json).
Rust mirror: [`protocol::metric_event`](../../protocol/rust/src/metric_event.rs).
Implementation:
[`tensorplate_observability::metrics`](../../observability/src/metrics.rs).

## Naming

- All metric names start with `tp_`. The body must match
  `[a-z][a-z0-9_]*`. The registry rejects names outside the policy.
- Units are explicit: `count`, `milliseconds`, `seconds`, `hertz`,
  `percent`, `bytes`, `ratio`. Producers MUST NOT reuse a name with a
  different unit.

## Labels

The bounded v0.1 keys are:

| Key            | Use                                                              |
| -------------- | ---------------------------------------------------------------- |
| `endpoint`     | Serving HTTP path (e.g. `/v1/infer`).                            |
| `model_class`  | Bundle model class (matches `ModelSpec`).                        |
| `model_name`   | Bundle model name.                                               |
| `backend`      | Bounded backend label.                                           |
| `component`    | Producer component (subset of `LogComponent`).                   |
| `status`       | Bounded outcome label (`ok`, `failed`, `timeout`, `cancelled`).  |

Values are bounded to
[`MAX_METRIC_LABEL_BYTES`](../../protocol/rust/src/metric_event.rs).
Unknown keys or oversize values are rejected with
`ObservabilityError::InvalidEvent` and counted in the export status.

## Baseline metrics

The v0.1 baseline reserves these metric names. Producers add more by
appending; existing names are stable for the life of v0.1.

| Name                            | Kind      | Unit         | Labels                                         |
| ------------------------------- | --------- | ------------ | ---------------------------------------------- |
| `tp_infer_requests_total`       | counter   | count        | `endpoint`, `backend`, `status`                |
| `tp_infer_failures_total`       | counter   | count        | `endpoint`, `backend`                          |
| `tp_infer_latency_ms`           | histogram | milliseconds | `endpoint`, `backend`, `model_name`            |
| `tp_scheduler_wait_ms`          | histogram | milliseconds | `endpoint`, `backend`                          |
| `tp_scheduler_queue_depth`      | gauge     | count        | `endpoint`, `backend`                          |
| `tp_scheduler_in_flight`        | gauge     | count        | `endpoint`, `backend`                          |
| `tp_scheduler_missed_deadlines` | counter   | count        | `endpoint`, `backend`                          |
| `tp_scheduler_admission_rejected` | counter | count        | `endpoint`, `backend`                          |
| `tp_scheduler_cancelled`        | counter   | count        | `endpoint`, `backend`                          |
| `tp_load_latency_ms`            | histogram | milliseconds | `backend`, `model_class`, `model_name`         |
| `tp_unload_latency_ms`          | histogram | milliseconds | `backend`, `model_class`, `model_name`         |
| `tp_backend_unsupported`        | counter   | count        | `backend`                                      |
| `tp_memory_pressure_ratio`      | gauge     | ratio        | `component`                                    |
| `tp_sidecar_health`             | gauge     | count        | `component`                                    |
| `tp_worker_health`              | gauge     | count        | `component`                                    |
| `tp_observability_queue_depth`  | gauge     | count        | `component`                                    |
| `tp_observability_state`        | gauge     | count        | `component` (`ready=0`, `degraded=1`, `failed=2`, `no_heartbeat=3`) |
| `tp_listener_malformed`         | gauge     | count        | `component`                                    |
| `tp_export_failures_total`      | counter   | count        | `component`                                    |

The Jetson Orin Nano 8GB default histogram buckets for latency are
defined by
[`default_latency_buckets_ms`](../../observability/src/metrics.rs):
`[1, 5, 10, 25, 50, 100, 250, 500, 1000, 2500]` ms.

Histogram `bucket_counts` are cumulative Prometheus-style counts. The
last bucket is the implicit `+Inf` bucket and must equal `count`.

## Local export sinks

Configured through `observability.json` at `metrics.sink` and represented
in Rust by `MetricSinkConfig`:

| Sink       | Purpose                                                          |
| ---------- | ---------------------------------------------------------------- |
| `noop`     | Drop metrics on the floor; `take_snapshot` is still available.   |
| `in_memory`| Hold the most recent snapshot in memory; tests / CLI scrape it.  |
| `file`     | Append wire-format JSON lines to the configured file.            |
| `stdout`   | Write wire-format JSON lines to standard output.                 |

File and stdout writes are bounded; write failures bump
`sink_write_errors` without crashing the serving path.

## Backpressure

The registry tracks:

- `series_rejected_unknown_label` — label key outside the bounded list.
- `series_rejected_bounded_label` — label value exceeded
  [`MAX_METRIC_LABEL_BYTES`](../../protocol/rust/src/metric_event.rs).
- `series_rejected_full` — series count reached `max_series`.
- `samples_dropped_queue_full` — reserved for the future async export
  queue.
- `sink_write_errors` — file / stdout sink failures.

All counters surface through
[`MetricsExportStatus`](../../observability/src/snapshot.rs) and the
status snapshot.

## Scraping

The in-memory scrape sink exposes a single snapshot through
[`MetricsRegistry::take_snapshot`](../../observability/src/metrics.rs).
The output is a `Vec<MetricEvent>` already in wire format, so the V01
serving worker HTTP envelope can serve it directly when an operator
points a local scraper at the device.
