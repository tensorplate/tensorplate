# `tensorplate-agent` (V01-E08)

`tensorplate-agent` is the Rust management-plane service that turns a
running `tensorplate-serving` process into a device appliance. It owns the
durable desired-state store, the deploy transaction state machine, bundle
verification, rollback to the previous active deployment, and the
prepare/warm/fence/activate handoff with the V01-E07 serving worker.

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
  `WorkerControl` trait. Its implementations are the deterministic
  `MockWorkerControl` used by host CI and the process-backed
  [member registry](#member-registry), which owns every
  `tensorplate-serving` process a `process`-mode agent starts.
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
singleton deploy and rollback: `set_operation_add`, `member_rollback` and
`deploy_streaming`. This agent lists `deploy_streaming` when its
configuration gives stream ports, and neither of the others. An agent that
predates `set_operation`, `rollback.deployment_id` or `deploy.streaming`
ignores them and would act on the rest of the request, so a client sends
`add`, a member rollback or a streaming deploy only to an agent that lists
the matching feature; the CLI checks before sending and otherwise refuses
locally with `unsupported`.

### Streaming

`deploy.streaming: true` asks for a deployment whose workers serve stream
sessions. Without it, which is what every deploy before the field meant, a
worker serves unary requests only. A speech worker serving streams refuses
unary `/infer`, so a deployment driven over `/infer`, as the qualification
recipe's are, does not set it.

The agent refuses `streaming` with `config_invalid` before anything is
staged when the bundle's class is not `speech`, and when its configuration
gives no stream ports (`worker.serving_port_range`, which no packaged
configuration sets; see [the member registry's
ports](#member-registry)). Otherwise the deployment record keeps the flag
(`streaming: true`, written only when set), so a rollback to the
deployment, the restore at the next start and a restart start it the same
way.

Each generation started with streaming is given, in its serving config,
`streaming: {"enabled": true, "port": <stream port>}` on the stream port
its generation holds, and `deployment.descriptor_digest`: the descriptor's
digest, or for a format 0.1 bundle the manifest's digest; the worker echoes
it and verifies nothing. A restart's generation names its own re-stamped
descriptor. Status reports, on `active`, the `stream_endpoint` (a loopback
`host:port`) and that `descriptor_digest` beside the `generation`, the three
a stream Open names, while the generation serving the active deployment was
started with streaming.

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
stream endpoints from the committed endpoint map. `process`, `readiness`,
`draining` and `contact` say how the worker process of that committed
generation stands, [as the member registry holds
it](#what-status-reads-from-the-registry); an agent whose worker control
reports no processes (`worker.mode: mock`) leaves all four absent.
`effective_quota` is the quota the member's worker answered its last
`activate` with, while its process runs; it is absent before a worker has
answered one `ok` and once it answers one `error` (see
[Admission](#member-registry)).
`stream_api_version` and `staged_bytes` stay absent until their sources
exist.
Beside a resident set the durable singleton slots
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

`bundle::verify_before_staging` is the single deploy-time gate; the
coordinator passes it the durable state, the platform registry, the
platform admission and the backend probe reports it holds. A format 0.2
manifest that declares a variant is first judged against the bundles the
agent is serving (see
[rules judged against the target](../bundles/compatibility.md#rules-judged-against-the-target)).
The gate then checks, in order:

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
11. For format 0.2, every `hardware_compatibility` id is a row of the loaded
    registry, and a `production` request names only Production rows.
12. The backend's probe report, and the named runner profile's, is
    `Runnable`; a named runner profile belongs to a backend that declares
    runner profiles and lists the manifest's `compute_type`.

Rule failures, the parser's and these, retain their typed rule code in the existing
`ErrorRecord.context`, through the control response and quarantine record. They
leave the active deployment unchanged and reach neither staging nor the worker.

## Deploy transaction state machine (V01-E08-F04)

Phases (forward-only along the success path):

```
received -> verified -> staged -> capacity_checked -> prepared
          -> warmed -> promoted -> active
```

The agent records the phases up to `promoted` on the transaction in
flight. `promoted` is recorded by the commit: one state write moves the
records (`active` to `previous_active` and the candidate to `active` for a
deploy, the swap for a rollback) and sets the phase, so a start that finds
`promoted` with the records moved reads a committed transaction. The commit
is written after the candidate is fenced and before it is activated, so the
member it replaces is told to retire only once the commit is durable (see
[Serving-worker handoff](#serving-worker-handoff-v01-e08-f05)). `active`,
`failed` and `rolled_back` remain values of the phase and are no longer
written to a transaction in flight: the write that ends a transaction
clears it.

A transaction ends in one state write:

- after its commit, the write that clears it also clears `last_error`;
- a deploy that fails is moved to the quarantine list, with the phase it
  had recorded and the failure, in the write that clears it and its
  candidate record;
- a rollback that fails is cleared in the write that records the failure
  as `last_error`; it makes no quarantine entry, because the record it
  would have restored is still a valid deployment.

No phase is run again at a start, whatever it is: see
[Restart recovery](#restart-recovery-v01-e08-f07).

The coordinator persists each phase to the durable state store
**before** the next phase begins. A crash mid-transaction therefore
either leaves the in-flight transaction at the last successfully
persisted phase (which the recovery planner can read) or at the
phase-just-before that one if the failing phase did not commit.

Every state write inside a transaction can fail without a crash (a full
disk): the write that begins it aside, these are the write of a phase, of
the bundle digest, of the candidate record or of a generation, the write
that ends a failed transaction, the commit, and the write that ends a
committed one. A refused commit write ends its transaction at once: the
candidate is given up, and the write that ends a failed transaction records
the failure, with a quarantine entry at `warmed` for a deploy and as
`last_error` for a rollback. Otherwise, and when that write fails too, a
write that fails before its commit rename (see the
[durable state store](#durable-state-store-v01-e08-f02)) changes nothing:
the request is answered with an `internal` error and the transaction stays
in flight at the phase it had recorded. Nothing clears it while the agent
runs. Every later `deploy` and `rollback` is refused: the control response
is `status: busy` with the error `not_ready`, "agent_busy". It carries no
transaction id; status shows the transaction as `in_flight_transaction`.
Restart the agent: its start settles the transaction (see
[Restart recovery](#restart-recovery-v01-e08-f07)). What that start finds
depends on the write that failed:

- A write before the commit, the commit and the one that ends a failed
  transaction included: the candidate, if one was prepared, is abandoned
  when the request returns, and what served before goes on serving. The
  start ends the transaction as failed, with a quarantine entry.
- The write that ends a committed transaction. The deployment is committed
  and serving, and status shows the new deployment as `active` beside
  `in_flight: phase=promoted`. The start clears the transaction, makes no
  quarantine entry and starts the committed deployment again.

A commit write that fails at or after its rename (`StateIndeterminate`)
gives the candidate up too, and the member that served goes on serving. The
store then refuses every write, so the transaction stays in flight until the
agent restarts. The next start reads whichever state the rename left, and
settles the transaction forward when it was committed.

A transaction in flight never carries a `failure`: the write that records
why a transaction failed is the one that clears it, so this agent never
sets `in_flight_transaction.failure` in status. The failure is read from
the quarantine entry or from `last_error`.

A deploy of a format 0.2 bundle derives the bundle's
[deployment descriptor](#deployment-descriptor-documents) after
`capacity_checked`, directly before it asks for a worker: it verifies the
staged copy again, takes the generation, names the descriptor on the
candidate record and on the transaction, and writes its document, in one
state update. The promotion moves that identity to `active` with the
candidate record, and refuses a candidate that does not name the identity
its deploy's transaction names. A deploy of a format 0.1 bundle derives
nothing and writes what it wrote before: its records carry no identity, no
document exists, and its worker's generation is taken by the registry
inside `prepare`.

## Serving-worker handoff (V01-E08-F05)

The agent copies the verified bundle (the manifest and every declared
artifact) into a tree of its own, `<staging_dir>/<deployment_id>/<transaction
id>/` (see [Staged trees](#staged-trees)), and then hands the candidate to
the worker via the typed `WorkerControl` trait:

```rust
pub trait WorkerControl: Send + Sync {
    fn prepare(&self, transaction_id, candidate, timeout) -> Result;
    fn warm(&self, transaction_id, candidate, timeout) -> Result<WorkerReadiness>;
    fn fence(&self, transaction_id, candidate) -> Result;
    fn activate(&self, transaction_id, candidate) -> Result;
    fn promote(&self, transaction_id, candidate) -> Result;
    fn owe_restore(&self, candidate, error) -> bool;
    fn abandon(&self, candidate);
    fn hold(&self);
    fn release(&self);
    fn unload(&self, deployment_id);
    fn active_deployment_id(&self) -> Result<Option<String>>;
    fn supervise(&self) -> Vec<MemberEvent>;
    fn member_failure(&self) -> Option<ErrorRecord>;
    fn shutdown(&self);
    fn stopping(&self) -> bool;
}
```

`fence` and `activate` are a promotion's two halves, on either side of the
transaction's commit write. `fence` refuses what a warm refuses before each
probe, a candidate that exited, whose control channel failed or whose
process cannot be inspected, as well as one that was given up, and moves
nothing; a refused fence fails the
transaction before its commit, and the serving worker keeps serving. Once
the candidate passes, the serving worker is told to stop admitting work
(`admission_fence` over its control channel) and must answer `ok` within 3
seconds, or the fence fails the transaction: `timeout` when it does not
answer, and `inference_failed`, naming the worker's answer, when it refuses
or its channel fails during the fence. A serving worker that exited or
whose control channel failed admits nothing and is not told. `activate`,
once the commit is durable, gives the candidate the serving slot, tells
the member it replaces to retire, sends the candidate `activate`, and
returns without waiting for the retired member's drain, which the
supervision step watches (see the registry's **Retire**); it is the only
call that mutates the worker's active deployment.
It is refused once the agent is stopping, or when the worker control no
longer holds the candidate; either refusal leaves the committed
transaction for the next start to settle forward. A candidate that exited
between the two takes the slot all the same, and the next supervision step
restarts it: after the commit the transaction does not fail. `promote`,
fence then activate, is for the start's restore, which has no commit to
write. `owe_restore` hands a restore whose worker did not come up to the
worker control's restarts (see
[Restart recovery](#restart-recovery-v01-e08-f07)); it is false by default,
so a worker control that restarts nothing ends the start.
`unload` is best-effort (failure is logged but never undoes a successful
promotion); the registry retires a serving member it unloads, as one a
promotion replaces, and stops a candidate. `abandon` tells the worker
control that the transaction which prepared a candidate returned without
promoting it; the coordinator calls it on every such return, a failed
state write included, and it does nothing by default. The registry then
sends `activate` to a serving worker that transaction fenced, so it admits
work again. `hold` asks the worker control to start no generation by
itself until `release`; a transaction that hands over a generation takes
the hold before it takes that generation and releases it when `prepare` has
returned (see [the member registry](#member-registry)). Every
implementation supplies both, so one that forwards `prepare` cannot leave
them out; the mock worker control's do nothing, since it starts no worker by
itself. `supervise`, `member_failure` and `shutdown` do nothing by
default; the [member registry](#member-registry) implements them to
restart a serving worker that stops, to report one it no longer restarts,
and to stop every worker when the agent is told to stop. `serving_member`
and `live_members` answer nothing by default; the registry answers them
with the generation serving now and its URL, in one reading, and with
[every worker process it holds](#what-status-reads-from-the-registry). `stopping` is
false by default and true once the registry's `shutdown` has begun; the
coordinator reads it to begin no transaction and record no failure from
then on. A registry whose lock is poisoned answers false: that is not a
stop, and the next call that needs the lock reports it as an internal
error.

The process-backed implementation is selected with
`worker.mode = "process"` and requires an absolute
`worker.serving_binary_path`. The agent writes per-candidate serving
configs under `worker.serving_config_dir` (default:
`<state_dir>/worker-configs`), starts the worker on loopback, and polls
`/health` until the candidate reports `ready`. The config's model block
carries the `runner_profile` the staged bundle's manifest names, when it
names one; the worker's sidecar launcher resolves it (see
[`backend-registry.md`](backend-registry.md#launching-a-runner-profiles-sidecar)).
Host CI and unit tests use
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

## Staged trees

Each deploy copies its bundle into a tree of its own,
`<staging_dir>/<deployment_id>/<transaction id>/`, named by the deploy's
transaction id (`tx-` and a UUID). The tree is created new: a deploy never
writes into a tree that is there, and a copy that fails removes its tree
and each directory it made above it, with an error that names the tree.
The candidate record names the tree once the copy is complete and synced
to disk: every file and directory of the tree, each directory above it
that this deploy made, and the nearest one that was there, which gained an
entry. A deploy under a `staging_dir` that is there therefore never opens
the directory `staging_dir` is in. A power loss after that state write
cannot leave a record naming a tree whose bytes never reached the disk,
with one exception: a directory that a deploy that was stopped made and
left unsynced is not synced by a later deploy that finds it there. An
id's directory left so is removed once it holds nothing (see below), when
the agent starts and before the next deploy stages, so that deploy makes
it again and syncs `staging_dir`. What remains is a `staging_dir`, or a
directory above it, that the agent made in a first deploy that was
stopped, or an id's directory a removal could not remove: a power loss
after a later deploy can lose its entry, and with it the trees under it
that the records name. The agent must be able to read `staging_dir`, and,
when a deploy makes `staging_dir`, the nearest directory above it that
was there.
A sync that fails fails the deploy as a copy that fails does. On macOS
each of these syncs, of every file and directory, is an `F_FULLFSYNC` (what
Rust's `File::sync_all` issues there), so a deploy's cost there differs
from Linux's, where `sync_all` is an `fsync`. Nothing writes the tree
again. Every reader takes the path
from a record's `staged_path`: the rollback, the restore at a start, the
registry's restart, `prepare`, the derived descriptor and status. An update
under the same id therefore leaves the tree the earlier record names as it
was: a rollback to that record starts the earlier bundle, and a start after
an update that did not complete restores the deployment that was serving.

A tree that nothing in either state file names (the staged path of
`active`, `previous_active`, `candidate`, and a resident set's members with
the generations they retain, and the bundle path of the transaction in
flight, since a deploy can take its bundle from a tree no record names) is
removed whole at two moments: when the agent starts, after the unnamed
[descriptor documents](#deployment-descriptor-documents) and before
recovery, and when
a deploy begins, before it copies its bundle (no other deploy stages while
its transaction is in flight). A removal that fails is printed on the
agent's standard error and neither stops the start nor fails the deploy.
Nothing is removed while what a state file holds cannot be told (it cannot
be read, is damaged, or is at a state version this agent does not read),
and only a directory named as a transaction id, under a directory named
as a deployment id, is ever removed; then that id's directory too, once it
holds nothing and no path named there resolves to it or goes up out of
it. A tree is matched by where its path resolves, and is kept when a path
named there resolves to it or into it, or goes up out of it with `..`, so
a `staging_dir` spelled another way than the records spell it (through a
link, or with `..`) removes nothing a record names; a named path that
does not resolve, other than because nothing is there, stops the removal
before anything goes. An entry that cannot be read or removed is
left, the removal goes on to the others in name order, and the first such
failure is printed. So the tree of a record a commit dropped from
`previous_active`, and the tree of a deploy that failed, stay until the
next deploy begins or the agent starts again. A deploy that failed after
its candidate was prepared leaves that candidate's process running until
the next deploy or rollback prepares a candidate, or the agent stops; the
abandoned candidate's tree can be removed before then, and nothing reads it
again. So can the tree of a member a commit retired while its process is
still draining: it has been told to retire and is not started again.

A `staging_dir` belongs to one state directory, because the agent removes
every tree its own state files do not name. Two agents with separate state
directories that share a `staging_dir` remove each other's trees, and an
agent started while its `state` directory is set aside removes every tree
the set-aside state names: an operator who sets `state` aside sets
`staging_dir` aside with it and puts both back together (see the
[rollback procedure](../install/lifecycle.md#downgrade-and-rollback)).

A singleton agent holds at most three trees, besides any it could not
remove: the one `active` names, the
one `previous_active` names, and either the candidate's while a deploy is in
flight or what the last transaction left. A same-id update holds two trees,
the active record's and the retained record's, until the retained record
goes. When the file system holding `staging_dir` is full, the copy fails:
the deploy fails before `staged` with an `internal` error that names the
tree and the system's reason, the partial tree is removed, and the active
deployment keeps serving; the quarantine entry and `last_error` carry the
error while the state directory has room to record it. When that write
fails too, the deploy returns the write's error instead, as any failed
deploy whose quarantine record cannot be written does.

Agents before this one staged every deploy of an id into
`<staging_dir>/<deployment_id>/` itself, overwriting in place. A record such
an agent wrote names that directory, and is started again from it; no
deploy writes its files again (a later deploy's tree is created inside it,
beside them), and the agent never removes it, nor a directory in it that
its `manifest.json` lists an artifact under. A later deploy's tree in it is
removed as any other once no record names it; while this agent cannot read
or parse that manifest, nothing in the directory is. Once neither `state.json` nor
`state.json.bak` in the state directory names it as a `staged_path`, an
operator can remove its files: `manifest.json` and the artifacts that
manifest lists, leaving the `tx-` directories inside it. A same-id update
made by such an agent left the directory holding the update's bundle: the
rollback to the earlier record and the restore of it are refused (see the
[staged bundle check](#member-registry)) until the id is deployed again.

## Rollback (V01-E08-F06)

Rollback is a transaction, not a file-pointer swap:

1. Read the previous active deployment from durable state.
2. Verify its staged files (`staged_path/` exists and contains
   `manifest.json`), then hash every artifact against that manifest, as the
   restore at a start does; a copy that fails is refused with the same
   code, before a transaction begins.
3. Walk the same prepare / warm / fence sequence as deploy.
4. On success, swap active <-> previous_active in durable state, in the
   write that records `promoted`, then activate the candidate and clear the
   transaction.

A failed rollback preserves the current active deployment — the
previous-active record is left intact, and the operator can retry.
A rollback to a record whose staged path holds another bundle than the
record names is refused, for a bundle of either format (see the
[staged bundle check](#member-registry)): a directory of the earlier layout
that an update under the id overwrote, or a damaged tree.

A previous record that names a
[deployment descriptor](#deployment-descriptor-documents) is started at a
new generation: just before step 3, under a hold on the worker control, the
rollback re-stamps the recorded descriptor and names the new one on its
transaction, and the candidate reference carries that identity. The swap
of step 4 gives it to the record it puts back as active, and refuses a
described record whose rollback transaction does not name a later
generation of the same configuration.

## Restart recovery (V01-E08-F07)

On process startup, before binding the local control socket, the agent
makes one pass over the durable state. It decides from the records alone;
the deployment a worker control reports serving is never read as a commit.

1. **Settle the transaction in flight**, in at most one state write.
   - Interrupted before its commit, in any phase: it is ended as failed.
     It is moved to the quarantine list with the phase it had reached, its
     candidate record is cleared, and the records stay as they were before
     it. This holds for a deploy and for a rollback.
   - At or after its commit: it is cleared, and the records it wrote
     stand.
   - Recorded `failed` and not cleared (a state an earlier release could
     leave): a rollback is cleared, and its failure becomes `last_error`.
     A deploy still holds its candidate record, so it is ended like one
     interrupted before its commit, with its own failure in the quarantine
     entry.
   - A candidate record with no deploy in flight (the same: this agent
     leaves none) is cleared, in the write that settles a rollback found
     beside it, or in a write of its own when no transaction is in flight.

   A deploy has committed when its phase is `promoted` or later and its
   candidate record is gone. A rollback has committed when its phase is
   `promoted` or later and `active` is the transaction's deployment at the
   generation the rollback named or a later one of the same configuration
   (a restart of the member names a later one), or, for a record that
   names no descriptor, when `active` is the transaction's deployment and
   bundle.
2. **Start the `active` record again**, in the same pass, unless the
   worker control already serves it (no start of the agent finds that: a
   new agent process holds no worker). `last_error` is cleared once it
   serves.

Nothing of a transaction is run again at a start: a deploy interrupted
before its candidate was prepared is not deployed again from its bundle
path, and a rollback is not rolled back again. No state the pass finds
needs a second start: an agent killed inside the pass leaves either the
state it found or a committed record with no transaction, and the next
start finishes from there.

The restore starts an active record that names a
[deployment descriptor](#deployment-descriptor-documents) at a new
generation, named on the record before a worker is asked for. What a
start does when the restore cannot be made:

| The restore finds | The start |
| --- | --- |
| an id that cannot name a member | comes up; `last_error` says why; the deployment is not serving |
| another bundle staged at the record's path than the record names, whether or not the record names a descriptor: a directory of the [earlier layout](#staged-trees) that an update under the id overwrote, or a damaged tree (see the [staged bundle check](#member-registry)) | the same |
| the staged directory gone, or a staged manifest that is missing, cannot be read, is incomplete or names no model artifact, or a staged artifact that is missing or altered | the same |
| a descriptor document that is missing, is not a regular file, does not verify, is another identity's or describes another bundle | the same |
| a re-stamp of the descriptor that is refused | the same |
| a state write of its own that fails, or a descriptor document it cannot write | ends with exit code `5` |
| a worker that cannot be started, exits while loading, is not ready within `worker.warm_timeout_ms`, exits or loses its control channel before it is promoted, or whose process cannot be inspected | comes up with the reason in `last_error`, and the [member registry](#member-registry) starts the record again as it restarts a serving member that stopped; `last_error`, whose `context` is whatever typed reason the failure carries or else `startup_restore`, is cleared once a restart serves the record, unless a deploy or rollback has begun since (no other error is cleared), and when the restarts are used up status reports the member's failure. A worker control that restarts nothing (`worker.mode = "mock"`) ends the start with exit code `5` |

The refusals in the first five rows are decided by the staged copy and
the descriptor document, so the agent stays up for `deploy` and
`rollback`, which both need its socket. A staged manifest that cannot be
read for another reason (its permissions) is refused the same way. The
start also prints each refusal on its standard error. Each refusal ends with "is not serving until
it is deployed again", except the first, which names the id rule.

Status reports what a start would do now as `recovery.action`:

- `restore_active` — the `active` record would be started again. The
  reason names a transaction the pass would settle first.
- `quarantine_candidate` — a transaction would be ended as failed and
  there is nothing to start.
- `no_op` — nothing to settle and nothing to start, or a committed
  transaction to clear beside a record that is serving.
- `operator_required` — no transaction and no `active` record, while the
  worker control serves a deployment. No start meets this, and the pass
  changes nothing.

`resume_verify`, `resume_stage`, `resume_prepare`, `resume_warm` and
`finalize_promotion` remain values of the control schema and are never
reported.

What an operator reads after a start that ended a transaction is the
quarantine entry: the transaction id, the deployment id, the phase it had
reached and the error. `last_error` carries the same error only while
nothing serves; it is cleared when the restore serves, so an agent that
ended a deploy and came back serving the old deployment reports `ready`
with one more entry in `quarantined`.

This holds when the deploy it ended was an update under the id that was
serving: the update staged a tree of its own, and the one `active` names is
as it was (see [Staged trees](#staged-trees)). It does not hold for a
record of the earlier layout whose directory an update under the id
overwrote: the start finds another bundle staged than `active` names,
starts nothing and comes up with that refusal as `last_error`, and the
deployment is not serving until the id is deployed again.

The agent records no process id of a worker. A worker that an earlier
agent process started is ended by the packaged service, not by the pass:
the systemd unit keeps the default control-group kill, and the Homebrew
launchd job does not set `AbandonProcessGroup`, so no worker outlives the
agent under either. Where an agent is run by hand and killed, the pass
does not end a worker it left behind: nothing records one. That worker
still listens on its port, and the agent does not check that a port it
leases is free (see [Ports](#member-registry)): the worker the restore
starts cannot bind it, as a worker's listener does not share its port; it
exits with a startup record, the start comes up with that failure as
`last_error`, and the registry's restarts meet the same port until they
are used up. `last_error` is that record's `unavailable`, recoverable,
with a message naming the address and the bind error, such as
`http server: could not bind 127.0.0.1:18080: Address already in use`.

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
`active`, `previous_active` and `candidate` slots are absent beside it). It
also lets a singleton record and the in-flight transaction name the
deployment descriptor of their generation by `generation`,
`descriptor_digest` and `configuration_digest`, all three or none; a deploy
of a format 0.2 bundle records them
([`integrity.md`](../bundles/integrity.md#naming-a-descriptor),
[descriptor documents](#deployment-descriptor-documents)). The
agent stamps the oldest state version whose readers decode the file without
loss: `0.2` from the first generation it allocates, `0.1` until then. A
deploy of a format 0.2 bundle takes a generation itself, so it writes a
`0.2` file under either worker mode; with `worker.mode = "mock"` a format
0.1 deploy takes none and the file stays `0.1`. A deployment record that
carries the [streaming](#streaming) flag is `0.2` content too, whatever
the generation counter says. An
agent through 0.2.x refuses a `0.2` file with its typed unsupported-version
error and exits instead of misreading it; nothing migrates a state file
backwards, and a downgrade sets the state directory aside instead. The
current agent refuses an unknown state version the same way, and a `0.2`
file with any field it does not know.

The durable state file is read only by the agent. The CLI (V01-E11) and
observability (V01-E10) read the status the agent projects from it, and the
deploy transaction phase names are load-bearing for both.

## Deployment descriptor documents

`agent/src/descriptor_store.rs` keeps the
[deployment descriptor](../bundles/integrity.md#deployment-descriptor-digests)
a record names as one file in the state directory, beside `state.json`:
`descriptor-<deployment_id>-<generation>.json`. A deployment id holds only
ASCII letters, digits, `-`, `_` and `.`, so the name is one path segment; a
generation holds no `-`, so the last one ends the id. Nothing is created
before the first document is written.

- **Write.** Inside the state update that names the descriptor, after the
  update has set the identity in the next state and before the store writes
  that state: a `.tmp` file, `fsync`, `rename(2)`, then a sync of the
  directory. A document that cannot be written fails the update, and the
  failed write removes its `.tmp`: nothing is named. The update of a
  deploy, a rollback or a restore, which takes the generation in that same
  update, then spends none; a restart's generation was allocated by the
  registry in its earlier state write, so a restart whose stamp cannot
  write the document has spent one. A crash between the two writes leaves
  a document no record names.
- **Deploy.** `start_deploy` builds the candidate reference a deploy hands
  the worker control. For a format 0.2 bundle it parses the staged copy
  with every artifact digest verified, requires its manifest digest to be
  the candidate record's (`load_failed` otherwise), and then, in one state
  update taken under a hold on the worker control, requires the candidate
  record and the deploy's transaction to stand as the deploy wrote them,
  takes the next generation, derives the descriptor from the staged copy,
  names it on both and writes its document. The descriptor states the
  runtime version the agent runs (its build's; the configuration's
  `runtime_version`, an override the compatibility check reads, is not
  stated), the runner profile as the backend's probe report found it
  installed,
  admission mode `production` and the default quota: no quota is assigned
  to a member yet, the value is not a limit, and every configuration digest
  changes when one is first assigned. A descriptor that cannot be derived
  refuses the deploy at `capacity_checked`, with `unsupported` for a runner
  profile that is not installed or does not list the manifest's compute
  type and `config_invalid` otherwise. On a deployed agent the bundle
  verifier has refused those two before staging, since every available
  backend that has a descriptor has a probe report; a coordinator built
  without a report for the bundle's backend refuses a named runner profile
  here. A `staging_dir` that is absolute and not normalized (a `.` or `..`
  segment) is `config_invalid`, naming `staged_path`; a format 0.1 deploy
  is not held to that. A refusal names nothing and takes no generation. A
  kill after the document and before the state write leaves a document no
  record names and the counter where it stood: the next start removes the
  document and ends the deploy as failed. A kill after the state write
  leaves the generation named on the transaction: the next start ends the
  deploy as failed, the generation is not handed out again, and the start
  after that removes its document. A bundle
  of another format gets the reference it got before; nothing is read,
  held or written for it.
- **Read.** By deployment id and the identity a record or candidate
  reference carries. The document must be a regular file of at most 1 MiB
  of UTF-8 (a directory, a FIFO or a symlink at its name is refused without
  being opened, since the read can run inside a state update), verify, and
  be that deployment's descriptor at that generation with those digests;
  anything else, a missing document or one the agent cannot read
  included, is refused with `load_failed`, not recoverable, naming the
  file and, for a read that failed, the operating system's error. A
  candidate reference must also state the descriptor's bundle digest,
  staged path, model class and backend and, where it carries them, its
  model artifact's path, bundle name and bundle version, or the candidate
  is refused the same way, naming the field.
- **Prune.** `DescriptorStore::prune` removes every document that no
  singleton record, in-flight transaction or resident-set member (with the
  generation it retains) names, and every `.tmp` a crash left; no other
  file, and nothing when a record's identity cannot be read. The agent
  prunes once when it starts (`prune_at_start`), after it has opened its
  state and before recovery or a restart can write a document: later it
  would race the write of a document not named yet. It keeps what either
  file of the state pair names: a state write that stopped between its two
  renames leaves the backup naming a document the primary does not, and a
  later start reads the backup once the primary is damaged. A state file
  that cannot be read, or is at a state version this agent does not read,
  leaves every document in place; a damaged one names nothing. A prune
  that fails is logged and does not stop the start, because a document
  nothing names is never read and a later write of its generation replaces
  it. Between starts these stay: the document a re-stamp replaced, the
  finished document of an update whose state write then failed, the
  document of a candidate whose deploy failed, that of a rollback whose
  worker did not come up, and the document of a record a later promotion
  pushed out of `previous_active`. That is at most one for each generation
  started.
- **Rollback and restore.** `start_again` builds the candidate reference a
  rollback, or recovery's restore, hands the worker control from a
  deployment record. It compares the staged manifest's digest with the
  record's, for every record. For a record that names a descriptor it then,
  in one state update, requires
  the record to stand where it was read (`previous_active` for a rollback,
  `active` for a restore), takes the next generation, reads the recorded
  document, requires it to describe the record's bundle, re-stamps it,
  names the new identity (on the in-flight rollback transaction, or on
  `active`) and writes the new document. The reference carries that
  identity, so the registry starts the member as that generation. A refusal
  names nothing and spends no generation: the rollback's is a failed
  rollback that leaves the previous record as it was, and the restore's
  leaves the agent up and the deployment not serving (see
  [Restart recovery](#restart-recovery-v01-e08-f07)). A kill after the document and
  before the state write leaves a document no record names; the next start
  removes it and hands out the same generation again. A kill after the
  state write leaves the generation named: the next start restores `active`
  from it, or ends the rollback as it ends any transaction interrupted
  before its commit, the previous record still naming its own document
  (the generation the rollback named is not handed out again, and its
  document goes at the start after). The descriptor is re-stamped as recorded, as by
  the restart stamp. The update runs under a hold on the worker control,
  which `prepare` ends, so no restart of the serving member takes a newer
  generation in between (see [the member registry](#member-registry)). A
  record that names no descriptor gets the reference it got before, and
  nothing is held or written.
- **Restart stamp.** For a member whose recipe names a descriptor, the stamp
  a `process`-mode agent installs reads the stopped generation's document,
  [re-stamps](../bundles/integrity.md#re-stamping) it, writes the new
  document and names it on `active` in one state update, and returns the
  recipe naming it. It refuses, as a launch that failed, unless `active`
  names the recipe's descriptor, the restart replaces that descriptor's
  generation, and the descriptor describes the member's bundle, whose
  digest the staged bundle check has just compared with the staged
  manifest's. A descriptor is re-stamped as recorded: after a package
  upgrade it still states the deploy's runtime version and runner
  environment. A recipe that names no descriptor is returned unchanged.

A record names a descriptor exactly when its bundle is format 0.2 and was
deployed by an agent that derives one. For every other record (a format 0.1
bundle, or a state file an earlier agent wrote) no document is read or
written, and deploy, rollback and restart behave as they did before
descriptors, except that the restore and the rollback compare the staged
bundle with the record's for every record (see the
[staged bundle check](#member-registry)).

## Worker runtime control transport

The reusable `ControlChannel` owns a socket, bounded command queue and dedicated
ledger-polling thread for one member generation. Its typed contact transitions
are available through a bounded snapshot; process recovery is a registry
responsibility. `spawn_with_control` supplies the corresponding safe stdin socket
handoff. The [member registry](#member-registry) is their production caller. See
[runtime control client](worker-supervision.md#runtime-control-client)
for framing, deadlines, ownership and loss-of-contact behavior.

## Member registry

`agent/src/registry.rs` is the one owner of the serving worker processes a
`process`-mode agent starts. Each worker is a member generation:

- **Generation.** Before a worker starts, the registry takes the next
  generation from the durable state's counter in a committed write, so a
  generation is never used twice, including across agent restarts and by a
  start that then fails. A restart takes one too, in a write of its own before
  its launch. The first allocation moves the state file to state
  version `0.2` (see [durable state](#durable-state-store-v01-e08-f02)); startup
  recovery relaunches the recorded active deployment through the same path,
  so it happens on the first start that has a deployment to restore. A
  candidate reference that names a
  [descriptor](#deployment-descriptor-documents), a deploy's, a rollback's
  or a restore's, hands over its generation instead: the launch is that
  generation and takes none from the counter. A handed generation must be
  above every generation the registry holds, its members', those of workers
  it has asked to stop or told to retire and not yet reaped, and the one a
  restart is owed for. One taken under a hold always is; a reference that
  is not, which only a generation handed twice can be, is refused with
  `internal`, recoverable: another attempt takes a new generation. A
  restart always takes a new one, whatever its recipe names.
- **Start.** The registry renders the serving config with
  `deployment.generation` and `deployment.endpoint` set to the deployment id,
  hands the worker its control socket on fd 0 in the same launch, and starts
  the 1 Hz ledger poll at once, before it waits for the model to load. The
  config file is named for the deployment and generation and is removed with
  the worker; files an earlier agent process left are removed at start. A
  deployment that asked for [streaming](#streaming) also gets its
  `streaming` block and `deployment.descriptor_digest`. The
  runner profile in the config is the staged manifest's, read by `prepare`
  and before a restart. For a candidate that names a descriptor it is the
  descriptor's instead: `prepare` does not read the staged manifest, and
  the document is read, once per launch, when the config is rendered.
- **Readiness.** A candidate is warm when `/health` reports it ready for the
  deployment and its control channel has delivered a ledger. A candidate
  that exits while loading answers with its startup failure record; one
  that answers no ledger poll for 10 seconds has a failed control channel
  and is killed and refused, whatever the warm timeout, and so is one whose
  process can no longer be inspected (its wait fails).
- **Stop.** A candidate being displaced or unloaded, a member whose retire
  is not acknowledged, and every member when the agent stops, is sent
  SIGTERM and is killed if it has not exited 10 seconds later; members
  stopped together share that deadline. Its serving config tells it to
  drain its sessions for 5 seconds (`shutdown.drain_deadline_ms`), so its
  own drain ends before the registry would kill it. The signal goes to the
  worker's process id only: a worker that has to be killed leaves its
  sidecar to exit when it sees the worker's socket close or, while a
  backend call holds it, once it sees its parent change: within about
  two seconds of the kill, unless the call holds the interpreter lock
  (status 4; see
  [the speech dispatch](serving-worker.md#the-speech-dispatch-in-the-worker)).
  Every member is stopped this way when the agent itself is told to stop.
- **Retire.** A serving member a promotion replaces, or one an unload
  stops, leaves the serving slot under the registry's lock and is held as
  retiring: listed as draining and holding its ports until it is reaped.
  Its `retire`, with a 30-second drain (`drain_timeout_ms` 30000) and the
  committing transaction's id (an unload's carries an id of its own), is
  queued on its control channel after the commit and before the
  candidate is sent `activate`; the answer is waited for on a thread of
  its own, so the candidate's `activate` does not wait for it. The
  retire takes the member's fence record, so an `activate` a failed
  transaction still owed it, or a restart's, is not sent after the
  retire. A worker that
  answers `ok` closes its admission for good, drains its sessions and
  exits by itself. The supervision step reaps a retiring member once its
  process has exited; one still running 40 seconds after it acknowledged
  (the drain, then 10 seconds to stop its sidecar and exit) is sent
  SIGTERM, and one still running 10 seconds after that is killed. One that
  does not answer `ok` within 3 seconds is sent SIGTERM at once and killed
  10 seconds later, as any member that is stopped; one whose answer is
  still not recorded 13 seconds after the retire was queued (the thread
  that waits for it did not run) is sent SIGTERM then. A `prepare` that
  finds every port held asks the retiring member retired first to stop
  (see Ports below). A displaced or abandoned candidate and a member
  taken out of service are never sent `retire`.
- **Promotion.** A candidate that exited after it was warmed is refused
  by the fence, before the commit write: the deploy fails with the
  worker's failure and the serving member stays. One whose control
  channel failed since its warm, or whose process can no longer be
  inspected, is killed and refused the same way. A serving member that
  does not answer its fence `ok` fails the deploy the same way and is
  activated again. Once
  the commit is written the candidate takes the serving slot even if it
  exited since, and the supervision step restarts it; the retire of the
  member it replaces is queued only after the commit, before the
  candidate is sent `activate`.
- **Supervision.** The agent's loop runs the coordinator's turn
  (`Coordinator::supervise`) every 100 ms and logs what it did: the
  registry's supervision step (`WorkerControl::supervise`), then, once a
  member serves the active record, the clearing of a restore's
  `last_error`. The step
  takes the serving generation out of service when its process has exited,
  when its control channel has failed (10 seconds without an answered
  ledger poll), or, for a generation started by a restart, when it is not
  ready within `worker.warm_timeout_ms`; in the last two cases the worker
  is killed. Nothing else stops a serving member: a slow `/health` answer
  or slow inference does not, and `/health` is not polled once a
  generation is ready.
- **Admission.** The registry tells workers over their control channels
  when to admit work. A transaction fences the serving worker after its
  candidate passes the fence's checks and before the commit write; the
  fence is recorded before it is sent, and a transaction that ends without
  promoting its candidate sends `activate` to the worker it fenced, with
  its own transaction id, so it admits again. One that does not answer
  that `ok` stays fenced in the registry, and the supervision step sends
  it the same activate again, from a thread of its own and at most once a
  `worker.status_poll_interval_ms`, until it answers `ok`, it leaves
  service, or another transaction fences it; the agent logs when it begins
  sending it again and when it is answered. A worker that answers it
  `unsupported` has no admission control, so it never closed its admission
  and is not sent it again. After the commit the new
  serving worker is sent `activate`. A restarted generation is sent
  `activate`, with a transaction id of its own, once the supervision step
  judges it ready; it is sent from a thread of its own, so the step does
  not wait for the answer, and not at all while a transaction has fenced
  that generation or once it was told to retire. An `activate` waits at
  most 3 seconds for its answer; one after a commit or a restart not
  answered `ok` is logged and fails nothing. The quota an `ok` carries is
  the member's `effective_quota` in status; an `activate` answered
  `error` clears it, and one not answered, or refused otherwise, leaves
  it. Operations to one worker are sent one at a time, and a restart's
  activate is not sent while a transaction's fence is in force; a fence
  that finds an activate being sent to its worker waits for it, at most
  3 seconds, before its own 3 seconds begin. Each is exchanged in the gap
  before the channel's next ledger poll, so none delays a poll; one not
  answered when its gap ends is sent again after the poll, as a new
  request with its own `correlation_id`.
- **Restart.** The successor is a new generation: a new generation number
  and serving config, on the same port, from the member's staged root. It
  starts 1, 2 and 4 seconds after the stop for the first, second and third
  restart inside a sliding five-minute window; a restart that cannot be
  launched counts as one. From the stop until the new generation is ready,
  by the candidate's rule, status carries no `active.serving_url`.
  A start's restore whose worker does not come up is owed a restart the
  same way (`WorkerControl::owe_restore`), on the ports of the restore's
  worker, which is killed, or on free ones, and the agent stays up. None
  is owed while the agent is stopping, a member serves, a deploy holds the
  member or a restart is already owed.
- **Staged bundle check.** Each deploy stages a tree of its own (see
  [Staged trees](#staged-trees)), so the tree a record names holds that
  record's bundle unless it is a directory of the earlier layout that a
  later deploy under the id overwrote, or it was damaged. Before a restart
  the registry reads the staged manifest and compares its digest with the
  bundle digest the member was started under. When they differ, nothing is
  started and the member is failed at once, without using a restart, with a
  `load_failed` error that names both digests. A staged manifest that cannot
  be read counts as a restart that could not be launched. The manifest's
  digest covers each artifact's digest; artifact bytes are not hashed again,
  since the tree was synced before a record named it and the restore hashes
  them at every start.
  Startup recovery and a rollback compare the two digests as well, for
  every record, whether or not it names a
  [descriptor](#deployment-descriptor-documents). A staged bundle that is
  not the record's is `load_failed` for both. The restore compares them
  before it reads anything else of the staged copy, so the state of the
  staged artifacts does not change its answer. The rollback is refused. The
  restore starts nothing, and the agent still comes up with its socket open:
  status reports `agent_state: degraded`, the refusal as `last_error` (it
  names both digests and says that the deployment is not serving until it is
  deployed again), and `active` as the record with no `generation` and no
  `serving_url`. To recover, deploy the id again through that agent, as
  either bundle: the deploy serves it and clears the error, and the next
  start restores it. The restore, once the two digests agree, and the
  rollback also hash every artifact of the staged copy: one that is missing,
  truncated or replaced is `load_failed` at the restore, with the same
  outcome, and at the rollback, which is refused before it begins a
  transaction.
- **Failure.** A member that stops with three restarts inside the last five
  minutes is failed: nothing is started for it until a deployment is
  promoted, by a deploy or a rollback. Status then reports
  `agent_state: degraded` and the failure as `last_error`, ahead of the
  last transaction's error, and `active` has no `serving_url`.

A failed member is held in the agent's memory only; neither it nor its
error is written to the state file, and status reads it from the registry.
An agent restart therefore starts the recorded deployment again with all
three restarts available; until then status reports the member as failed.

A restart never starts while a deploy holds the member: from the moment
`prepare` begins until its candidate is promoted or fails, its `warm`
gives up, or the transaction returns without promoting it for any other
reason, such as a state write that failed. A promoted generation the
registry allocated is therefore never older than the one it replaces. A
transaction that hands a generation over takes it before `prepare`, so it
holds the member earlier: `WorkerControl::hold`, taken before the state
update that takes the generation and released when `prepare` has returned,
counts as a `prepare` that is running. A restart takes its generation
first and checks for a hold after. One that finds the hold starts and
stamps nothing, and stays owed. One that passed the check before the hold
took its generation before the transaction did: it may still stamp
`active` and start its worker, and it yields to the transaction when its
launch returns. Either way every generation the registry holds when the
handed one is launched is older, and after the promotion or the swap
`previous_active` names an older generation than `active`. A promotion
drops the restart that was owed and gives the member its three restarts
back. A candidate whose `warm` gave up is left
where it was, as before, but is not promoted afterwards.

A member's name is its deployment id, and the serving worker accepts only 1
to 128 of ASCII letters, digits, `-`, `_` and `.`. Deploys have been held to
that since 0.2.1; a recorded active deployment from an earlier release whose
id breaks it is not restored at start. The agent comes up without a worker
and with the reason in `last_error`.

**Ports.** Every member generation holds ports from the agent's pool, from
its start until its process has been reaped; a member that is draining
after a promotion or a yield still holds them, and so does the restart owed
to a member that stopped, whose successor starts on the same ports. A new
candidate is given the first ports nothing holds. When none is free its
`prepare` asks the retiring member retired first, the lowest generation, to
stop: SIGTERM, which cuts the worker's drain to at most its 5-second
shutdown drain, and a kill at the 10-second stop deadline. It asks none while
another member is already on its way out (asked to stop, exited or being
reaped), and only one at a time, so a drain is cut short only when no port
is free. It waits, up to `worker.prepare_timeout_ms`, for ports to be given
back, and then fails with `resource_exhausted` before anything is started.
A failed or unloaded member gives its ports back. The agent does not check
that a port it leases is free on the host: when another process listens on
one, the worker given it does not start, so every deploy onto it fails with
the worker's startup error, `unavailable` naming the address, and a
restart, which keeps its ports, fails until its three restarts are used up
and the member is failed.

The pool is `worker.serving_port_range` when the configuration has one:
pairs taken in order from `first`, an HTTP port and the port after it, kept
for the member's stream listener (an odd port left over is not used; the
range must hold at least two pairs). Without a range, which is how every
packaged configuration ships, the pool is the two ports
`serving_bind_port` and `serving_candidate_bind_port`, one worker each, and
no stream port is held. A worker's stream port is rendered into its config
only for a deployment that asked for [streaming](#streaming); for any
other the range reserves the stream port and nothing binds it. A
generation that is to stream and holds no stream port, as on a pool
without a range, is not started: `prepare` or the restart refuses it with
`config_invalid`.

A restart can be stamped: a registry built with a restart stamp calls it
for each generation a restart starts, after the staged bundle check and
before the config is rendered, with what the stopped generation was started
from, the generation that was last stamped or started, and the new one. The
new generation is started from what the stamp returns, and so is the
restart after it; a stamp that fails is a restart that could not be
launched and counts as one. A `process`-mode agent installs the stamp
described under [descriptor documents](#deployment-descriptor-documents); a
restart in a registry without one is unchanged. The registry writes no
resident set.

### What status reads from the registry

The registry answers for every worker process it holds: the serving member,
the candidate a deploy has prepared (listed as abandoned once that deploy
ended without promoting it, and running until the next deploy displaces
it), and each generation it has asked to stop or told to retire and not
yet reaped. A generation that left the registry's map to be stopped is
recorded as draining under the same lock that took it out, and the record
is removed once its process has been reaped. A worker the registry is
still starting is not listed until its launch has returned, and
one it kills, for lost contact or for not becoming ready after a restart,
is not listed while it is killed. For each one listed the registry reports
its deployment id and generation, whether its process has been seen to
exit, whether the agent judged it ready, whether its control channel is in
contact, the ports it holds, and the quota its worker answered its last
`activate` with (none for a draining one).

- **Running** means the process has not been seen to exit. A process the
  agent cannot inspect has not been, and is reported as running with the
  readiness it had and the contact its channel reports now: supervision
  treats it the same way, and keeps a serving member in service until its
  control channel has gone ten seconds without an answer.
- **Ready** is the agent's last judgement, not a reading taken for the
  status: `warm` records it for a candidate it found ready, and the
  supervision step records it for a restarted generation. A serving worker
  is not probed again afterwards.
- **In contact** is the control channel's own reading: contact is lost once
  three polls in a row went unanswered. Lost contact alone does not take a
  member out of service; ten seconds without an answer does.
- **Draining** generations are reported as running until they are reaped,
  with the readiness they had; their control channel is not read for
  status.

Agent status uses this in two places. Beside a singleton record,
`active.generation` is the generation serving at `active.serving_url`, read
together with the URL. The serving member is read only when there is an
active record; a serving process that cannot be inspected fails that
reading, and the status with it. The generation is attached only when the
member serving is the recorded active deployment: the same id, started from
the same bundle and, when the record names a descriptor, from the
descriptor whose descriptor and configuration digests the record carries. A
transaction writes the record that names its worker before that worker is
activated, so between the commit and the activation the member serving is
the outgoing one; `active` then carries the committed deployment with the
outgoing worker's URL and no generation. From the activation on, the
record and the member serving agree, through the outgoing worker's stop
too. A record that names no descriptor, which is every record of a format
0.1 bundle, cannot tell a redeploy of its own bundle under its own id from
the deployment it records: between the commit and the activation `active`
then carries the outgoing worker's generation and URL beside the new
promotion time. Both are absent
while nothing serves: the worker stopped, a restart is waiting or loading,
or the member is failed. `candidate.generation` is
the generation a deploy has prepared and not promoted; a worker an earlier
deploy abandoned is not named. `active.stream_endpoint` and
`active.descriptor_digest` are attached with `active.generation`, and only
when that generation was started with [streaming](#streaming); in the
window above where the outgoing worker's generation is attached, a
redeploy of a format 0.1 bundle under its own id, the outgoing worker's
endpoint and digest are too, if it streams.

A singleton record that names a descriptor carries its generation: a
deploy, a rollback, a restore and the
[restart stamp](#deployment-descriptor-documents) each write the generation
they start into the record. `active.generation` is still read from the
registry, and only while the member serving is the one the record names. A
restart, a restore, a deploy and a rollback write the record before the
new generation serves, so the field is absent in between and is the
record's generation once both are true.
`previous_active.generation` is the retained record's own, the generation
that deployment was last started as, since nothing runs for a retained
deployment; it is absent for a record that names no descriptor.

For a resident set, each member is matched to the registry by deployment id
and generation. A committed generation the registry holds no running
process for reads `process: stopped`, `readiness: not_ready`, with no
`contact`: it exited, it was stopped, or it was never started. After a
restart the registry runs a later generation than the committed one, so the
committed generation stays `stopped` until a record names the generation
that runs.

`draining` describes the committed generation only, and the committed
generation is the one the record names, whichever serves. A committed
generation that was promoted over reads `process: running` and
`draining: true`, with the readiness it had and no `contact`, until it is
reaped; from then on it reads `stopped`. A retired worker is reaped once it
exits after its drain, at most 50 seconds after it acknowledged the retire
(the 30-second drain, 10 seconds of cleanup and the stop deadline), or
within the stop deadline when it did not acknowledge it. Its successor
serves from the promotion on and is not in status until a record names
it. A generation that drains and is not a committed member, as the one a
singleton deploy replaced, is not listed: it is visible on the host as a
worker process that still runs, and through the ports it still holds,
since a deploy that needs them waits for it to be reaped.

Not done yet: `tensorplate status` severity and `tensorplate doctor` do not
judge `process` or `readiness`. They judge `contact`, which the agent now
fills: a member in `serving` state reported `out_of_contact` lowers
severity to `degraded` and is named by `doctor`. A member whose process is
`stopped` or draining reports no `contact`, so it is still counted as
healthy; the message of `doctor`'s `ok` finding counts such members apart
from those in contact and from those the agent reports nothing for.

Beside a resident set the singleton slots are projections of the record. A
set of one carries its committed generations into `active.generation` and
`previous_active.generation`, and its committed unary endpoint into
`active.serving_url`. They say what is committed, not what serves: after a
promotion the registry serves a later generation on another port, and the
projected slot does not move. No production path writes a resident set yet,
so the member fields and these projections are reached only through a state
that already holds one.

`WorkerSupervisor` is a separate process manager that no packaged
configuration enables; a configuration that enables `supervision` beside
`worker.mode: process` is refused at start, since each would start its own
worker. The registry reuses that module's clock trait and nothing else: its
backoff scheduler does not time these restarts.

The agent installs a handler for SIGTERM and SIGINT after startup recovery,
just before its listener opens, so the handler is in place by the time the
agent logs that it is listening. Before that a signal ends the agent at
once. The handler only stores a flag, so a second signal during the stop
does nothing: it neither hurries the stop nor ends the agent. The agent's loop
then stops the registry, which sets its stopping flag before it stops the
first member: every worker, serving, candidate and retiring, is sent
SIGTERM, all share the one 10-second stop deadline, and what is left is
killed and reaped. A worker's SIGTERM ends a retire drain it is in early.
From the flag on no launch begins. Requests are answered on threads
that are not joined, so the registry also waits for a worker one of them
was starting or stopping when the flag was set: that thread stops it,
within a stop deadline counted from when it began to. That wait ends 12
seconds after the flag (the stop deadline and 2 seconds more) whether or
not those threads are done, and the registry logs how many were not. The
case it is there for is a launch held in the state write that allocates its
generation; when such a write returns, the launch starts nothing. Otherwise,
when the registry's stop returns, no worker process is running and none can
be started. The listener stops and the agent exits 0.

A deploy or rollback in flight at that moment is not recorded as failed.
Its caller is answered with status `error` and the error code
`unavailable` (message `the agent is stopping`), if the answer is sent
before the agent exits. The status is not `unavailable`: that status is
the refusal of an operation that cannot apply, carries the code
`unsupported`, and makes the CLI exit 6; this answer is a retryable
error, and the CLI exits 3 with the code `unavailable`. The transaction stays in the state file at the last phase
it wrote, exactly as an agent that was killed leaves it, and the next
start's recovery treats it as it treats that:

- Interrupted before its commit, in any phase, the transaction is ended
  as failed with a quarantine entry and none of it is run again: the
  caller that was answered the `unavailable` error deploys or rolls back
  again.
- The deployment that was serving is started again in that same start, so
  after `systemctl restart tensorplate-agent` during a deploy's warm the
  node serves what it served before the deploy.
- Stopped after its commit write, before its candidate took the serving
  slot, the transaction is at `promoted`: the start clears it and starts
  the deployment it committed, although its caller was answered the
  `unavailable` error.

A failure the deploy met just before the stop began is not recorded
either. A deploy or rollback that arrives after the flag is refused with
the same `unavailable` error before it writes anything; one that passes that check as
the flag is set is left in flight like the others.

The longest the agent takes to stop its workers is 23 seconds: a
supervision step that was stopping a successor which yielded to a deploy
(the stop deadline) and reading a stopped worker's last stderr (1 second),
then the registry's stop (12 seconds). The packaged systemd unit keeps the
default control-group kill and `TimeoutStopSec=30`, which a test holds
above that bound.
Under systemd the workers therefore receive the service manager's SIGTERM
as well as the registry's, which is harmless: a worker ignores a second
request while it drains. Whatever remains in the unit's control group after
the stop timeout, sidecars included, is killed by the service manager.

The Homebrew launchd job sets no `ExitTimeOut`, so launchd's own default
bounds the stop there. Apple documents that default as system-defined (20
seconds in earlier editions of `launchd.plist(5)`), which can be shorter
than the bound above; an agent launchd kills before its stop ends leaves
what any killed agent leaves, and the next start's recovery reads it the
same way. The job does not set `AbandonProcessGroup` either, so launchd
kills what is left of the agent's process group when the agent exits.
