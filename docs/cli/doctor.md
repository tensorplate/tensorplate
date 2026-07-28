# `tensorplate doctor`

Read-only validation pass over device, runtime, and agent state. The command
never mutates desired state, restarts workers, downloads packages, or modifies
config. It is safe to run from any operator session.

```
tensorplate doctor [--skip-agent] [--output <human|json>]
```

## What it checks

| Finding ID | What it asserts |
| --- | --- |
| `cli_version` | CLI + protocol version stamp. |
| `profile_mode` | Resolved profile mode is supported in v0.1.0. |
| `agent_socket` | Local profile's UDS path exists. |
| `agent_reachable` | Version request round-trips against the agent. |
| `agent_status_shape` | Agent returns a parseable `AgentStatus`. |
| `agent_state` | Agent self-reported state is ready/degraded/failed. |
| `active_deployment` | The agent has an active deployment promoted. |
| `worker_state` / `worker_crash_loop` | Supervision summary, crash-loop flag. |
| `host_facts` | Detected CPU architecture and vendor, read from the machine through `tensorplate-platform` — not from the binary's build target. |
| `host_os` | Detected OS identity (name, version, and image identity where the platform has one), with the exact version, build, and L4T release alongside it for evidence recording. |
| `platform_profile` | Which support rows the detected host could be. Deliberately a **set**: rows sharing an OS and CPU profile differ only by accelerator, so naming one would assert a match that has not been established. `unsupported` with a typed reason when the host matches no row; `skipped` when there is no installed registry to match against (see `platform_registry`). |
| `core_packages` | packaging. On the Debian package target, `tensorplate-common`, `-agent`, `-serving`, `-observability`, and `-cli` are installed and versioned. |
| `path_layout` | packaging. Every directory under `/etc/tensorplate`, `/var/lib/tensorplate`, `/var/log/tensorplate`, `/run/tensorplate`, `/usr/share/tensorplate/backends`, and `/usr/share/tensorplate/platform` is present, not world-writable, has the documented mode, and uses the expected owner/group on Linux. |
| `platform_registry` | packaging. The platform support registry at `/usr/share/tensorplate/platform` loads, and reports its row, supported-combination, and roadmap-target counts. `missing` when no registry is installed; `warning` when it is installed but this account cannot read it (it ships group-readable, so run as root or as a member of the `tensorplate` group — the registry itself is not inspected and is not claimed to be bad); **fails** when one is installed and readable but does not load — an invalid document, a collision between two rows, or no rows at all. The registry loads whole or not at all, so a partial load is never reported as a smaller registry. |
| `config_files` | packaging. Each `/etc/tensorplate/*.json` exists, has the documented file mode/ownership, and declares a recognized `schema_version`. |
| `config_endpoints` | packaging. Installed agent, serving-worker, and observability configs keep first-run endpoints on a Unix socket, loopback, or in-process transport. |
| `agent_systemd_unit` | packaging. `tensorplate-agent.service` is installed under a known systemd unit directory. |
| `agent_service_state` | packaging. Reports whether the agent unit is active, stopped, failed, or not queryable. |
| `observability_systemd_unit` | packaging. `tensorplate-observability.service` is installed. |
| `observability_service_state` | packaging. Reports whether the independent observability unit is active, stopped, failed, or not queryable. |
| `serving_systemd_absent` | packaging. **Fails** if `tensorplate-serving.service` is installed — the agent supervises the serving worker (V01-E09). |
| `serving_binary_installed` | packaging. `/usr/lib/tensorplate/tensorplate-serving` exists. |
| `python_pytorch_backend` | Packaging probe. The backend descriptor at `/usr/share/tensorplate/backends/python_pytorch/backend.json` is present and parses. |
| `python_pytorch_runtime` | packaging. The descriptor's Python interpreter exists, meets the declared minimum Python version, imports the declared backend module, and imports PyTorch at or above the declared minimum version. |
| `cuda_runtime` / `tensorrt_runtime` / `libtorch_runtime` | packaging. Best-effort artifact presence (paths only — actual validation happens in release validation). |
| `ros2_health_stub` | Packaged observability config exposes the optional ROS 2 health-stub section; runtime publications remain visible in `tensorplate status`. |

Each finding has a stable `id`, `status` (`ok`, `fail`, `missing`, `unsupported`,
`skipped`, `warning`), `severity` (`info`, `warning`, `critical`), human
`message`, and optional `hint`. JSON output preserves all fields so release validation
scripts can grep on `id` strings.

## Exit codes

- `0` if no finding has `status: fail`.
- `10` if at least one finding is failing (see [exit-codes.md](./exit-codes.md)).

## Limitations

- CUDA / TensorRT / LibTorch checks assert *artifact presence* only. Functional
  validation happens in release validation.
- The PyTorch runtime check shells out to the interpreter pinned in the
  backend descriptor (e.g. `/usr/bin/python3 -c 'import torch; ...'`). It
  never executes user model code; refused module names that fail an
  identifier-safety check fail as `python_pytorch_runtime = fail`.
- `--skip-agent` skips every agent-backed probe but still runs the install
  probes (packages, paths, configs, systemd units, service state, backend
  descriptor). Use it on a
  development host where no agent is running yet.
- The install probes degrade to `missing` (not `fail`) when no install layout
  is detected at all — that lets host CI on macOS / non-Linux dev hosts pass
  without a real package install. A *partial* install — some directories
  present but not all — still surfaces as `fail`.
