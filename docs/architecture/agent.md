# `tensorplate-agent` (V01-E08)

`tensorplate-agent` is the Rust management-plane service that turns a
running `tensorplate-serving` process into a device appliance. It owns the
durable desired-state store, the deploy transaction state machine, bundle
verification, rollback to the previous active deployment, and the
prepare/warm/promote handoff with the V01-E07 serving worker.

This document records the V01-E08 architecture decisions. It is the
single source of truth for the cross-component contracts the CLI
(V01-E11), the observability service (V01-E10), and the package layout
(V01-E14) build on top of.

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
- Restrictive socket permissions (`0o600`, owner-only) match the trust
  model: only operators on the device may mutate state.
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

Supported ops: `deploy`, `status`, `rollback`, `health`, `version`.

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

## Durable state store (V01-E08-F02)

The store persists exactly two files in the agent's state directory:

- `state.json` — the current, latest-committed state.
- `state.json.bak` — a snapshot refreshed after every successful primary
  write. Consulted only when `state.json` fails to decode.

Every mutation:

1. Walks an in-memory clone of the current state through a closure.
2. Bumps `store_version`.
3. Writes the new state to `state.json.tmp` and `fsync`s it.
4. `rename(2)`s the tmp file over `state.json` (atomic on POSIX).
5. Best-effort directory `fsync` so the rename survives power loss.
6. Refreshes `state.json.bak` from the just-committed primary.

The store never persists model bytes, request payloads, or unbounded
logs — only digests, paths, and bounded error metadata. The
quarantine list is capped at 32 entries; oldest entries are dropped on
overflow.

## Bundle verifier (V01-E08-F03)

`bundle::verify` is the single deploy-time gate. It checks, in order:

1. The bundle path exists and is a directory.
2. `manifest.json` decodes against `protocol/schemas/bundle_manifest.json`.
3. Each declared artifact's `sha256` digest matches its content.
4. (Optional) the manifest's `manifest_digest` field matches the canonical
   manifest with that field stripped.
5. `format_version` major matches the runtime's supported major.
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

Every payload carries `schema_version` const-fixed to `0.1`. The agent
rejects unknown schema versions on the control API with the typed
`Unsupported` error code. The on-disk state file follows the same rule;
older versions are migrated forward by an explicit migration step when
one ships (v0.1 ships none).

The durable state schema and the deploy transaction phase names are
load-bearing for the CLI (V01-E11) and observability (V01-E10). They
will not change without a coordinated schema bump.
