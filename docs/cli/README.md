# `tensorplate-cli`

Operator command-line interface for one reachable `tensorplate-agent`. Lands in
**V01-E11** as the single-device operator client.

## Scope (v0.1.0)

| Command | What it does |
| --- | --- |
| `tensorplate doctor` | Read-only device, agent, and dependency checks (V01-E11-F03). |
| `tensorplate deploy <bundle>` | Submits a bundle to the agent deploy transaction API (V01-E11-F04). |
| `tensorplate status` | Renders agent, worker supervision, and observability state (V01-E11-F05). |
| `tensorplate infer` | Sends a single inference request to the active deployment (V01-E11-F06). |
| `tensorplate logs` | Reads bounded NDJSON log entries from a local source (V01-E11-F07). |
| `tensorplate rollback` | Restores the previous active deployment via the agent (V01-E11-F04). |
| `tensorplate version` | Prints CLI, protocol, and bundle-format versions. |

The CLI is **a client**. Every mutating operation goes through the
`tensorplate-agent` control API; the CLI never edits desired-state files,
launches/restarts workers, or mutates serving-worker process state directly.

## Device access profiles

Profiles live in the CLI config (default: `$TENSORPLATE_CLI_CONFIG`, or pass
`--config <path>`). Schema: [`config/schemas/cli.json`](../../config/schemas/cli.json).

| Mode | Status | Reaches |
| --- | --- | --- |
| `local` | implemented | Local agent Unix domain socket. |
| `url` | implemented | Explicit `host:port` over loopback TCP, intended for SSH/VPN/overlay-tunneled workflows. |
| `ssh_tunnel`, `overlay`, `relay` | reserved | Parse but fail with a typed `Unsupported` error at command execution. |

Default profile, if no config file is present, is `local` against
`/var/run/tensorplate/agent.sock`. `--agent-url <host:port>` always wins over
`--profile`.

## JSON output envelope

Every subcommand supports `--output json`. Envelope schema:
[`protocol/schemas/cli_output.json`](../../protocol/schemas/cli_output.json).

```jsonc
{
  "schema_version": "0.1",
  "command": "status",
  "status": "ok",          // ok | error | busy | unavailable | not_found
  "correlation_id": "cli-…",
  "transaction_id": null,
  "payload": { /* command-specific */ },
  "error": null
}
```

## Exit codes

See [`exit-codes.md`](./exit-codes.md). The numeric values are part of the v0.1.0
CLI contract — V01-E15 validation scripts assert on them.

## Command details

- [`doctor`](./doctor.md)
- [`deploy` and `rollback`](./deploy-rollback.md)
- [`status`](./status.md)
- [`infer`](./infer.md)
- [`logs`](./logs.md)
- [`profiles`](./profiles.md)

## Non-goals (v0.1.0)

- Cloud-backed auth, device registry, or fleet inventory.
- Hosted relay / SSH tunneling automation.
- Multi-device orchestration.
- Python SDK (the CLI is operator-facing; the SDK is optional and separate).
