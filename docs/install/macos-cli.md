# TensorPlate CLI on macOS (Apple Silicon)

macOS support is **CLI-only by design**: the TensorPlate runtime (device
agent, serving worker, observability service) is Linux-only and is
installed on Jetson devices through the TensorPlate APT repository. The
macOS CLI is for operators driving a TensorPlate device from a
workstation.

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

The formula lives in the first-party tap at
[`tensorplate/homebrew-tap`](https://github.com/tensorplate/homebrew-tap).

## Notes and limitations

- **The tap is required.** Plain `brew install tensorplate` without the
  tap is not supported: the formula is not in `homebrew/core`.
- **Apple Silicon only.** The formula declares `arm64`; Intel Macs are
  not a supported target.
- **Built from source.** The formula compiles the CLI from the pinned
  release tag using Homebrew's Rust toolchain (build-time dependency
  only). Prebuilt bottles are follow-up work.
- No runtime service, launchd unit, or background process is installed —
  only the `tensorplate` binary.

## Upgrade and uninstall

```bash
brew upgrade tensorplate    # after a new TensorPlate release bumps the formula
brew uninstall tensorplate
brew untap tensorplate/tap  # optional
```
