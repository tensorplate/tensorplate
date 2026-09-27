# `tensorplate deploy`, `rollback`, `undeploy` and `recover`

These commands wrap the agent's deploy-transaction API. The CLI never verifies,
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
  [--set-operation <replace|add>]   # default: replace
  [--output <human|json>]
```

`--set-operation replace` (the default) deploys in place of the current
deployment and is sent as today's request. `--set-operation add` admits an
additional resident-set member; the CLI first asks the agent for its control
features and, unless the agent lists `set_operation_add`, refuses locally
with a typed `unsupported` error (exit code `3`) without sending the deploy,
because an agent that predates the field would ignore it and replace the
active deployment. No released agent lists it yet.

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
tensorplate rollback [--reason <text>] [--deployment-id <id>] [--output <human|json>]
```

`--deployment-id` names the resident-set member to roll back to its retained
previous generation; it is required when the set has more than one member.
Like `--set-operation add`, it is sent only to an agent that lists the
`member_rollback` control feature, and refused locally with `unsupported`
(exit code `3`) otherwise. Without it, `rollback` is the singleton rollback.

Calls `ControlOp::Rollback` and reports:

- `transaction_id` of the rollback transaction.
- `restored_deployment_id`, `restored_bundle_digest`, `restored_backend`.
- Typed `unavailable` (exit code `6`) when there is no previous active
  deployment.

## `tensorplate undeploy` and `tensorplate recover`

```
tensorplate undeploy --deployment-id <id> [--reason <text>] [--output <human|json>]
tensorplate recover  --deployment-id <id> [--reason <text>] [--output <human|json>]
```

`undeploy` retires one resident-set member; `recover` returns a quarantined
member to service (`undeploy` removes it instead). Each sends the agent's
`undeploy` or `recover` operation with the member and reports its
`deployment_id` and `transaction_id`. An agent that knows the operation but
does not execute it answers with a typed `unsupported` error (exit code
`3`), which the CLI reports unmodified; the current agent executes neither
yet. An agent from before these operations refuses the unknown operation
with `config_invalid` (also exit code `3`). Over `--device`, both forward to
the device's CLI.
