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
  [--streaming]
  [--output <human|json>]
```

`--set-operation replace` (the default) deploys in place of the current
deployment and is sent as today's request. `--set-operation add` admits an
additional resident-set member; the CLI first asks the agent for its control
features and, unless the agent lists `set_operation_add`, refuses locally
with a typed `unsupported` error (exit code `3`) without sending the deploy,
because an agent that predates the field would ignore it and replace the
active deployment. No released agent lists it yet.

`--streaming` starts every generation of the deployment with its stream
listener, and `tensorplate status` then prints the stream endpoint and the
digest a stream Open names. It is sent only to an agent that lists the
`deploy_streaming` control feature, and refused locally with `unsupported`
(exit code `3`) otherwise. An agent lists it when its configuration gives
each worker a stream port, which no packaged configuration does: add a
range of loopback ports holding at least two pairs to the `worker` section
of the agent's configuration, for example
`"serving_port_range": {"first": 18100, "last": 18103}`, and restart the
agent (which stops every worker it runs). The agent refuses `--streaming`
with `config_invalid` (exit code `3`) for a bundle whose class is not
`speech`. A speech worker serving streams refuses unary `/infer` requests;
deploy without the flag for a deployment driven over `/infer`, as
`tensorplate infer` and the qualification recipe are.

What else a streaming deployment means for an operator:

- A deploy or rollback that replaces it, and an agent stop, stop its worker
  as they stop any: SIGTERM and a 5-second drain, after which every open
  stream session is closed with `worker_shutdown`.
- Setting the range moves every worker's HTTP port into it, so
  `serving_url` leaves 18080 and 18081.
- The installed serving worker must be built with streaming support
  (`tensorplate doctor` reports `serving_streaming_support`). One built
  without it refuses the config at start, and the deploy fails with that
  startup error.
- The deployment needs the range for as long as it is kept. With the range
  removed, its restore at a start, a rollback to it and its restarts are
  refused with `config_invalid`.

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
5. In wait mode the CLI polls `ControlOp::Status` until no transaction is
   in flight and a deployment is recorded active, or the wait timeout
   expires. The agent clears a transaction in the write that ends it, so
   the poll does not see a terminal phase (`active`, `failed`,
   `rolled_back`) from this agent; the CLI still accepts one.

The agent answers a `deploy` or `rollback` request only once the candidate
has warmed or failed, which can take as long as its `worker.warm_timeout_ms`
(30,000 by default). The CLI therefore waits at least 120,000 ms for that
answer: the configured `timeout_ms` applies when it is larger, and an
explicit `--timeout-ms` is used as given. A timeout below the agent's warm
timeout can end the command with exit code `4` while the agent is still
inside the transaction, and the next command is then refused as busy.

When the candidate's runner refuses the model while loading, the agent
answers with the runner's typed code (for example `unsupported` or
`oom_error`) as soon as the candidate worker exits.

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
  deployment, or its staged copy or that copy's manifest is gone.
- Typed `load_failed` (exit code `3`) when an artifact of the previous
  deployment's staged copy is missing, truncated or replaced, and
  `config_invalid` (exit code `3`) when the copy's manifest no longer
  validates, as the restore at the agent's start answers them; `internal`
  (exit code `3`), naming the copy, when a file of it cannot be read. No
  transaction begins and the active deployment keeps serving.

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
