# TensorPlate on macOS (Apple Silicon)

TensorPlate's Homebrew channel provides the complete appliance on Apple
M-series Macs running macOS 26. M-series compatibility is Preview; the
current hardware-validation target is an Apple M1 Pro MacBook Pro with 16 GB
unified memory. The appliance includes the agent, serving worker, CLI,
observability service, and Python/PyTorch backend.

## Install

```bash
brew install tensorplate/tap/tensorplate
tensorplate version
```

Or in two steps:

```bash
brew tap tensorplate/tap
brew install tensorplate
```

The formula graph lives in the first-party tap at
[`tensorplate/homebrew-tap`](https://github.com/tensorplate/homebrew-tap).

## Services

Homebrew exposes two independent launchd jobs:

```bash
brew services start tensorplate-agent
brew services start tensorplate-observability
```

Starting a service loads it immediately and registers it to start again at
login. Both jobs restart after an unsuccessful exit and launchd throttles
rapid failures. Observability has no dependency on the agent, so it can
continue to report missing health independently.

There is intentionally no `tensorplate-serving` launchd job. The agent starts
the formula-installed serving binary for an active deployment and applies its
own bounded restart and crash-loop policy to that child process.

Inspect or restart the jobs with:

```bash
brew services list
brew services restart tensorplate-agent
brew services restart tensorplate-observability
```

## Files, permissions, and local control

All mutable paths live under the active Homebrew prefix. View it with:

```bash
brew --prefix
```

The installed configs are under `$(brew --prefix)/etc/tensorplate`, durable
state is under `$(brew --prefix)/var/tensorplate`, and the agent socket is:

```text
$(brew --prefix)/var/run/tensorplate/agent.sock
```

Formula post-install hooks create the required directories with mode `0750`,
set service configs to `0640`, set the CLI config to `0644`, and fail with the
affected path if those modes cannot be enforced. The agent binds the socket
with mode `0660`. Services and the interactive CLI run as the same Homebrew
user, so no macOS system account or group membership change is required.

The packaged `tensorplate` launcher selects the installed CLI config by
default, which makes local commands use that UDS. An explicit `--config`
argument still has highest precedence; `TENSORPLATE_CLI_CONFIG` remains the
environment override.

## Packaged runtime discovery

The component formulas link read-only runtime data under:

```text
$(brew --prefix)/share/tensorplate/platform
$(brew --prefix)/share/tensorplate/backends/python_pytorch/backend.json
```

The agent and observability launchd jobs plus the packaged CLI point discovery
at those prefix-local paths. The agent also passes the Python/PyTorch backend
module path and the PyTorch formula's private Python interpreter to every
agent-owned serving worker. Operators do not need to set `PYTHONPATH` or a
sidecar interpreter in their login shell. The backend descriptor records that
exact interpreter, so doctor, the startup package probe, and the serving
worker inspect and launch the same runtime.

## Logs

launchd output is available at:

```text
$(brew --prefix)/var/log/tensorplate/agent.log
$(brew --prefix)/var/log/tensorplate/agent.error.log
$(brew --prefix)/var/log/tensorplate/observability.log
$(brew --prefix)/var/log/tensorplate/observability.error.log
```

Structured events are retained at
`$(brew --prefix)/var/log/tensorplate/events.ndjson` and can be read with:

```bash
tensorplate logs
tensorplate logs --follow
```

See [`filesystem-layout.md`](./filesystem-layout.md) for the complete mode
contract.

## Notes and limitations

- **The tap is required.** Plain `brew install tensorplate` without the
  tap is not supported: the formula is not in `homebrew/core`.
- **M-series Preview.** Recognized Apple M-series chips on macOS 26 use a
  conservative Preview compatibility row. The exact M1 Pro 16 GB row remains
  the current evidence-backed hardware-validation target and takes precedence.
  Other M-series chips are not yet individually hardware-validated, and their
  admission capacity is capped at 16 GiB even when more unified memory is
  detected. Intel Macs and Apple chips outside the M-series fail closed.
- **Built from source.** The component formula graph compiles the Rust
  services and CLI, builds the Python/PyTorch sidecar package, and installs
  their pinned runtime dependencies. Prebuilt bottles are follow-up work.

## Upgrade and uninstall

```bash
brew upgrade tensorplate    # after a new TensorPlate release bumps the formula
brew services stop tensorplate-agent
brew services stop tensorplate-observability
brew uninstall tensorplate \
  tensorplate-agent \
  tensorplate-backend-python-pytorch \
  tensorplate-cli \
  tensorplate-observability \
  tensorplate-serving
brew untap tensorplate/tap  # optional
```

The meta-formula does not own the component kegs after installation, so
uninstall every component before untapping. Homebrew may retain configuration
and state under its prefix; review those paths separately before removing
operator data.
