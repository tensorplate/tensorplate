# Device Access Profiles

The CLI talks to **one** agent at a time. Profile selection precedence:

1. `--agent-url <host:port>` global flag → always wins, behaves like an ad-hoc
   `mode: url` profile.
2. `--profile <name>` global flag.
3. `default_profile` from the config file.
4. Hard-coded `local` profile against `/var/run/tensorplate/agent.sock` if no
   config is present.

## Config discovery

The config file itself is found in this order, first match wins:

1. `--config <path>`.
2. `$TENSORPLATE_CLI_CONFIG`. The Homebrew launcher exports this, pointing
   at the config under its own prefix; a value already set in the shell is
   left alone.
3. `/etc/tensorplate/cli.json` — the conffile the native packages install.
4. The built-in defaults.

Step 3 is one fixed absolute path, never a search of the working directory,
`$HOME`, or `$PATH`. On an installed host only root can write it.

A config found at any step must parse and validate; a malformed one fails
the command with exit `2` rather than falling back to the defaults, so the
CLI never quietly talks to a socket the operator did not configure. Step 3
is the one place a *missing* file is not an error: with no packaged install
there is nothing to read, and the built-in defaults are the documented
behaviour. If the packaged file exists but the caller cannot read it —
`/etc/tensorplate` is `root:tensorplate 0750` — the CLI uses the built-in
defaults and prints one line on stderr naming the file and the group, so
commands that do not need the agent keep working and the operator learns
why the packaged profile is not in effect.

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
