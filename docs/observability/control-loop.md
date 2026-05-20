# Control-Loop Jitter and Frequency Stability (V01-E12-F05)

The control-loop aggregator emits the metrics that V01-E15 SmolVLA
validation needs: missed-deadline rate, jitter p50/p95/p99/max, mean
frequency, instant-frequency standard deviation, and frequency error
percent. Every formula uses monotonic time and a rolling 60s window.

Wire format: [`protocol/schemas/control_loop_metrics.json`](../../protocol/schemas/control_loop_metrics.json).
Rust mirror: [`protocol::control_loop_metrics`](../../protocol/rust/src/control_loop_metrics.rs).
Implementation:
[`tensorplate_observability::control_loop`](../../observability/src/control_loop.rs).

## Configuration

Each control loop carries a fixed target frequency `control_frequency_hz`
and a bounded label set:

```
endpoint     = serving HTTP path (e.g. `/v1/act`)
model_class  = bundle model class (`vla`)
model_name   = bundle model name (e.g. `smolvla-tiny`)
backend      = bounded backend label (e.g. `tensorrt`)
```

A missing or non-positive `control_frequency_hz` disables the
aggregator with a typed config warning; the rest of the observability
pipeline keeps running.

## Formulas

```
target_period_ms        = 1000 / control_frequency_hz
interval_ms_i           = t_i - t_{i-1}                       (monotonic)
jitter_ms_i             = abs(interval_ms_i - target_period_ms)
missed_deadline_i       = interval_ms_i > target_period_ms + grace_ms
instant_frequency_hz_i  = 1000 / interval_ms_i
mean_interval_ms        = mean(interval_ms in rolling 60s window)
mean_frequency_hz       = 1000 / mean_interval_ms
frequency_stddev_hz     = stddev(instant_frequency_hz in window)
frequency_error_pct     = abs(mean_frequency_hz - control_frequency_hz)
                          / control_frequency_hz * 100
missed_deadline_rate    = missed_deadlines / samples            (in window)
```

The default grace window is 25% of `target_period_ms`; producers can
override it via
[`ControlLoopAggregatorConfig::grace_ms`](../../observability/src/control_loop.rs).
Mean frequency is computed from the mean interval rather than
`output_count / window_seconds` so the value is meaningful even when
the window is sparsely populated during early validation runs.

## Sample handling

- Each `record_output(at)` call expects a monotonic
  [`Instant`](https://doc.rust-lang.org/std/time/struct.Instant.html).
- Zero or negative intervals (clock skew, duplicate outputs) are
  rejected and counted in
  [`invalid_intervals`](../../observability/src/control_loop.rs) without
  disturbing the rolling window.
- The window evicts samples older than the configured duration on every
  call. The aggregator keeps at most
  [`MAX_CONTROL_LOOP_SAMPLES`](../../observability/src/control_loop.rs)
  to bound memory under bursty producers.

## Export

The aggregator emits two surfaces:

1. A [`ControlLoopEvent`](../../protocol/rust/src/control_loop_metrics.rs)
   carrying the full window summary + bounded labels.
2. A
   [`ControlLoopStatus`](../../observability/src/snapshot.rs)
   projection embedded in the status snapshot so `tensorplate status`
   and V01-E15 validation can read the rolling summary without
   subscribing to the event stream.

## V01-E15 expectations

The SmolVLA validation harness will consume:

- `mean_frequency_hz` to verify the device hits its target rate.
- `frequency_error_pct` and `frequency_stddev_hz` to verify the rate is
  stable across the validation run.
- `jitter_p99_ms` and `jitter_max_ms` to verify the worst-case
  inter-output latency stays inside the v0.1 budget.
- `missed_deadline_rate` to confirm the scheduler keeps up.
