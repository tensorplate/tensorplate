# Local Status Projection (V01-E12-F07)

The V01-E10 status snapshot is the operator's primary read-only view of
the device. V01-E12 extends the snapshot with the new diagnostic
surfaces so a single read answers every operator question that does not
require a hosted dashboard.

Wire format: [`protocol/schemas/observability_status.json`](../../protocol/schemas/observability_status.json).
Rust mirror:
[`tensorplate_observability::snapshot`](../../observability/src/snapshot.rs).

## Fields added in V01-E12

| Field                  | Source                                              |
| ---------------------- | --------------------------------------------------- |
| `diagnostics_sink`     | `tensorplate_observability::retention::RetentionCounters` |
| `metrics_export`       | `tensorplate_observability::metrics::MetricsCounters`     |
| `control_loop`         | `tensorplate_observability::control_loop::ControlLoopAggregator` |
| `last_correlation_id`  | Producer (agent or serving worker) most-recent failure path |
| `last_failure_reason`  | `protocol::failure_reason::FailureReason`                  |

The new fields are optional and skip-serialise when empty so existing
V01-E10 CLI fixtures continue to round-trip.

## Producers

- Update the diagnostics-sink and metrics counters through
  [`SnapshotWriter::update_v12`](../../observability/src/snapshot.rs).
  The composition root calls this on every tick after the V01-E10
  `update` so consumers always see consistent counters.
- Update the control-loop projection from the aggregator
  ([`ControlLoopStatus::from_summary`](../../observability/src/snapshot.rs)).
  Producers omit the field when no control-loop event source is
  attached.
- Record the most recent operator-actionable failure through
  [`SnapshotWriter::update_last_failure`](../../observability/src/snapshot.rs).

## Consumers

- `tensorplate status` reads the snapshot through the V01-E11
  observability snapshot flag and renders:
    - the V01-E10 ready / degraded / failed / no_heartbeat status,
    - the V01-E12 diagnostics-sink and metrics-export counters,
    - the rolling control-loop summary,
    - the most recent failure reason + correlation id.
- `tensorplate doctor` consumes the same projection and surfaces:
    - `diagnostics_sink_full` when `dropped_queue_full > 0`,
    - `metric_export_failures` when `sink_write_errors > 0`,
    - `control_loop_unstable` when `frequency_error_pct` exceeds the
      validation threshold.
- `tensorplate logs` does not consume the projection; it reads the
  retention store directly through the configured file source.

## Schema-version policy

The snapshot is pinned to `schema_version="0.1"`. New fields are
skip-serialised when empty, so older CLI builds continue to parse the
new payload as long as they ignore unknown fields. Readers that need
strict validation use
[`tensorplate_protocol::decode_with_version_check`](../../protocol/rust/src/lib.rs).
