# `cli/`

`tensorplate-cli` — the Rust operator command-line interface. Single-device
operator client for v0.1.0; targets exactly one reachable `tensorplate-agent`
endpoint at a time.

## Ownership

- **Layer:** management plane (operator client)
- **Language:** Rust
- **Cargo crate:** `tensorplate-cli` (library + `tensorplate` binary)

## Scope (v0.1.0, V01-E11)

- `tensorplate doctor` — read-only device, runtime, and dependency checks.
- `tensorplate deploy <bundle>` — submits bundles through the agent
  deploy-transaction API.
- `tensorplate status` — renders active deployment, worker supervision, and
  observability state.
- `tensorplate infer` — convenience single-shot inference against the active
  deployment.
- `tensorplate logs` — bounded NDJSON reader for local structured logs.
- `tensorplate rollback` — restores the previous active deployment via the
  agent.
- `tensorplate version` — CLI / protocol / bundle format versions.

## Dependency direction

```
cli/  ──(local control API)──>  agent/
cli/  ──(http /infer envelope)──>  serving_worker/   (data-plane convenience only)
cli/  ──(reads atomic snapshot)──>  observability/   (status command, optional)
```

The CLI never mutates the serving worker directly. All state-changing
operations route through the agent control API. Device access profiles
(`local`, `url`, plus reserved `ssh-tunnel`/`overlay`/`relay`) are loaded
from the CLI config; v0.1.0 implements `local` and explicit URL targeting
only.

## Rules

- No cloud-backed auth, device registry, or fleet inventory in OSS v0.1.0.
- CLI talks to one agent at a time.
- Mutating operations go through the agent API; the CLI never edits
  desired-state files, bundle staging directories, worker configs, or worker
  process state directly.
- Reserved profile modes return typed `Unsupported` rather than silently
  falling back to `local`.

## Documentation

See [`docs/cli/`](../docs/cli/README.md) for command reference, the
[exit-code table](../docs/cli/exit-codes.md), and the device access
[profile guide](../docs/cli/profiles.md).
