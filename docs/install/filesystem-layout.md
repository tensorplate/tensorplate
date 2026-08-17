# TensorPlate filesystem layout

This document is the on-device contract owned by packaging. Linux package
paths and modes are defined by
[`protocol/rust/src/install_paths.rs`](../../protocol/rust/src/install_paths.rs);
the shell sourcing helper at
[`packaging/scripts/path-constants.sh`](../../packaging/scripts/path-constants.sh)
mirrors it so maintainer scripts, integration tests, and `tensorplate
doctor` install probes assert against identical values. macOS paths are
prefix-rendered from [`packaging/homebrew/conf/`](../../packaging/homebrew/conf/)
and enforced by the owning formulas.

Operators reading this doc want to understand: where are configs, where
is durable state, where do bundles live, what runs as which user, and
what does `tensorplate doctor` check.

## Linux package layout

### Users and groups

| Identity | Purpose |
| --- | --- |
| `tensorplate` (system user) | Runs `tensorplate-agent`, `tensorplate-observability`, and the agent-supervised `tensorplate-serving` process. Created by `tensorplate-common` postinst. Shell is `/usr/sbin/nologin`. |
| `tensorplate` (system group) | Owns durable state, log, runtime, and config files. CLI / operator users that need to query the agent socket can be added to this group; that is the only group-write privilege the layout grants. |

Removing a tensorplate package never removes the `tensorplate` user or
group. Adding the user to `video`, `render`, or `dialout` for hardware
access is a site policy and is left to the operator.

### Directories

| Path | Owner:Group | Mode | Created by | Purpose |
| --- | --- | --- | --- | --- |
| `/etc/tensorplate/` | `root:tensorplate` | `0750` | `tensorplate-common` postinst | Config root. Holds the four installed config files. |
| `/var/lib/tensorplate/` | `tensorplate:tensorplate` | `0750` | `tensorplate-common` postinst | Durable state root. |
| `/var/lib/tensorplate/state/` | `tensorplate:tensorplate` | `0750` | postinst | Desired-state file and transaction journals. |
| `/var/lib/tensorplate/bundles/staging/` | `tensorplate:tensorplate` | `0750` | postinst | Verified bundles stage here under `<deployment_id>/`. |
| `/var/lib/tensorplate/bundles/active/` | `tensorplate:tensorplate` | `0750` | postinst | Active deployment (symlink or dir). |
| `/var/lib/tensorplate/bundles/previous/` | `tensorplate:tensorplate` | `0750` | postinst | Previous deployment retained for rollback. |
| `/var/lib/tensorplate/bundles/quarantine/` | `tensorplate:tensorplate` | `0750` | postinst | Failed deploys land here for operator review. |
| `/var/lib/tensorplate/bundles/import/` | `tensorplate:tensorplate` | `1775` | postinst | Remote enrollment copies bundles here over SSH before the agent stages them; sticky + group-writable so a group member can stage without deleting others' imports. |
| `/var/lib/tensorplate/worker-configs/` | `tensorplate:tensorplate` | `0750` | postinst | Agent-rendered serving-worker configs (one per warming candidate). |
| `/var/log/tensorplate/` | `tensorplate:tensorplate` | `0750` | postinst | On-disk JSON-lines logs when the observability service is configured for file retention. journald is preferred. |
| `/run/tensorplate/` | `tensorplate:tensorplate` | `0750` | systemd `RuntimeDirectory=` for `tensorplate-agent.service`; postinst as fallback. | tmpfs; holds the agent control socket. |
| `/usr/share/tensorplate/backends/` | `root:tensorplate` | `0750` | `tensorplate-common` postinst | Backend descriptors read by doctor / agent. |
| `/usr/share/tensorplate/platform/` | `root:tensorplate` | `0750` | `tensorplate-common` payload; mode applied by `install-paths.sh` | Platform support registry (`rows/` and `roadmap_targets/`) read by the agent, `doctor`, and the observability service. Group-readable, not world-readable: a caller outside the `tensorplate` group can stat it but not read it. |
| `/usr/lib/tensorplate/` | `root:root` | `0755` | dpkg | Holds the agent-supervised `tensorplate-serving` binary and optional backend payloads. |

### Files

| Path | Owner:Group | Mode | Conffile? | Purpose |
| --- | --- | --- | --- | --- |
| `/etc/tensorplate/agent.json` | `root:tensorplate` | `0640` | yes | Agent runtime config (schema: `config/schemas/agent.json`). |
| `/etc/tensorplate/observability.json` | `root:tensorplate` | `0640` | yes | Observability config (schema: `config/schemas/observability.json`). |
| `/etc/tensorplate/serving_worker.json` | `root:tensorplate` | `0640` | yes | Serving worker config (schema: `config/schemas/serving_worker.json`). |
| `/etc/tensorplate/cli.json` | `root:tensorplate` | `0644` | yes | CLI default profile (schema: `config/schemas/cli.json`). Readable by all operators. |
| `/run/tensorplate/agent.sock` | `tensorplate:tensorplate` | `0660` | n/a (socket) | Agent control Unix domain socket. Group membership grants CLI access. |
| `/usr/share/tensorplate/backends/python_pytorch/backend.json` | `root:tensorplate` | `0644` | no | Python/PyTorch backend descriptor. Read by `tensorplate doctor`. |

## macOS Homebrew layout

Homebrew expands `${HOMEBREW_PREFIX}` to its installation prefix
(`/opt/homebrew` on the supported Apple Silicon installation). Services run
as the user that invoked `brew services`; no system user or group is created.
The formula post-install hooks reject symlinked managed paths and fail with
the exact path and required mode when a directory or file cannot be secured.

| Path | Mode | Owner | Purpose |
| --- | --- | --- | --- |
| `${HOMEBREW_PREFIX}/etc/tensorplate/` | `0750` | Homebrew user | Installed runtime configs. |
| `${HOMEBREW_PREFIX}/etc/tensorplate/agent.json` | `0640` | Homebrew user | UDS, state, staging, and serving-worker paths. |
| `${HOMEBREW_PREFIX}/etc/tensorplate/observability.json` | `0640` | Homebrew user | Snapshot and structured-diagnostics paths. |
| `${HOMEBREW_PREFIX}/etc/tensorplate/cli.json` | `0644` | Homebrew user | Local CLI profile; selected by the packaged CLI launcher unless the operator supplies a config. |
| `${HOMEBREW_PREFIX}/var/tensorplate/` | `0750` | Homebrew user | Durable state root and service working directory. |
| `${HOMEBREW_PREFIX}/var/tensorplate/state/` | `0750` | Homebrew user | Agent state and the observability snapshot. |
| `${HOMEBREW_PREFIX}/var/tensorplate/bundles/staging/` | `0750` | Homebrew user | Verified bundle staging. |
| `${HOMEBREW_PREFIX}/var/tensorplate/worker-configs/` | `0750` | Homebrew user | Agent-rendered serving configs. |
| `${HOMEBREW_PREFIX}/var/run/tensorplate/` | `0700` | Homebrew user | Owner-only runtime directory for the agent control socket. |
| `${HOMEBREW_PREFIX}/var/run/tensorplate/agent.sock` | `0600` | Homebrew user | Owner-only local CLI-to-agent UDS; the agent applies the mode after binding. |
| `${HOMEBREW_PREFIX}/var/log/tensorplate/` | `0750` | Homebrew user | launchd output and structured diagnostics. |
| `${HOMEBREW_PREFIX}/var/log/tensorplate/{agent,agent.error,observability,observability.error}.log` | `0640` | Homebrew user | launchd standard output and standard error. |
| `${HOMEBREW_PREFIX}/var/log/tensorplate/events.ndjson` | `0640` | Homebrew user | Bounded structured events read by `tensorplate logs`. |

The agent uses the stable
`${HOMEBREW_PREFIX}/opt/tensorplate-serving/libexec/tensorplate-serving`
path. This survives formula version changes and keeps the worker out of the
operator's command path.

## Network surfaces

The default install does **not** open any non-loopback port.

- Agent control: Unix domain socket at `/run/tensorplate/agent.sock` for
  Linux packages or `${HOMEBREW_PREFIX}/var/run/tensorplate/agent.sock` for
  Homebrew. The reserved alternative `loopback_tcp` transport binds to
  `127.0.0.1` only and rejects any other host in config validation.
- Serving worker: loopback bind (`127.0.0.1`) on a fixed port owned by
  the agent. The agent rejects non-loopback `serving_bind_host` values
  in process mode.
- Observability: in-process transport by default. The reserved
  `unix_socket` listener stays within the platform runtime directory when
  enabled in a future release.

## What `tensorplate doctor` checks on Linux packages

For each directory, the `path_layout` family of findings asserts:

- path exists
- directory mode matches its expected mode (`DIR_0750` for most; `DIR_1775`
  for the `bundles/import/` staging dir)
- group ownership is the `tensorplate` system group (best-effort on
  non-Linux hosts: reported as `unsupported` rather than `fail`)
- world-writable is **not** set

For each config file, the `config_files` finding asserts:

- file exists
- mode matches `FILE_0640` (or `FILE_0644` for the CLI config)
- root ownership and `tensorplate` group ownership on Linux package installs
- the file parses as JSON and has a recognized `schema_version`

The `config_endpoints` finding reads the installed agent, serving
worker, and observability configs and fails when a first-run endpoint
escapes the packaged Unix-socket, loopback, or in-process defaults.

For the agent control socket, the `agent_socket` finding asserts the
socket path lives under `/run/tensorplate/` and is owned by the
expected group.

The Python/PyTorch backend descriptor is checked by the
`python_pytorch_backend` family of findings (see
`docs/install/python-pytorch-backend.md`).

## Why these paths

- `/etc/tensorplate/` follows FHS for configuration of multiple
  cooperating services owned by one project.
- `/var/lib/tensorplate/` keeps durable state local to the device, in
  line with the v0.1.0 promise of no hosted-platform dependency.
- `/run/tensorplate/` is tmpfs and lets systemd recreate the socket
  directory on every boot; the agent therefore never needs to clean up
  stale sockets after a crash.
- `/usr/lib/tensorplate/` for `tensorplate-serving` matches the
  V01-E09 supervision decision that the agent owns the worker's
  lifecycle: the binary lives off `$PATH` so an operator cannot
  accidentally launch it ahead of the agent.
- `/usr/share/tensorplate/backends/` collects optional backend
  descriptors in a stable location so doctor probes do not have to
  walk arbitrary Python environments.
- `/usr/share/tensorplate/platform/` holds one copy of the platform
  support registry for the whole device. The agent, `doctor`, and the
  observability service all read it from here rather than each shipping
  a copy, so they cannot disagree about which platforms are supported.
  It is package data: a device never edits it, and the registry either
  loads whole or not at all.
