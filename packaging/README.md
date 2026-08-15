# TensorPlate native packaging (V01-E14)

This tree owns the v0.1.0 native package skeleton for TensorPlate. It
follows the OSS Debian / debhelper convention so the same source tree
can produce `.deb` artifacts via `dpkg-buildpackage -us -uc -b` on a
Jetson-class Linux host once the binaries are built and staged.

V01-E14 does **not** publish packages anywhere. It produces inspectable
artifacts and a clean-install runbook that V01-E15 consumes on the
target device. Signing, APT repositories, and GitHub release uploads
are out of scope for this milestone. The public release publication flow
is owned by [`docs/release/`](../docs/release/).

## Layout

```
packaging/
├── README.md                       This file.
├── VERSION                         Single-source version stamp.
├── version.sh                      Helper to emit the version string.
├── debian/                         debhelper-style packaging metadata.
│   ├── control                     Source + binary package definitions and debhelper compat level.
│   ├── changelog                   Debian changelog (auto-aligned to VERSION).
│   ├── rules                       dh-style build / install rules.
│   ├── copyright                   Apache-2.0 declaration.
│   ├── source/format               Quilt format.
│   ├── tensorplate-agent.install   Per-binary-package install manifest.
│   ├── tensorplate-agent.postinst  Per-package maintainer script.
│   ├── tensorplate-agent.prerm
│   ├── tensorplate-agent.postrm
│   ├── tensorplate-agent.service   Auto-installed by dh_installsystemd.
│   ├── ...                         Same files for -observability/-serving/-cli/-backend-python-pytorch.
│   ├── tensorplate-agent.service
│   ├── tensorplate-observability.service       Auto-installed by dh_installsystemd.
│   └── tensorplate-apt-source.*    APT source bootstrap package manifest + fail-closed postinst.
├── apt/                            Payloads shipped by tensorplate-apt-source; see apt/README.md.
│   ├── tensorplate.sources         Deb822 stable-channel source installed to /etc/apt/sources.list.d/.
│   └── tensorplate-archive-keyring.asc  Armored archive key, dearmored into the package at build time.
├── conf/                           Default config installed under /etc/tensorplate/.
├── homebrew/                       Source formula graph for the macOS Homebrew tap.
│   ├── Formula/                    Five component formulas plus the tensorplate meta-formula.
│   └── README.md                   Rendering and validation notes.
├── scripts/                        Shared helpers used by maintainer scripts and tests.
│   ├── install.sh                  Release installer published with GitHub Release assets.
│   ├── build-install-from-source.sh Build/install unreleased branch snapshots through install.sh.
│   ├── build-deb.sh                Source-tree helper for dpkg-buildpackage.
│   └── ...                         Maintainer-script helpers installed by tensorplate-common.
└── backend-metadata/               JSON descriptors consumed by doctor + agent for backend detection.
```

## Package split

| Package | Purpose | Binary | systemd? |
| --- | --- | --- | --- |
| `tensorplate-agent` | Device control plane, deploy transaction, worker supervision. | `tensorplate-agent` | yes — `tensorplate-agent.service` |
| `tensorplate-serving` | Data-plane serving worker. Installed but launched by the agent. | `tensorplate-serving` | no — supervised by agent (V01-E09) |
| `tensorplate-observability` | Independent health monitor. | `tensorplate-observability` | yes — `tensorplate-observability.service` |
| `tensorplate-cli` | Operator CLI. | `tensorplate` | no |
| `tensorplate-backend-python-pytorch` | Python sidecar backend for SmolVLA / Python-native bundles. | `tensorplate-backend-python-pytorch` (entrypoint) | no — backend lifecycle is per-session by the serving worker |
| `tensorplate-apt-source` | One-time APT source bootstrap: archive keyring + stable Deb822 source. Installs no runtime component and never runs `apt update`. | — | no |
| `tensorplate` | Full-runtime metapackage, built once per runtime architecture: depends on agent, serving, observability, CLI, and common; ships no files. | — | no |

Core packages do **not** depend on `tensorplate-backend-python-pytorch`.
Installing it later makes the Python module + console entrypoint
discoverable by the descriptor interpreter; after PyTorch is installed
and the agent is restarted, SmolVLA-class deploys can pass the startup
backend probe.

The Homebrew graph mirrors the five appliance components and keeps the
existing `tensorplate` install name as a meta-formula. Release automation
renders every formula from the same source tag and checksum, then proposes
the graph as one tap change. The agent and observability formulas define
independent launchd jobs; serving deliberately has no job because the agent
owns its lifecycle. Prefix-rendered configs, state, runtime socket, and log
paths are installed from `packaging/homebrew/conf/`; formula post-install
checks enforce their modes before a service can be started.

## Building

The `debian/rules` file assumes the upstream build has already produced
release artifacts at `target/release/` (Rust) and `build/release/` (C++).
`tools/release/build-release-artifacts.sh` chains those builds for
release and snapshot artifact bundles; the packaging tree owns staging
into `debian/<pkg>/`.

```bash
# 1) Build upstream binaries (out of scope for the skeleton).
cargo build --release --bin tensorplate-agent --bin tensorplate-observability --bin tensorplate
cmake --build build/release --target tensorplate-serving

# 2) Run packaging lint / dry-run.
./packaging/scripts/verify-artifacts.sh

# 3) Build Debian binary packages.
./packaging/scripts/build-deb.sh
```

`dpkg-buildpackage` expects a root `debian/` directory. The helper creates
the temporary `debian -> packaging/debian` symlink needed by that tool and
removes it after the build when it created it.

For an unreleased branch snapshot, use the source installer wrapper. It
builds packages with a `X.Y.Z~dev.YYYYMMDD.gitsha` Debian version, writes
the local manifest and `SHA256SUMS`, then calls `install.sh
--local-artifacts --allow-unsigned`:

```bash
sudo bash packaging/scripts/build-install-from-source.sh --branch develop
```

## v0.1.0 invariants

- All bind addresses default to loopback / Unix domain sockets. The
  package never publishes a public network endpoint.
- Maintainer scripts are idempotent: reinstall preserves user config,
  desired state, bundles, and logs unless `--purge` is supplied.
- The packaged agent default uses the process-backed worker and launches
  `/usr/lib/tensorplate/tensorplate-serving`; the mock backend remains
  test-only and is not advertised by the installed config.
- The packaged backend list advertises `tensorrt` for the Jetson vision
  path and `python_pytorch` for the optional sidecar path. Python/PyTorch
  deploys are still refused before staging until the optional backend
  package and PyTorch runtime probe are green.
- `dh_installsystemd` enables `tensorplate-agent.service` and
  `tensorplate-observability.service` but does **not** start them.
  Operators run `systemctl enable --now tensorplate-agent` after
  `tensorplate doctor` reports green.
- `tensorplate-serving` has no `.service` unit. The agent owns the
  serving-worker lifecycle (V01-E09).
- The Python/PyTorch backend lands a backend descriptor at
  `/usr/share/tensorplate/backends/python_pytorch/backend.json`. The
  agent + CLI use that descriptor for typed, actionable doctor /
  deploy compatibility checks (V01-E14-F05).

See `docs/install/` for the full installation, upgrade, and
clean-install runbook, and `test/packaging/` for the verification
suite that asserts these invariants on every change.
