# `tensorplate status`

Renders active deployment, worker supervision, and observability state.

```
tensorplate status [--observability-snapshot <path>] [--no-quarantine] [--output <human|json>]
```

## Sources

- **Agent**: `ControlOp::Status` gives the agent's view of the deploy
  transaction, active/previous/candidate deployments, supervision summary,
  quarantine entries, and last error.
- **Observability snapshot** (optional): when `--observability-snapshot
  <path>` is supplied, the CLI reads the V01-E10 status snapshot at that
  path (schema: [`protocol/schemas/observability_status.json`](../../protocol/schemas/observability_status.json))
  and folds heartbeat, deadline, and queue-depth fields into the output.

If the snapshot file is missing or malformed, the CLI renders the
observability block as `available: false` with a reason — it does **not**
fail the status command. Operators see a complete picture even when one
data source is down.

## Severity ordering

The `payload.severity` field is a stable label used by release validation:

```
ready < degraded < no_heartbeat < crash_loop < failed
```

The CLI picks the highest severity across agent state, supervision state,
and observability state. Crash-loop is surfaced explicitly because the
supervisor's `crash_loop` flag is the early-warning signal V01-E09 publishes.

## JSON payload skeleton

```jsonc
{
  "agent_response_status": "ok",
  "severity": "ready",
  "agent": {
    "available": true,
    "agent_state": "ready",
    "active": {
      "deployment_id": "d-1",
      "backend": "tensorrt",
      "serving_url": "http://127.0.0.1:18081/infer",
      …
    },
    "previous_active": null,
    "candidate": null,
    "in_flight_transaction": null,
    "supervision": { "serving_state": "ready", "crash_loop": false, … },
    "quarantined": [],
    "last_error": null
  },
  "observability": {
    "available": true,
    "observability_state": "ready",
    "missed_heartbeat_count": 0,
    "missed_deadline_rate": 0.0,
    "queue_depth": 0,
    …
  }
}
```
