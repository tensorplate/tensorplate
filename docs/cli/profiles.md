# Device Access Profiles

The CLI talks to **one** agent at a time. Profile selection precedence:

1. `--agent-url <host:port>` global flag → always wins, behaves like an ad-hoc
   `mode: url` profile.
2. `--profile <name>` global flag.
3. `default_profile` from the config file.
4. Hard-coded `local` profile against `/var/run/tensorplate/agent.sock` if no
   config is present.

## Config schema

[`config/schemas/cli.json`](../../config/schemas/cli.json) is the canonical
source. v0.1.0 fields in summary:

```json
{
  "schema_version": "0.1",
  "default_profile": "local",
  "timeout_ms": 30000,
  "output": {"mode": "human", "color": "auto"},
  "log_source": {
    "kind": "file",
    "path": "/var/log/tensorplate/agent.ndjson",
    "tail_default": 100
  },
  "profiles": {
    "local": {
      "mode": "local",
      "socket_path": "/var/run/tensorplate/agent.sock"
    },
    "edge-orin": {
      "mode": "url",
      "agent_url": "127.0.0.1:18000",
      "serving_url": "http://127.0.0.1:18080",
      "timeout_ms": 60000,
      "display_name": "Orin via SSH tunnel"
    }
  }
}
```

## Modes

| Mode | v0.1.0 |
| --- | --- |
| `local` | Implemented. Uses the configured Unix domain socket. |
| `url` | Implemented. Connects to a loopback `host:port` TCP endpoint over an SSH tunnel, VPN, or overlay. The CLI does **not** open the tunnel for you. |
| `ssh_tunnel` | Reserved schema slot. Returns typed `Unsupported` at command execution. |
| `overlay`    | Reserved schema slot. Returns typed `Unsupported` at command execution. |
| `relay`      | Reserved schema slot. Returns typed `Unsupported` at command execution. |

The reserved modes parse so configs written for the hosted platform validate
against the v0.1.0 schema; commands that need to act fail loudly rather than
silently downgrading to `local`.

## Remote workflows

For a laptop-to-device workflow, the recommended pattern is:

```sh
# Tunnel the agent's loopback control endpoint.
ssh -L 18000:127.0.0.1:18000 orin-dev

# Point the CLI at the tunnel.
tensorplate --agent-url 127.0.0.1:18000 status
```

`tensorplate logs` reads local NDJSON files; it has no remote log API in
v0.1.0. SSH to the device and run `tensorplate logs` locally there. Remote
profiles return `unavailable` from `logs` rather than silently failing.
