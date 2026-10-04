# `tensorplate-agent` (V01-E08)

`tensorplate-agent` is the Rust management-plane service that turns a
running `tensorplate-serving` process into a device appliance. It owns the
durable desired-state store, the deploy transaction state machine, bundle
verification, rollback to the previous active deployment, and the
prepare/warm/promote handoff with the V01-E07 serving worker.

This document records the V01-E08 architecture decisions. It is the
single source of truth for the cross-component contracts the CLI
(V01-E11), the observability service (V01-E10), and the package layout
(packaging) build on top of.

## Layering

```
                       ┌───────────────────────┐
                       │  tensorplate-cli       │
                       │  (V01-E11)             │
                       └────────────┬───────────┘
                                    │ ControlRequest / ControlResponse
                                    │ over Unix domain socket (NDJSON)
                                    ▼
                       ┌───────────────────────┐
                       │  tensorplate-agent     │
                       │                        │
                       │  ┌──────────────────┐  │
                       │  │ control::dispatch│  │  pure functions
                       │  └────────┬─────────┘  │
                       │           │            │
                       │  ┌────────▼─────────┐  │
                       │  │   Coordinator    │  │  F04 / F05 / F06
                       │  └─┬───────┬────────┘  │
                       │    │       │           │
                       │  ┌─▼─┐   ┌─▼─┐         │
                       │  │ S │   │ B │         │  F02 / F03
                       │  │ t │   │ u │         │
                       │  │ a │   │ n │         │
                       │  │ t │   │ d │         │
                       │  │ e │   │ l │         │
                       │  │ S │   │ e │         │
                       │  │ t │   └───┘         │
                       │  │ o │                 │
                       │  │ r │                 │
                       │  │ e │                 │
                       │  └─┬─┘                 │
                       │    │                   │
                       │    │ atomic file       │
                       │    │ replace           │
                       │    ▼                   │
                       │  state.json            │
                       │  state.json.bak        │
                       └───────────┬────────────┘
                                   │ WorkerControl trait
                                   ▼
                       ┌───────────────────────┐
                       │ tensorplate-serving    │
                       │ (V01-E07 data plane)   │
                       └───────────────────────┘
```

Layer rules:

- The agent never links against the C++ runtime or the serving worker.
- The serving worker is supervised through the typed
  `WorkerControl` trait. v0.1.0 ships both the deterministic
  `MockWorkerControl` used by host CI and a process-backed
  `ProcessWorkerControl` that renders a V01-E07 serving config, starts
  `tensorplate-serving`, polls `/health`, and promotes only warmed
  candidates.
- The CLI never speaks to the serving worker directly; every mutating
  operation flows through the agent.

## Local control API (V01-E08-F01)

The control transport is a **Unix domain socket** by default. The
rationale:

- The agent and CLI run on the same device in v0.1.0. UDS keeps the
  attack surface off the loopback interface entirely.
- Socket permissions follow the package trust boundary. Homebrew uses an
  owner-only runtime directory (`0o700`) and socket (`0o600`) because the
  agent service and interactive CLI run as the same macOS user. Native Linux
  packages use a `tensorplate`-owned runtime directory (`0o750`) and socket
  (`0o660`), where membership in the dedicated `tensorplate` group is the
  explicit operator authorization boundary. Neither channel grants world
  access.
- The wire format is **newline-delimited JSON**: one request per
  connection, one response, then close. This avoids reinventing HTTP and
  keeps the test surface small.

`config/schemas/agent.json` documents the wire format for the config
file; `protocol/schemas/agent_control.json` documents the request /
response envelope. Loopback TCP is supported as an opt-in for
environments without UDS (rare; recorded as a future-compatibility
escape hatch).

### Request shape

```json
{
  "schema_version": "0.1",
  "correlation_id": "optional-caller-supplied",
  "op": "deploy",
  "deploy": {
    "bundle_path": "/var/lib/tensorplate/bundles/yolov8n",
    "deployment_id": "deploy-2024-1",
    "expected_bundle_digest": "sha256:cafebabe...",
    "labels": { "env": "lab" }
  }
}
```

Supported ops: `deploy`, `status`, `rollback`, `health`, `version`,
`undeploy` and `recover`. Each payload belongs to its op: `deploy` requires
`deploy`; `undeploy` and `recover` require a member payload
(`{"deployment_id": ..., "reason": ...}`); `rollback` and `status` take an
optional payload; `health` and `version` take none. The agent refuses a
request carrying a field it does not know, a payload that belongs to
another op, or an explicit `null`, with `config_invalid`, so a field a later
client adds is refused rather than ignored.

### Set mutation

The resident set (the deployments kept loaded together, each a member at a
deployment generation) is changed through the same operations:

- `deploy.set_operation` is `replace` (the default, and what an absent field
  means) or `add`. `replace` keeps the singleton semantics; in a resident
  set it deploys a new generation of the named member, and in a set of more
  than one member it must name a current member. `add` admits an additional
  member.
- `rollback.deployment_id` names the member to roll back to its retained
  previous generation. It is required when the set has more than one member.
- `undeploy` retires a member; `recover` returns a quarantined member to
  service (`undeploy` removes it instead).
- Operator-only deploy fields: `admission_mode` (`production`, the default,
  or `qualification`, which admits an unqualified member under the explicit
  `test_count`, required with it and refused without it; the test count
  becomes the member's quota `session_count`, so it is 1 to 2048 and a
  larger one is refused at decode) and `evidence_ref`
  (the approved evidence reference of an ordinary activation, refused in
  qualification mode). Speech clients do not send them: the SDK sends only
  the status query, which refuses them at decode, and the serving worker's
  unary endpoint takes no admission field.

This agent executes none of these yet. `undeploy`, `recover`, `add`,
qualification admission, an `evidence_ref`, a rollback naming a member, and
any deploy or rollback while the durable state records a resident set are
answered with a typed `unsupported` error before any transaction starts, so
nothing is staged and the agent does not become busy. The two requests the
contract always refuses (`replace` naming a non-member, and a rollback naming
nobody, in a set of more than one member) are refused with `config_invalid`
first.

`agent_status.control_features` lists what the agent executes beyond the
singleton deploy and rollback: `set_operation_add` and `member_rollback`.
This agent lists neither. An agent that predates `set_operation` or
`rollback.deployment_id` ignores them and would act on the rest of the
request, so a client sends `add` or a member rollback only to an agent that
lists the matching feature; the CLI checks before sending and otherwise
refuses locally with `unsupported`.

### Response shape

```json
{
  "schema_version": "0.1",
  "correlation_id": "echoed",
  "status": "ok",
  "transaction_id": "tx-uuid",
  "deploy_status": { "phase": "active", ... },
  "agent_status": { ... }
}
```

`status` is one of `ok`, `error`, `busy`, `not_found`, `unavailable`.
Errors carry a typed `code` matching `tensorplate_protocol::ErrorCode`,
so the CLI (V01-E11) and the observability service (V01-E10) see the
same stable code surface as the C++ runtime.

For an admitted accelerator row, `agent_status.platform_telemetry` is an
optional additive block containing the row identity, validation state, and
startup memory facts. A resolved live signal snapshot contains all five
stable signal names; applicable omissions are explicit `unavailable`
outcomes, while an absent snapshot omits the `signals` field.
Context-only failures degrade agent status without blocking deployment;
load-bearing failures do both.

When the durable state records a resident set, `agent_status.resident_set`
lists it: `set_id`, `revision`, and one entry per member in committed order
with its `deployment_id`, `generation`, `bundle_digest`, `state` (`serving`
or `quarantined`), `admission_mode`, committed `quota`, and its unary and
stream endpoints from the committed endpoint map. `stream_api_version`,
`effective_quota` (the quota the member's worker reports in force),
`staged_bytes` and `contact` (`in_contact` or `out_of_contact`) stay absent
until their sources exist. Beside a resident set the durable singleton slots
are empty; a set whose only member is serving projects that member into
`active` (with `serving_url`, the `/infer` URL of its unary endpoint when
that endpoint is `http://127.0.0.1:<port>` or `http://localhost:<port>` with
no path) and its retained generation into `previous_active`. Clients that
read only `active` keep working on such a set when `active` carries
`serving_url`. A set of any other shape fills neither, and clients read
`resident_set`. A client that finds `resident_set` but no
`active.serving_url` has no single unary route to discover and must not
assume the v0.1 loopback default, which could reach any member's listener or
one unrelated to all of them: `tensorplate infer` refuses with
`unavailable` and the Python SDK raises `EndpointUnavailableError`. A client
that reads only `active` and falls back to the default without a
`serving_url`, as every CLI and SDK before this change does, misroutes on a
stream-only or off-loopback set of one.

## Memory admission configuration

The optional `memory_admission` block in `config/schemas/agent.json` joins a
`row_id` to a `memory_profile_instance_id` and requires both `guest_ram` and
`device_vram` entries under `domains`. Each entry supplies `reserve_bytes`
explicitly and may supply `cap_bytes`. Byte counts are nonnegative exact JSON
integers up to 2^53−1; a configured cap is positive. Configuration validation
rejects a reserve above an explicit cap. `MemoryDomainConfig::resolve_cap`
requires a positive measured capacity, uses it when the cap is omitted, and
rejects a cap above measurement or a reserve above the resolved cap.

This is a configuration shape for subsequent live admission integration;
startup does not yet call `resolve_cap`, verify the row/profile join, start
sampling or change singleton admission. Packaged configurations omit the
block because their reserves require measurements under load. An older agent
rejects the new block as an unknown field. No nominal figure supplies an
absent measurement. The platform parsers and observation semantics are
specified in [memory observations](memory-observation.md).

## Durable state store (V01-E08-F02)

The store persists exactly two files in the agent's state directory:

- `state.json` — the current, latest-committed state.
- `state.json.bak` — the same bytes, written by every mutation. Consulted
  when `state.json` is missing, empty or fails to decode — but never when
  `state.json` is refused for its state version, because a newer file
  supersedes whatever older backup sits beside it, and never when
  `state.json` cannot be read at all (an I/O error stops the agent). After a failed 0.2
  write the backup may hold the state that write attempted until the next
  write succeeds; it is read only if `state.json` is missing or damaged.

Every mutation:

1. Walks an in-memory clone of the current state through a closure.
2. Bumps `store_version` and stamps the state version (below).
3. Refuses the result, before writing anything, if it would remove or
   lower the deployment generation counter, remove the resident set,
   change its `set_id`, change it without advancing its `revision`, add a
   generation below the newest one the set already names, or not decode
   back to exactly the same state.
4. Writes each file to a sibling `.tmp` file, `fsync`s it and `rename(2)`s
   it over its target (atomic on POSIX), syncing the directory after each
   rename. A state-version-0.1 state writes `state.json` first and then the
   backup, as every earlier release did. A state-version-0.2 state writes
   the backup first and `state.json` last: an agent through 0.2.x falls back
   to the backup when `state.json` does not decode, so a 0.2 `state.json`
   must never sit beside a 0.1 backup; and with `state.json` renamed last, a
   write that fails before that rename has not committed while `state.json`
   decodes. For a 0.2
   write both directory syncs must succeed on Linux, so a state the store
   acknowledged, and any generation it handed out, survives power loss;
   elsewhere, and for a 0.1 write, the syncs are best-effort.
5. Commits at the rename that makes the new state what the next start
   reads: `state.json`'s for a 0.1 write and while `state.json` decodes;
   the backup's when a 0.2 write renames the backup first over a
   `state.json` that does not decode (missing, empty or damaged when the
   store opened, and no write has succeeded since). A write that fails at
   or after that rename, including a required directory sync, may or may
   not be what the next start reads: it returns `StateIndeterminate`, and
   the store refuses every later write until the agent restarts and
   re-reads the durable state, so nothing is written from memory that may
   be stale. Until then status reports `agent_state` `failed` with a
   `last_error` saying so.

The store assumes it is the directory's only writer; the mutex serializes
writers inside one agent process, and nothing stops a second agent process.

The store never persists model bytes, request payloads, or unbounded
logs — only digests, paths, and bounded error metadata. The
quarantine list is capped at 32 entries; oldest entries are dropped on
overflow.

## Bundle verifier (V01-E08-F03)

`bundle::verify` is the single deploy-time gate. It checks, in order:

1. The bundle path exists and is a directory.
2. `manifest.json` is readable JSON with `schema_version: "0.1"`.
3. `format_version` is exactly `0.1` or `0.2`, then the corresponding
   payload decodes through the shared protocol bundle parser, including
   the format 0.2 [manifest-local rules](../bundles/compatibility.md#format-02-manifest-rules).
4. Each declared artifact's `sha256` digest matches its content.
5. (Optional) the manifest's `manifest_digest` field matches the canonical
   manifest with that field stripped.
6. `runtime_compatibility` range includes the agent's runtime version.
7. `target_hardware.device_family` matches the agent's configured family
   (or is `any`).
8. `target_hardware.min_memory_bytes` / `memory_estimate_bytes` fit
   within `agent.device_memory_bytes`.
9. `backend_hint` is in `agent.available_backends`. **No heuristic
   fallback.** Bundles that declare an unavailable backend are rejected
   with the typed `Unsupported` error.
10. `capability_requirements` are satisfied by the configured
    `backend_capabilities` map. Missing capabilities are rejected.

Manifest-local rule failures retain their typed rule code in the existing
`ErrorRecord.context`, through the control response and quarantine record. They
leave the active deployment unchanged and reach neither staging nor the worker.

## Deploy transaction state machine (V01-E08-F04)

Phases (forward-only along the success path):

```
received -> verified -> staged -> capacity_checked -> prepared
          -> warmed -> promoted -> active
```

Terminal failure states: `failed`, `rolled_back`.

Replayable phases (safe to retry from scratch on restart): `received`,
`verified`, `staged`, `capacity_checked`. Worker-side phases
(`prepared`, `warmed`, `promoted`) are not replayable; a candidate
interrupted there is quarantined.

The coordinator persists each phase to the durable state store
**before** the next phase begins. A crash mid-transaction therefore
either leaves the in-flight transaction at the last successfully
persisted phase (which the recovery planner can read) or at the
phase-just-before that one if the failing phase did not commit.

## Serving-worker handoff (V01-E08-F05)

The agent stages the verified bundle into
`<staging_dir>/<deployment_id>/` (copies the manifest + all declared
artifacts) and then hands the candidate to the worker via the typed
`WorkerControl` trait:

```rust
pub trait WorkerControl: Send + Sync {
    fn prepare(&self, transaction_id, candidate, timeout) -> Result;
    fn warm(&self, transaction_id, candidate, timeout) -> Result<WorkerReadiness>;
    fn promote(&self, transaction_id, candidate) -> Result;
    fn unload(&self, deployment_id);
    fn active_deployment_id(&self) -> Result<Option<String>>;
}
```

`promote` is the only call that mutates the worker's active deployment.
`unload` is best-effort (failure is logged but never undoes a successful
promotion).

The process-backed implementation is selected with
`worker.mode = "process"` and requires an absolute
`worker.serving_binary_path`. The agent writes per-candidate serving
configs under `worker.serving_config_dir` (default:
`<state_dir>/worker-configs`), starts the worker on loopback, and polls
`/health` until the candidate reports `ready`. Host CI and unit tests use
`worker.mode = "mock"` so the transaction coordinator is tested without
requiring hardware backends.

Each worker's stderr is a pipe the agent copies line by line to its own
stderr, so under systemd the worker's and the sidecar's log lines are in
the agent's journal. Nothing is added to them or removed from them.

While it waits for a candidate to warm, the agent also checks whether the
candidate process has exited. When it has, the deploy (or rollback) fails
at once instead of at `worker.warm_timeout_ms`:

- If the worker ended its stderr with a startup failure record (see
  [serving-worker.md](serving-worker.md)), the transaction fails with the
  record's code and message — a runner that refuses a load as `unsupported`
  or `oom_error` reaches the operator as that code. The agent waits up to
  one second for the record after it sees the exit.
- Otherwise it fails with `load_failed` and the worker's exit status.

Either way the candidate is quarantined, `last_error` carries the same
code, the active deployment is untouched and no transaction is left in
flight, so the next `deploy` or `rollback` is accepted. A candidate that
stays alive without becoming ready still fails at the warm timeout, as
`not_ready` when it answers `/health` and `inference_failed` when nothing
listens.

## Rollback (V01-E08-F06)

Rollback is a transaction, not a file-pointer swap:

1. Read the previous active deployment from durable state.
2. Verify its staged files (`staged_path/` exists and contains
   `manifest.json`).
3. Walk the same prepare / warm / promote sequence as deploy.
4. On success, swap active <-> previous_active in durable state and
   mark the transaction `rolled_back`.

A failed rollback preserves the current active deployment — the
previous-active record is left intact, and the operator can retry.

## Restart recovery (V01-E08-F07)

The recovery planner reads durable state and (best-effort) the worker's
actual active deployment, and returns one of:

- `no_op` — desired and actual agree.
- `resume_verify` / `resume_stage` / `resume_prepare` — replayable
  in-flight phase, safe to retry from scratch.
- `quarantine_candidate` — in-flight transaction stopped at a
  worker-side phase; agent moves it to the quarantine list and clears
  the candidate slot.
- `restore_active` — desired active recorded, worker reports no
  active deployment (typical on a fresh device boot).
- `operator_required` — desired and actual disagree in a way recovery
  can't reason about (e.g., the worker is running a deployment that is
  not the recorded active).

On process startup, the agent applies the recovery action before binding
the local control socket. Replayable transactions are resumed through the
normal coordinator path, unsafe worker-side candidates are quarantined,
and promoted-but-not-finalized transactions are finalized only when the
worker-reported active deployment matches the transaction target.

Recovery is **state-diff based**; the planner never replays commands
just because they appeared in the original request order.

## Versioning policy

Every control API payload carries `schema_version` const-fixed to `0.1`.
The agent rejects unknown schema versions on the control API with the typed
`Unsupported` error code. The set-mutation fields and operations are
additive under `0.1`; [`protocol.md`](protocol.md#versioning) states how
agents that predate them are kept from misreading them.

The on-disk state file has its own version track
(`protocol/schemas/agent_state.json`). State version `0.1` is the singleton
layout every agent through 0.2.x reads. State version `0.2` adds the
deployment generation counter (`next_generation`, never removed or lowered,
so the agent never hands out the same generation twice from one state
file; the allocator takes an inclusive floor, and a caller that knows of
generations recorded elsewhere, such as staged roots left by an earlier
state directory, passes one more than the highest of them, getting the
typed exhausted-counter error when no generation remains above it)
and the resident set (members at their generations, retained
previous generations, and the committed endpoint map; the singleton
`active`, `previous_active` and `candidate` slots are absent beside it). The
agent stamps the oldest state version whose readers decode the file without
loss: `0.2` from the first generation it allocates, `0.1` until then. An
agent through 0.2.x refuses a `0.2` file with its typed unsupported-version
error and exits instead of misreading it; nothing migrates a state file
backwards, and a downgrade sets the state directory aside instead. The
current agent refuses an unknown state version the same way, and a `0.2`
file with any field it does not know.

The durable state file is read only by the agent. The CLI (V01-E11) and
observability (V01-E10) read the status the agent projects from it, and the
deploy transaction phase names are load-bearing for both.

## Worker runtime control transport

The reusable `ControlChannel` owns a socket, bounded command queue and dedicated
ledger-polling thread for one member generation. Its typed contact transitions
are available through a bounded snapshot; process recovery is a registry
responsibility. `spawn_with_control` supplies the corresponding safe stdin socket
handoff. These APIs have no production caller yet and do not change the existing
process managers. See [runtime control client](worker-supervision.md#runtime-control-client)
for framing, deadlines, ownership and loss-of-contact behavior.
