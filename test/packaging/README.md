# `test/packaging/`

packaging verification suite.

## What lives here

| File | What it asserts |
| --- | --- |
| `verify_layout.sh` | Stages the packaging install layout under a tempdir using the shared `packaging/scripts/install-paths.sh` helper and asserts every documented directory and config file has the documented permissions. Asserts that no directory ends up world-writable. |
| `verify_debian_metadata.sh` | Lints the debhelper-style metadata under `packaging/debian/`: every binary package declared in `control` has an `*.install` manifest, `/etc` config payloads rely on debhelper's single auto-generated conffile list, every maintainer script is executable, and only `tensorplate-agent` and `tensorplate-observability` ship a systemd unit. |
| `verify_systemd_units.sh` | Parses `packaging/debian/*.service` and asserts the v0.1.0 invariants: `User=tensorplate`, `Group=tensorplate`, loopback-only `ReadWritePaths=`, hardening directives present, no `tensorplate-serving.service`. |
| `verify_lifecycle_scripts.sh` | Runs `version-utils.sh` and `upgrade-preflight.sh` against known-good and known-bad config fixtures and asserts each returns the correct exit code. |
| `verify_descriptor.sh` | Validates the shipped backend descriptor against the JSON schema using `jq` (or falls back to a structural check). |
| `verify_installer.sh` | Syntax-checks `packaging/scripts/install.sh`, runs `shellcheck` when available, and exercises installer self-check, supported OS, unsupported OS, `--force-os`, hardware warning, `--strict-hardware`, and `--cli-only` paths with fixtures. |
| `run.sh` | Orchestrator. Runs every verifier and exits non-zero on the first failure. |

## Running

```bash
# Single command runs the full suite. Returns 0 on green.
./test/packaging/run.sh

# Or run an individual verifier.
./test/packaging/verify_layout.sh
```

The suite does not require root: every check operates on the source
tree or stages under `mktemp -d`. It runs on macOS dev hosts (no
systemd present) and on Linux CI hosts.

## What is NOT verified here

- Actual `.deb` artifact build (`dpkg-buildpackage`). The skeleton is
  build-system-ready but the upstream Rust + C++ binaries are not
  staged on host CI yet — `dpkg-buildpackage` runs on the release validation
  Jetson target.
- Live `systemctl enable --now`. That happens on the release validation target
  device, with output captured in
  `docs/install/clean-install-runbook.md`.
- CUDA / TensorRT / LibTorch functional checks. F06 doctor probes
  cover artifact presence; functional validation belongs to release validation.
