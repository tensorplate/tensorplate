# TensorPlate on macOS (Apple Silicon)

TensorPlate's Homebrew channel provides the complete appliance on macOS 26:
the agent, serving worker, CLI, observability service, and Python/PyTorch
backend. It supports Apple Silicon only.

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

## Notes and limitations

- **The tap is required.** Plain `brew install tensorplate` without the
  tap is not supported: the formula is not in `homebrew/core`.
- **macOS 26 and Apple Silicon only.** Earlier macOS releases and Intel Macs
  are not supported.
- **Built from source.** The formula compiles the CLI from the pinned
  release tag using Homebrew's Rust toolchain (build-time dependency
  only). Prebuilt bottles are follow-up work.

## Upgrade and uninstall

```bash
brew upgrade tensorplate    # after a new TensorPlate release bumps the formula
brew services stop tensorplate-agent
brew services stop tensorplate-observability
brew uninstall tensorplate
brew untap tensorplate/tap  # optional
```
