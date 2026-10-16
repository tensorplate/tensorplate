# `tensorplate status`

Renders active deployment, worker supervision, and observability state.

```
tensorplate status [--observability-snapshot <path>] [--no-quarantine] [--output <human|json>]
```

## Sources

- **Agent**: `ControlOp::Status` gives the agent's view of the deploy
  transaction, active/previous/candidate deployments, supervision summary,
  quarantine entries, last error, and the optional matched-row platform
  telemetry projection. Startup memory facts are present for accelerator
  rows; signal outcomes appear only when a live collector snapshot was
  supplied. When the agent reports a committed resident set, human output
  adds one line per member and JSON adds the agent's `resident_set` block;
  both are absent otherwise. A member's line ends with `process=`,
  `readiness=` and `draining=` when the agent reports how its worker
  process stands (see [what status reads from the
  registry](../architecture/agent.md#what-status-reads-from-the-registry)).
  When the agent names the generation of the active deployment (the one
  serving it, or beside a resident set of one the committed one), human
  output adds a `generation=` line under `active:`; in JSON, `active`,
  `previous_active` and `candidate` always carry a `generation` key, `null`
  when the agent did not name one. `previous_active.generation` is the
  generation a retained deployment was last started as, for a deployment
  whose record names a deployment descriptor. For a deployment started with
  `deploy --streaming`, human output adds `stream_endpoint=` and
  `descriptor_digest=` under `active:`, the endpoint and digest a stream
  Open names with the generation; in JSON, `active`, `previous_active` and
  `candidate` always carry both keys, non-null only on `active` while its
  serving generation streams. When the agent reports the mode the active
  deployment was admitted under (from its deployment descriptor, which a
  format 0.1 bundle has none of, or from its resident-set member), human
  output adds `admission=production`, or
  `admission=qualification (unqualified) test_count=<n>` for a deployment
  made with `deploy --qualification --test-count <n>`, under `active:`; in
  JSON, the three blocks always carry `admission_mode` and `test_count`
  keys, non-null only on `active`, and `test_count` only in qualification
  mode. Not done yet: `severity` does not judge
  `process` or `readiness`. It judges `contact`, and a member whose process
  is `stopped` or draining reports none, so a `serving` member whose process
  is `stopped` does not lower it.
- **Observability snapshot** (optional): when `--observability-snapshot
  <path>` is supplied, the CLI reads the V01-E10 status snapshot at that
  path (schema: [`protocol/schemas/observability_status.json`](../../protocol/schemas/observability_status.json))
  and folds heartbeat, deadline, and queue-depth fields into the output.

A transaction that a start of the agent ended is read from its quarantine
entry: the transaction id, the deployment id, the phase it had reached
and the error. `last_error` carries the same error only until a
deployment serves; an agent that ended an interrupted deploy and started
the deployment that was serving again reports `ready`, no `last_error`
and one more quarantine entry (see [restart
recovery](../architecture/agent.md#restart-recovery-v01-e08-f07)).
`in_flight_transaction` never carries a `failure` from this agent.
`recovery.reason` says what a start of the agent would do now, so during
a deploy it names that deploy as one a start would end.

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
resident-set members (a quarantined or out-of-contact member is at least
`degraded`; `tensorplate doctor` reports the same members as
`resident_set_members`), and observability state. Crash-loop is surfaced explicitly because the
supervisor's `crash_loop` flag is the early-warning signal V01-E09 publishes.

## Accelerator placement

Supervision includes `desired_device_index` and `actual_device_index`.
Read them alongside `desired_active` and `actual_active`: a present
deployment ID with a null pin means an unpinned worker; an absent ID means
no worker is requested or running on that side. The agent wire protocol
omits absent optional pins, while CLI JSON renders them as `null`.

Human output distinguishes the running worker from the requested placement:

| State | Accelerator line |
| --- | --- |
| Running on the requested device | `accelerator: device=2` |
| Running unpinned while a pin is requested | `accelerator: unpinned (moving to 1)` |
| Running on one device while another is requested | `accelerator: device=0 (moving to 3)` |
| Running pinned while an unpinned replacement is requested | `accelerator: device=0 (removing pin)` |
| Desired deployment cleared, pinned worker still stopping | `accelerator: device=0 (stopping)` |
| Pin requested, worker has not launched or is awaiting restart | `accelerator: device=1 requested, no worker running` |

Ordinary unpinned deployments omit the accelerator line. The requested
and actual pins can differ during initial launch, restart, and placement
changes. The running pin is cleared only when the supervisor removes the
worker from its actual state.

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
      "generation": 4,
      "backend": "tensorrt",
      "serving_url": "http://127.0.0.1:18081/infer",
      "stream_endpoint": null,
      "descriptor_digest": null,
      "admission_mode": "production",
      "test_count": null,
      …
    },
    "previous_active": null,
    "candidate": null,
    "in_flight_transaction": null,
    "supervision": {
      "serving_state": "ready",
      "desired_active": "d-1",
      "actual_active": "d-1",
      "desired_device_index": null,
      "actual_device_index": null,
      "crash_loop": false,
      …
    },
    "platform_telemetry": {
      "row_id": "ubuntu2404-x86-l4-g2s8",
      "validated": true,
      "memory": { "memory_profile": "discrete_gpu", … }
    },
    "quarantined": [],
    "last_error": null,
    "resident_set": {                 // present only with a committed set
      "set_id": "…",
      "revision": 3,
      "members": [
        {
          "deployment_id": "speech-tts",
          "generation": 5,
          "bundle_digest": "sha256:…",
          "state": "serving",
          "admission_mode": "production",
          "quota": { "session_count": 1, "domain_bytes": { … } },
          "stream_endpoint": "127.0.0.1:18105",
          "contact": "in_contact",
          "process": "running",
          "readiness": "ready",
          "draining": false
        }
      ]
    }
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
