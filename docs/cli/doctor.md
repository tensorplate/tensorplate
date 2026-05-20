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
| `host_facts` / `host_os` | Bounded host detection (arch, OS). |
| `python_pytorch_backend` / `tensorrt_runtime` / `libtorch_runtime` | Deferred to V01-E14 packaging probes; surfaced as `missing` so operators know to consult the agent's view. |
| `ros2_health_stub` | Deferred to `tensorplate status` against the observability snapshot. |

Each finding has a stable `id`, `status` (`ok`, `fail`, `missing`, `unsupported`,
`skipped`, `warning`), `severity` (`info`, `warning`, `critical`), human
`message`, and optional `hint`. JSON output preserves all fields so V01-E15
scripts can grep on `id` strings.

## Exit codes

- `0` if no finding has `status: fail`.
- `10` if at least one finding is failing (see [exit-codes.md](./exit-codes.md)).

## Limitations

- The CLI cannot reliably introspect TensorRT/LibTorch/CUDA/PyTorch installs
  from inside a Rust process without breaking sandboxing assumptions. V01-E14
  ships those probes inside the agent and packaging layer; the CLI surfaces
  the agent's view via `agent_state` and `worker_state`.
- `--skip-agent` skips every agent-backed probe but still runs the local
  environment probes. Use it on a development host where no agent is
  running yet.
