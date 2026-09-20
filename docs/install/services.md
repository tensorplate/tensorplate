# TensorPlate service supervision

This page is the packaging service lifecycle contract.

## Linux systemd services

### What is installed

| Unit | Package | Type | Notes |
| --- | --- | --- | --- |
| `tensorplate-agent.service` | `tensorplate-agent` | `simple` | Appliance entrypoint. Supervises the serving worker (V01-E09). |
| `tensorplate-observability.service` | `tensorplate-observability` | `simple` | Independent health monitor (V01-E10). |

No `tensorplate-serving.service` ships in v0.1.0. The serving worker
runs only as a child of the agent. Operators that try
`systemctl start tensorplate-serving` get `Unit not found` — that is the
intended behavior.

### Enable / start

The packages enable the units but do **not** start them at install
time. After `tensorplate doctor` reports a green pass, run:

```bash
sudo systemctl enable --now tensorplate-agent
sudo systemctl enable --now tensorplate-observability
```

Either order works: the observability unit declares no dependency on
the agent so a hung agent does not prevent missing-heartbeat
detection.

### Restart policy

Both units use `Restart=on-failure` with a bounded `RestartSec=5` and
`StartLimitBurst=5` within `StartLimitIntervalSec=60`. The start-limit
directives live in the `[Unit]` section for the Jetson systemd version. This gives
systemd enough room to recover from a hard crash without masking a
broken config: a unit that fails to start five times in a minute
enters `failed` state and stops retrying.

Inside the agent, V01-E09 supervises the serving worker with its own
bounded backoff + crash-loop detector. The two layers do not race:
systemd restarts only the agent process; the agent owns the worker.

### Start-up platform detection

The agent settles which platform row it is running on once, at start. On
a Compute Engine instance that verdict needs the GCE metadata service,
which on some boots is not answering yet when the unit starts. The agent
therefore retries the observation a bounded number of times over a
bounded window before it settles, and says what it did:

- `platform detection recovered: attempt=... elapsed=... first_error=...`
  — a later attempt answered. The start is normal from here on; the line
  records the delay and the failure that preceded it so a boot that
  needed the retry is still visible.
- `platform detection exhausted: attempts=... elapsed=... budget=...`
  followed by `platform detection failed: ...` — no attempt answered
  inside the window. The agent keeps running and keeps listening, but it
  refuses deploys, because an agent that cannot read its own hardware
  must not deploy as though the check passed.

The remedy for an exhausted detection is unchanged: restart
`tensorplate-agent` once with the metadata service reachable.

Neither line is written on a host that answers on the first attempt, and
neither is written off Compute Engine. On every other platform the
verdict is settled from local sources and the retry costs nothing.

### Hardening defaults

The unit files apply the same default sandbox to both services:

- `User=tensorplate`, `Group=tensorplate`
- `ProtectSystem=strict` + `ReadWritePaths=/var/lib/tensorplate /var/log/tensorplate`
- `NoNewPrivileges=true`, `ProtectHome=true`, `PrivateTmp=true`,
  `ProtectKernel*=true`
- `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`
- `LockPersonality=true`, `RestrictRealtime=true`, `RestrictSUIDSGID=true`
- `Restart=on-failure` with `RestartSec=5`, bounded by
  `StartLimitBurst=5` / `StartLimitIntervalSec=60`, so a hard crash is
  recovered but a crash loop is given up on instead of masking a config
  error forever

`/run/tensorplate` is the **agent's** alone. Only
`tensorplate-agent.service` declares `RuntimeDirectory=tensorplate` and names
the path in `ReadWritePaths=`, because that is where the agent control socket
lives and systemd deletes a `RuntimeDirectory=` when its unit stops. Both
units used to declare it, which meant stopping the agent removed a directory
observability still named — and observability could then not start at all,
failing at step `NAMESPACE`. Observability needs no runtime directory: its
listener transport is `in_process` and its snapshot lives under
`StateDirectory=`. If the reserved Unix-socket listener lands, it must declare
a runtime directory it owns rather than the agent's.

`tensorplate-agent.service` explicitly keeps `PrivateDevices=false`
because it supervises `tensorplate-serving` as a child process and the
worker must see Jetson CUDA/TensorRT device nodes. Observability does
not need device access and keeps `PrivateDevices=true`.

These are starting points. Sites can tighten further with drop-in
files at `/etc/systemd/system/tensorplate-agent.service.d/*.conf`
without forking the unit; the v0.1.0 install does not assume any
non-default loosening (e.g. `MemoryDenyWriteExecute=true` is **not**
enabled because backend adapters JIT code on some platforms).

### EnvironmentFile

Both units source `/etc/default/tensorplate-<unit-name>` if it
exists. The packages do not ship these files; they are reserved for
operator overrides (e.g. `RUST_LOG=info`). The leading `-` in
`EnvironmentFile=-/etc/default/tensorplate-*` makes the file optional.

### Lifecycle expectations

- `systemctl restart tensorplate-agent` triggers a graceful shutdown:
  the agent flushes desired-state, gives the supervised serving
  worker its `graceful_stop_timeout_ms` window (default 5 s), and then
  exits.
- `systemctl restart tensorplate-observability` discards in-memory
  diagnostics by design; persistent state lives in
  `/var/log/tensorplate/` only when the `diagnostics_retention`
  section configures a file sink.
- `systemctl reload tensorplate-agent` sends `SIGHUP`; the v0.1.0
  agent ignores reload requests because every mutation is a deploy
  transaction. The unit exposes the action so future config-reload
  work has a stable surface.
- Upgrading the agent or observability package stops that unit: the
  package's `prerm` stops it on `upgrade`, and nothing in the packages
  starts it again, because the units are installed with
  `dh_installsystemd --no-start`. The current serving worker stops with
  the agent. `sudo systemctl enable --now tensorplate-agent
  tensorplate-observability`, which the release `install.sh` runs after
  installing, brings both back, and the new agent re-warms the active
  deployment from durable state. See
  [`lifecycle.md`](./lifecycle.md#upgrade).

### Debugging

```bash
systemctl status tensorplate-agent
journalctl -u tensorplate-agent -f
systemctl status tensorplate-observability
journalctl -u tensorplate-observability -f
```

The `tensorplate doctor` install probes also surface the same state
through stable finding IDs (`agent_service_state`,
`observability_service_state`, `serving_systemd_absent` — see
`docs/cli/doctor.md`).

## macOS Homebrew services

The Homebrew agent and observability formulas each generate a launchd job.
They run independently, start when loaded through `brew services`, restart
after an unsuccessful exit, and use a five-second launch throttle. The stable
commands are:

```bash
brew services start tensorplate-agent
brew services start tensorplate-observability
brew services list
```

The serving formula deliberately defines no service. The agent launches the
formula-installed serving worker for the active deployment and remains its
sole process owner.

Homebrew uses the formula's stable `opt_bin` path in each generated service,
so an upgrade can move the versioned Cellar directory without leaving a
launchd job pointing at the old keg. Standard output and standard error are
routed to the paths documented in
[`filesystem-layout.md`](./filesystem-layout.md).
