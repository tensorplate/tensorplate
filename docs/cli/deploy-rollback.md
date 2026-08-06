# `tensorplate deploy` and `tensorplate rollback`

Both commands wrap the agent's deploy-transaction API. The CLI never verifies,
stages, warms, promotes, or quarantines bundles itself — the agent does that
work and the CLI projects the response.

## `tensorplate deploy <bundle>`

```
tensorplate deploy <bundle-path>
  [--deployment-id <id>]            # default: deploy-<uuid4>
  [--expected-digest algo:hex]
  [--no-wait]                       # default: wait until terminal
  [--wait-timeout-ms <n>]           # default: 120000
  [--label key=value]               # may be repeated
  [--output <human|json>]
```

Behavior:

1. The CLI checks that `<bundle-path>` exists, is a directory, and contains a
   `manifest.json` file. Any of those failures returns exit code `2` before
   any agent call.
2. An explicit deployment ID must be 1–128 ASCII bytes using only letters,
   digits, `.`, `-`, or `_`; the path components `.` and `..` are reserved.
   Both the CLI and agent enforce this before transaction state or staging
   paths are created.
3. The bundle path is canonicalized so the agent always sees an absolute
   path.
4. The CLI sends a `ControlOp::Deploy` request and reports the
   `transaction_id` it gets back.
5. In wait mode the CLI polls `ControlOp::Status` until the in-flight
   transaction reaches a terminal phase (`active`, `failed`, `rolled_back`)
   or the wait timeout expires.

Exit codes:

| Code | When |
| --- | --- |
| `0` | Transaction reached `active`. |
| `2` | Local bundle path rejected. |
| `3` | Agent returned a typed error (e.g. backend unavailable). |
| `4` | Transport / timeout. |
| `5` | Agent busy with another transaction. |
| `6` | Reserved profile mode. |

JSON payload:

```json
{
  "agent_response_status": "ok",
  "transaction_id": "tx-…",
  "phase": "active",
  "deployment_id": "d-1",
  "bundle_digest": "sha256:…",
  "failure": null
}
```

## `tensorplate rollback`

```
tensorplate rollback [--reason <text>] [--output <human|json>]
```

Calls `ControlOp::Rollback` and reports:

- `transaction_id` of the rollback transaction.
- `restored_deployment_id`, `restored_bundle_digest`, `restored_backend`.
- Typed `unavailable` (exit code `6`) when there is no previous active
  deployment.
