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
| `row`          | Speech model row. Registered values only.                        |
| `mode`         | Stream serving mode. Registered values only.                     |
| `outcome`      | How a session or request ended. Registered values only.          |
| `stage`        | Serving stage a duration covers. Registered values only.         |

Values are bounded to
[`MAX_METRIC_LABEL_BYTES`](../../protocol/rust/src/metric_event.rs).
Unknown keys or oversize values are rejected with
`ObservabilityError::InvalidEvent` and counted in the export status.

The last four keys take only the values registered in
[`metric_event.json`](../../protocol/schemas/metric_event.json) and
mirrored by `registered_metric_label_values`; any other value is rejected
the same way.

| Key       | Registered values                                                                 | Where the names come from                                                     |
| --------- | --------------------------------------------------------------------------------- | ----------------------------------------------------------------------------- |
| `row`     | `speech-stt-whisper-turbo-stream-l4`, `speech-tts-kokoro-stream-l4`               | Defined here: the two speech model rows the release is validated against. Not a platform support row id. |
| `mode`    | `stt_streaming`, `tts_streaming`                                                  | The serving mode a bundle's speech block resolves to, and the stream schema's session modes. |
| `outcome` | `succeeded`, `failed`, `cancelled`, `rejected`                                    | The words the serving worker's request counters use. For this label `rejected` is a refusal before admission, and an expiry or a timeout is `failed`. |
| `stage`   | `ingress`, `queue`, `vad`, `preprocessing`, `backend`, `postprocessing`, `egress` | The runtime pipeline stages of a bundle profile, with the scheduler queue after `ingress`. |

A deployment that is not one of the registered rows carries no `row`
label. `backend` covers the whole backend job: a backend that cannot time
its encode and decode separately reports the one stage and nothing finer.
A session, turn, request, utterance or voice identifier is never a label
key or a label value.

`outcome` does not replace `status`: the baseline metrics below keep
`status` and its four values, and `outcome` labels the speech series. The
rule for an expiry is the label's own; the worker's request counters are
unchanged and still count an expiry and a deadline rejection where they
did. The `queue` and `backend` stages name what the worker's `queue_wait`
and `execution` histograms already measure for unary requests; those two
histograms keep their names.

A reader built before a label key was added rejects a metric event that
carries it: the Rust mirror and this registry refuse a key they do not
know. They hold a registered key to the values listed when they were
built, so they also reject a value appended to a list later. No component
sets the four speech keys yet. The change that first emits one of them,
or an appended value, must not send it to a reader that may predate it.

## Delivery class

A metric event may carry the optional `priority` field described in
[log-schema.md](log-schema.md#delivery-class), with the same four values
and the same rule for a producer that sheds by class.

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

The serving worker's own `/metrics` histograms use a different, fixed
layout, in which every streaming speech latency gate value is a boundary;
see [serving-worker.md](../architecture/serving-worker.md#health-and-metrics).

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
  [`MAX_METRIC_LABEL_BYTES`](../../protocol/rust/src/metric_event.rs),
  or is not a registered value of a key that takes registered values.
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
