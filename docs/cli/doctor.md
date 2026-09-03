# `tensorplate doctor`

Read-only validation pass over device, runtime, and agent state. The command
never mutates desired state, restarts workers, downloads packages, or modifies
config. It is safe to run from any operator session.

```
tensorplate doctor [--skip-agent] [--record <dir>] [--output <human|json>]
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
| `host_facts` | Detected CPU architecture and vendor, read from the machine through `tensorplate-platform` — not from the binary's build target. `warning` when host identity could not be detected at all, with a hint that distinguishes a source that could not be *read* from one that was readable but not *interpretable*. |
| `host_os` | Detected OS identity (name, version, and image identity where the platform has one), plus the cloud machine shape as ` on <machine-type>` where the host reports one — that shape decides whether shape-scoped rows can match. The exact version, build, and L4T release follow in brackets for evidence recording. |
| `agent_config_valid` | Whether `/etc/tensorplate/agent.json` still satisfies the agent config schema. `skipped` when there is no agent config, which is normal for a CLI-only install; `unsupported` with every schema problem listed when it does not validate. Worth running BEFORE an upgrade: the agent refuses a config it cannot validate, including one carrying an unknown key, and the upgrade preflight checks only `schema_version`. The CLI validates against the schema rather than the agent's own loader because `cli/` may not depend on `agent/`; a contract test holds the two in agreement. |
| `platform_profile` | Which support rows the detected host could be. Deliberately a **set**: rows sharing an OS and CPU profile differ only by accelerator, so naming one would assert a match that has not been established. `unsupported` with a typed reason when the host matches no row; `unsupported` naming the machine shape when the hardware matches but no row's evidence covers the chassis it runs on — the hardware is validated, that shape is not, which does **not** by itself mean the machine will not run: where the matched row records its thermal, power and throttle signals as context rather than gates, the agent admits it on technical prerequisites and reports it as unvalidated at startup, while a row that gates on those signals requires evidence covering the machine. This finding is the host-level answer, taken before any accelerator is identified, so it cannot say which of the two applies; `skipped` when the registry could not be loaded (see `platform_registry` for why). Never `fail`: an off-matrix host is a reportable state, not a fault in `doctor`. |
| `platform_row` | The one support row the detected host **and accelerator** resolve to, or the typed reason they resolve to none. `platform_profile` answers from host identity alone and must report a *set*; this is the answer that set defers to, and it is where an exact-equality miss on either half becomes visible — a near-miss OS version or an off-matrix SKU resolves to no row with the reason naming which dimension missed. `unsupported` for a Planned or Experimental row (named, no validation evidence), for a machine shape no evidence covers, and for no match at all; `skipped` when the registry could not be loaded. Kept separate from `platform_profile` deliberately: an operator whose accelerator probe fails still gets the host-level answer, and the pair says which half of the identity was the problem. An unreadable accelerator probe plus independent NVIDIA PCI evidence is reported `unsupported` with `missing_driver_runtime`; a probe answer this release cannot interpret (for example, multiple GPUs) remains a detection warning and never blames the driver. |
| `model_class_rows` | Which model classes the matched row serves and at what support level, read from the row's `model_class_rows` registry pointers rather than a list kept in the CLI — a row that gains or loses a model class changes this without code. It consumes the same guarded resolution as `platform_row`, so a driverless GPU cannot resolve this dependent finding to a CPU row. `skipped` when no row matched or detection failed (the `platform_row` finding already carries why). A row that claims none says so plainly rather than rendering empty: a Planned row carries no model-class claims, and the registry refuses to let it. |
| `core_packages` | packaging. On the Debian package target, `tensorplate-common`, `-agent`, `-serving`, `-observability`, and `-cli` are installed and versioned. |
| `path_layout` | packaging. Every directory under `/etc/tensorplate`, `/var/lib/tensorplate`, `/var/log/tensorplate`, `/run/tensorplate`, `/usr/share/tensorplate/backends`, and `/usr/share/tensorplate/platform` is present, not world-writable, has the documented mode, and uses the expected owner/group on Linux. |
| `platform_registry` | packaging. The platform support registry at `/usr/share/tensorplate/platform` loads, and reports its row, supported-combination, and roadmap-target counts. `missing` when no registry is installed; `warning` when it is installed but this account cannot read it (it ships group-readable, so run as root or as a member of the `tensorplate` group — the registry itself is not inspected and is not claimed to be bad); **fails** when one is installed and readable but does not load — an invalid document, a collision between two rows, or no rows at all. The registry loads whole or not at all, so a partial load is never reported as a smaller registry. |
| `config_files` | packaging. Each `/etc/tensorplate/*.json` exists, has the documented file mode/ownership, and declares a recognized `schema_version`. |
| `config_endpoints` | packaging. Installed agent, serving-worker, and observability configs keep first-run endpoints on a Unix socket, loopback, or in-process transport. |
| `agent_systemd_unit` | packaging. `tensorplate-agent.service` is installed under a known systemd unit directory. |
| `agent_service_state` | packaging. Reports whether the agent is running, asked of the supervisor that owns it: `systemctl is-active` on Linux, `brew services list` on macOS, where the agent is a Homebrew-managed launchd job. Both Homebrew service findings share one validated listing; a failed query is `skipped`, not misreported as an absent service. A host without the supervisor is `skipped` rather than failed — the CLI runs on machines that never installed the services. |
| `observability_systemd_unit` | packaging. `tensorplate-observability.service` is installed. |
| `observability_service_state` | packaging. As `agent_service_state`, for the independent observability service. |
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
- `--record <dir>` runs no checks at all: it captures this machine's raw
  platform sources — the same files and command output detection reads — as
  private evidence under `<dir>`. The JSON and accelerator text use the
  shapes consumed by `test/platform/host_identity/` and
  `test/platform/accelerator/`, but they are **not committable as-is**: the
  command warns that the raw files can contain live cloud identifiers,
  device UUIDs, or serials. Create and review a sanitized publication copy
  under the [fixture and evidence rules](../validation/fixture-and-evidence-rules.md)
  before its first commit; retain the unsanitized capture privately.
  Record-first: the raw text is written even when
  detection cannot interpret it, because the machines worth recording are
  exactly the ones it cannot interpret yet — a multi-GPU host, an unknown
  SKU, a new OS image; the failure becomes a note in the output. When the
  machine resolves to a support row, the files are named for the row and the
  observed SKU is compared byte-for-byte against the row's declared one —
  a mismatch means the row gets corrected, never the recording. With a
  device configured, the command routes to the device and `<dir>` is a
  device-local path — the recording is written on the machine being
  recorded; fetch it from there.
- `--skip-agent` skips every agent-backed probe but still runs the install
  probes (packages, paths, configs, systemd units, service state, backend
  descriptor). Use it on a
  development host where no agent is running yet.
- The install probes degrade to `missing` (not `fail`) when no install layout
  is detected at all — that lets host CI on macOS / non-Linux dev hosts pass
  without a real package install. A *partial* install — some directories
  present but not all — still surfaces as `fail`.
