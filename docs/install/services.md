# TensorPlate v0.1.0 systemd services

This page is the V01-E14-F03 service lifecycle contract.

## What is installed

| Unit | Package | Type | Notes |
| --- | --- | --- | --- |
| `tensorplate-agent.service` | `tensorplate-agent` | `simple` | Appliance entrypoint. Supervises the serving worker (V01-E09). |
| `tensorplate-observability.service` | `tensorplate-observability` | `simple` | Independent health monitor (V01-E10). |

No `tensorplate-serving.service` ships in v0.1.0. The serving worker
runs only as a child of the agent. Operators that try
`systemctl start tensorplate-serving` get `Unit not found` — that is the
intended behavior.

## Enable / start

The packages enable the units but do **not** start them at install
time. After `tensorplate doctor` reports a green pass, run:

```bash
sudo systemctl enable --now tensorplate-agent
sudo systemctl enable --now tensorplate-observability
```

Either order works: the observability unit declares no dependency on
the agent so a hung agent does not prevent missing-heartbeat
detection.

## Restart policy

Both units use `Restart=on-failure` with a bounded `RestartSec=5` and
`StartLimitBurst=5` within `StartLimitIntervalSec=60`. The start-limit
directives live in the `[Unit]` section for the Jetson systemd version. This gives
systemd enough room to recover from a hard crash without masking a
broken config: a unit that fails to start five times in a minute
enters `failed` state and stops retrying.

Inside the agent, V01-E09 supervises the serving worker with its own
bounded backoff + crash-loop detector. The two layers do not race:
systemd restarts only the agent process; the agent owns the worker.

## Hardening defaults

The unit files apply the same default sandbox to both services:

- `User=tensorplate`, `Group=tensorplate`
- `ProtectSystem=strict` + `ReadWritePaths=/var/lib/tensorplate /var/log/tensorplate /run/tensorplate`
- `RuntimeDirectory=tensorplate` so systemd recreates `/run/tensorplate`
  on every boot (no stale-socket cleanup required)
- `NoNewPrivileges=true`, `ProtectHome=true`, `PrivateTmp=true`,
  `PrivateDevices=true`, `ProtectKernel*=true`
- `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`
- `LockPersonality=true`, `RestrictRealtime=true`, `RestrictSUIDSGID=true`

These are starting points. Sites can tighten further with drop-in
files at `/etc/systemd/system/tensorplate-agent.service.d/*.conf`
without forking the unit; the v0.1.0 install does not assume any
non-default loosening (e.g. `MemoryDenyWriteExecute=true` is **not**
enabled because backend adapters JIT code on some platforms).

## EnvironmentFile

Both units source `/etc/default/tensorplate-<unit-name>` if it
exists. The packages do not ship these files; they are reserved for
operator overrides (e.g. `RUST_LOG=info`). The leading `-` in
`EnvironmentFile=-/etc/default/tensorplate-*` makes the file optional.

## Lifecycle expectations

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
- Upgrading the agent package while the unit is active triggers a
  restart on `configure`. Operator-visible side effect: the current
  serving worker is stopped and re-warmed by the new agent process.

## Debugging

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
