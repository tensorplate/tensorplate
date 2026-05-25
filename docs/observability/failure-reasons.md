# Failure Reason Taxonomy (V01-E12-F03)

Failure reasons normalise local error explanations so CLI consumers
render them without parsing free-form messages. Each reason maps to
exactly one [`ErrorCode`](../../protocol/schemas/error.json) and one
category; the mapping is the source of truth across the agent,
serving worker, runtime, sidecar adapter, observability, and CLI.

Wire format: [`protocol/schemas/failure_reason.json`](../../protocol/schemas/failure_reason.json).
Rust mirror: [`protocol::failure_reason`](../../protocol/rust/src/failure_reason.rs).

## Reasons

| Reason                          | Category    | Severity  | Retryable | Error code        |
| ------------------------------- | ----------- | --------- | --------- | ----------------- |
| `config_invalid`                | config      | critical  | no        | `config_invalid`  |
| `bundle_schema_invalid`         | bundle      | critical  | no        | `config_invalid`  |
| `bundle_integrity_failed`       | bundle      | critical  | no        | `config_invalid`  |
| `unsupported_runtime`           | platform    | critical  | no        | `unsupported`     |
| `unsupported_hardware`          | platform    | critical  | no        | `unsupported`     |
| `backend_unavailable`           | backend     | critical  | no        | `load_failed`     |
| `backend_unsupported_capability`| backend     | error     | no        | `unsupported`     |
| `shape_mismatch`                | backend     | error     | no        | `shape_mismatch`  |
| `oom`                           | backend     | error     | no        | `oom_error`       |
| `timeout`                       | backend     | error     | yes       | `timeout`         |
| `deadline_missed`               | backend     | warning   | yes       | `timeout`         |
| `sidecar_startup_failed`        | sidecar     | critical  | no        | `load_failed`     |
| `sidecar_malformed_response`    | sidecar     | error     | no        | `inference_failed`|
| `sidecar_process_exit`          | sidecar     | critical  | yes       | `load_failed`     |
| `worker_not_ready`              | supervision | error     | no        | `not_ready`       |
| `worker_exit`                   | supervision | error     | yes       | `not_ready`       |
| `worker_crash_loop`             | supervision | critical  | no        | `not_ready`       |
| `no_heartbeat`                  | heartbeat   | critical  | yes       | `not_ready`       |
| `permission_denied`             | permission  | critical  | no        | `unsupported`     |
| `internal`                      | internal    | critical  | no        | `internal`        |

The taxonomy is union-stable: post-v0.1.0 additions append rather than
rename. Each reason carries an optional bounded `detail` string
(truncated to
[`MAX_FAILURE_DETAIL_BYTES`](../../protocol/rust/src/failure_reason.rs))
and an optional `correlation_id` joining the failure with logs and
metrics.

## Producer contract

- Construct records through
  [`FailureReasonRecord::new`](../../protocol/rust/src/failure_reason.rs)
  so the category, severity, retryable hint, and error code are filled
  from the canonical mapping. Callers may override the severity for a
  specific context (e.g. a transient `deadline_missed` during warmup
  stays a warning while a saturated `deadline_missed` becomes
  critical).
- Decode-time validation rejects records whose `category` or
  `error_code` field does not match the canonical mapping. This catches
  drift between hand-edited fixtures and the taxonomy.
- The detail string must redact absolute paths, environment values,
  and credentials before emission. The agent and serving worker do
  this at the boundary; CLI doctor does it again when rendering remote
  records.

## Consumer contract

- The CLI renders the reason and category directly; it surfaces the
  error code only when release validation grep assertions require it.
- The status snapshot carries the most recent failure as
  `last_failure_reason` (see [snapshot](../../protocol/schemas/observability_status.json)).
- Metrics aggregation counts failure categories — never reasons — to
  keep label cardinality bounded.
