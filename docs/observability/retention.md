# Bounded Diagnostics Retention and Sinks (V01-E12-F06)

Diagnostic retention keeps a bounded local copy of structured log
events for the V01-E11 `tensorplate logs` reader and the release validation
validation harness. The retention store never blocks the serving path;
producers drop into a bounded queue and the sink absorbs the loss.

Implementation:
[`tensorplate_observability::retention`](../../observability/src/retention.rs).

## Config

Configured through `observability.json` at `diagnostics_retention`.

| Field            | Default     | Purpose                                                              |
| ---------------- | ----------- | -------------------------------------------------------------------- |
| `queue_capacity` | `1024`      | Maximum buffered events; capped at `MAX_QUEUE_CAPACITY` (`8192`).    |
| `drop_policy`    | `drop_oldest` | Behaviour when the queue is full. The alternative is `drop_incoming`. |
| `file_path`      | none        | Optional JSON-lines file sink. When unset, the store is in-memory.   |
| `rotate_bytes`   | `1 MiB`     | File-size threshold; the file is renamed to `<file>.1` before further writes. |

The defaults are tuned for a Jetson Orin Nano 8GB Super: the queue uses
< 1 MiB at peak; the rotation policy keeps disk usage bounded across a
24h validation run.

## Drop policy

`drop_oldest` is the v0.1 default and mirrors the safe-state sink: the
producer always wins, the oldest pending event is evicted. The
`drop_incoming` mode preserves the existing tail; choose it when an
operator wants the early-failure record over the latest spam.

## Rotation

When a flush would push the file size past `rotate_bytes`, the
retention store renames the current file to `<file>.1` (overwriting any
previous rotation) before opening a fresh file. The rotation counter
surfaces through the status snapshot.

## Redaction

The bounded-context sanitiser in
[`LogEvent::insert_context`](../../protocol/rust/src/log_event.rs)
already drops tensor payloads, NUL bytes, and control characters before
events reach the retention store. The store still defends against
direct callers by validating events through
[`ValidatePayload`](../../protocol/rust/src/lib.rs); a record that fails
validation is dropped and counted via `dropped_redacted`.

## Disk-full behaviour

A file-write failure bumps `file_write_errors` and returns
`ObservabilityError::SnapshotSink` to the caller; the serving worker
never blocks waiting for a write to succeed. The status snapshot
surfaces the counter so `tensorplate doctor` reports the issue.

## Counters

`RetentionCounters` is surfaced through
[`DiagnosticsSinkStatus`](../../observability/src/snapshot.rs):

| Counter              | Meaning                                                                |
| -------------------- | ---------------------------------------------------------------------- |
| `enqueued`           | Events accepted by the producer-side queue.                            |
| `dropped_queue_full` | Events evicted under the bounded-queue policy.                         |
| `dropped_redacted`   | Events dropped at the sink because they failed validation.             |
| `file_write_errors`  | File / IO errors recorded by `flush_to_file`.                          |
| `file_rotations`     | Rotations performed by the size threshold.                             |
| `drains`             | Drain calls that successfully shipped at least one event.              |

## Shutdown flush

Components flush the retention store before shutdown so the operator
sees the final tail. `flush_to_file` is bounded: a slow disk only adds
the queue's worth of latency to shutdown, not unbounded backpressure.
