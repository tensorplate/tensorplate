# `test/packaging/`

packaging verification suite.

## What lives here

| File | What it asserts |
| --- | --- |
| `verify_layout.sh` | Stages the packaging install layout under a tempdir using the shared `packaging/scripts/install-paths.sh` helper and asserts every documented directory and config file has the documented permissions. Asserts that no directory ends up world-writable. |
| `verify_debian_metadata.sh` | Lints the debhelper-style metadata under `packaging/debian/`: every binary package declared in `control` has an `*.install` manifest, `/etc` config payloads rely on debhelper's single auto-generated conffile list, every maintainer script is executable, and only `tensorplate-agent` and `tensorplate-observability` ship a systemd unit. |
| `verify_apt_source.sh` | Lints the `tensorplate-apt-source` bootstrap package: arch `all` with no runtime dependencies, installs exactly the archive keyring and the Deb822 source, stable versionless `https://packages.tensorplate.com/apt` URI signed by the shipped keyring, fail-closed `postinst` that never runs `apt update` or touches runtime state, keyring dearmor wiring in `debian/rules`, and the staging-placeholder release guard in the release tooling. |
| `verify_metapackage.sh` | Lints the `tensorplate` runtime metapackage: `Architecture: any` (built once per runtime architecture, never `all`), `Section: metapackages`, strict-versioned depends on the full runtime set, Python backend suggested but never pulled in, no dependency on the apt bootstrap package, no payload or maintainer scripts, and release tooling carries it into assets/manifest/verification. |
| `verify_ready_check.sh` | Runs `tools/validation/tensorplate-ready-check.sh` against known-good and known-bad source/keyring fixtures (missing keyring, armored keyring, version-pinned URI, Signed-By mismatch) and asserts the correct verdict. |
| `verify_systemd_units.sh` | Parses `packaging/debian/*.service` and asserts the v0.1.0 invariants: `User=tensorplate`, `Group=tensorplate`, loopback-only `ReadWritePaths=`, hardening directives present, no `tensorplate-serving.service`. |
| `verify_lifecycle_scripts.sh` | Runs `version-utils.sh` and `upgrade-preflight.sh` against known-good and known-bad config fixtures and asserts each returns the correct exit code. |
| `verify_macos_homebrew_lifecycle.sh` | Syntax-checks the guarded M1 Pro Homebrew lifecycle harness, verifies its mutation opt-in, and runs shellcheck when available. The hardware run itself is manual. |
| `verify_descriptor.sh` | Validates the shipped backend descriptor against the JSON schema using `jq` (or falls back to a structural check). |
| `verify_descriptor.sh` | Validates the shipped backend descriptor's required fields, absolute interpreter path, and that its declared `tensorplate_runtime_range` both admits the release line this tree targets and brackets `packaging/VERSION`. The descriptor is not rewritten by the release driver on a version bump, so this is what makes the drift loud. |
| `verify_arch_package_set.sh` | Runs `dpkg-buildpackage -B` with stub ELF binaries and asserts debhelper emits exactly the five arch-dependent runtime packages for the host architecture, leaves the three `Architecture: all` packages to the primary build, keeps the metapackage payload-free, ships the serving binary from the package, and that the release job's copy globs exclude auto-generated `-dbgsym` packages. Not in `run.sh`: it writes `.deb` files to the repository parent. |
| `verify_installer.sh` | Syntax-checks `packaging/scripts/install.sh`, runs `shellcheck` when available, and exercises installer self-check, supported OS, unsupported OS, `--force-os`, hardware warning, `--strict-hardware`, and `--cli-only` paths with fixtures. |
| `verify_service_supervision.sh` | Drives the shipped systemd units against a real systemd: the agent reaches active and logs to the documented path, a hard crash is recovered, a crash LOOP is given up on rather than restarted forever, a clean stop is not restarted, observability survives the agent stopping, and no serving unit is registered. Mutates the host; refuses to run unless `CI=true` or `TP_SUPERVISION_ALLOW=1`. Not in `run.sh`. |
| `verify_cpu_only_smoke.sh` | The Ubuntu x86_64 CPU-only Preview smoke, and the only check that installs REAL binaries and runs the REAL CLI: builds the runtime, installs the package set, asserts package closure and the x86_64 agent config, confirms the installed registry reports the row as Preview, starts the agent and observability, requires a GREEN `tensorplate doctor` that resolves the row by live detection with no Production claim, and queries the control plane. Writes evidence under `dist/smoke/`. Mutates the host; refuses unless `CI=true` or `TP_CPU_SMOKE_ALLOW=1`. Not in `run.sh`. |
| `run.sh` | Orchestrator. Runs every verifier except the host-mutating `verify_arch_package_set.sh`, `verify_service_supervision.sh`, and `verify_cpu_only_smoke.sh`, and exits non-zero on the first failure. |

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
