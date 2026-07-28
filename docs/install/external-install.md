# TensorPlate External Install Guide (GitHub-assets fallback)

The preferred install is the APT channel — see
[`tensorplate-ready.md`](./tensorplate-ready.md) for the two-command
install and the one-time stock-machine bootstrap. This guide is the
supported no-APT fallback starting from GitHub Release assets. It does
not require a TensorPlate source checkout or local build tree. The
fallback path is the release `install.sh` script: download it and run it
locally. The script self-checks against release `SHA256SUMS` before
downloading or installing package assets. Do not use `curl | sh`.

The concrete command below targets `v0.1.1`; later releases use the same
shape with the tag changed in the URL.

## Supported Target

Runtime install:

| Requirement | Release support |
| --- | --- |
| Hardware | Jetson Orin Nano 8GB Super hardware floor. |
| OS | JetPack 6.x with L4T 36.x. |
| Architecture | `arm64`. |
| Package source | `.deb` files attached to the matching GitHub Release. |
| Network posture | Services default to local-only endpoints. |

Best-effort validation may run on Jetson Orin NX 16GB. Kria K26/K24,
Vitis AI execution, hosted fleet control, container-only install, and
public network endpoints are not supported for the v0.1 release line.

CLI-only install:

| Requirement | Release support |
| --- | --- |
| Host | Debian/Ubuntu desktop or workstation. |
| Architecture | Any architecture with a matching `tensorplate-cli` release asset. |
| Package source | `tensorplate-common` plus `tensorplate-cli` `.deb` files attached to the matching GitHub Release. |
| Runtime services | Not installed or enabled. |

## Runtime Install On Jetson

For TensorPlate `v0.1.1`, paste this into the terminal on the target
Jetson:

```bash
curl -fLO https://github.com/tensorplate/tensorplate/releases/download/v0.1.1/install.sh && sudo bash install.sh
```

This downloads the installer and runs it locally. The installer verifies a
keyless cosign signature over `SHA256SUMS`, then verifies `install.sh` and
the selected release packages before installing. It is intentionally not a
`curl | sh` path. If `cosign` is not already installed, the installer
downloads a pinned Linux cosign binary to its temporary work directory,
verifies its pinned SHA256, and runs it from there on Linux `arm64` and
`amd64`. Pass `--allow-unsigned` only to accept checksum-only integrity at
your own risk.

The script uses the pinned current release by default. For a newer
published release, replace `v0.1.1` in the URL with that release tag.

What the installer does:

- validates the host OS as JetPack 6.x / L4T 36.x and aborts by default
  on unsupported OS metadata.
- warns on unrecognized Jetson hardware or non-`arm64` architecture; in
  interactive mode it prompts before continuing.
- downloads `tensorplate-${TP_TAG}-artifacts.json`, `SHA256SUMS`,
  `SHA256SUMS.cosign.bundle`, and the selected release artifacts listed in
  the manifest.
- verifies a keyless cosign signature over `SHA256SUMS` against the
  TensorPlate release workflow identity before trusting it. If `cosign` is
  not already installed, the installer downloads and checksum-verifies a
  pinned transient cosign binary first on Linux `arm64` and `amd64`.
- verifies the downloaded manifest and artifacts with `sha256sum -c
  SHA256SUMS`; any mismatch stops the install.
- installs the core Debian packages with the `apt-get install --reinstall`
  path.
- enables `tensorplate-agent` and `tensorplate-observability`.
- runs `tensorplate doctor --output json` and fails if a critical finding
  is present.

Installer flags:

| Flag | Behavior |
| --- | --- |
| `--version VERSION` | Selects the release tag/version. Accepts `0.1.1`, `v0.1.1`, or `v0.1.1-rc.N`. |
| `--cli-only` | Installs only `tensorplate-common` and `tensorplate-cli` for the host Debian architecture. Skips Jetson validation, service enablement, and doctor. |
| `--with-python-backend` | Also installs `tensorplate-backend-python-pytorch`. |
| `--yes` / `-y` | Continues without interactive prompts for unattended provisioning. |
| `--force-os` | Overrides the OS gate. This is unsupported and at your own risk. |
| `--strict-hardware` | Treats advisory hardware warnings as fatal. |
| `--dry-run` | Runs validation gates and prints planned actions without downloading or installing. |
| `--local-artifacts DIR` | Installs from a locally built artifact directory containing the installer, manifest, `SHA256SUMS`, and `.deb` packages. Intended for unreleased snapshot/build-only artifacts. |
| `--allow-unsigned` | Skips the cosign signature check on `SHA256SUMS` and accepts checksum-only integrity. Unsupported; for air-gapped or bootstrap use at your own risk. |
| `--help` | Prints usage and all flags. |

For fleet provisioning on validated Jetson hosts:

```bash
sudo bash install.sh --yes
```

Install the optional Python/PyTorch backend package with:

```bash
sudo bash install.sh --with-python-backend
```

The optional package does not install PyTorch. Install the
Jetson-compatible PyTorch stack separately, then run `tensorplate doctor`
and restart the agent if needed.

## CLI-Only Install On Desktop

On a Debian/Ubuntu desktop or workstation, paste:

```bash
curl -fLO https://github.com/tensorplate/tensorplate/releases/download/v0.1.1/install.sh && sudo bash install.sh --cli-only
```

CLI-only mode installs the operator CLI package for the host Debian
architecture when the release includes a matching `tensorplate-cli`
asset. It does not install Jetson runtime services, does not validate
JetPack / L4T, and does not run `tensorplate doctor`.

## Build And Install An Unreleased Branch

Use this only before a GitHub Release exists, or when validating a branch
before release. On a Jetson with the native build dependencies installed,
paste:

```bash
curl -fL https://raw.githubusercontent.com/tensorplate/tensorplate/develop/packaging/scripts/build-install-from-source.sh -o build-install-from-source.sh && sudo bash build-install-from-source.sh --branch develop
```

From an existing TensorPlate checkout, the equivalent command is:

```bash
sudo bash packaging/scripts/build-install-from-source.sh --branch develop
```

The wrapper clones or checks out the requested branch, builds snapshot
packages named with `X.Y.Z~dev.YYYYMMDD.gitsha`, generates a local
manifest and `SHA256SUMS`, verifies them with
`tools/release/tensorplate-release.sh`, then invokes the same
`install.sh` path with `--local-artifacts --allow-unsigned`. There is no
second installer implementation.

Useful source-install flags:

| Flag | Behavior |
| --- | --- |
| `--branch BRANCH` | Branch, tag, or ref to build. Defaults to `develop`. |
| `--with-python-backend` | Installs the optional Python/PyTorch backend after building. Runtime install mode only. |
| `--cli-only` | Installs only `tensorplate-common` and `tensorplate-cli` from the built snapshot artifacts. |
| `--no-install` | Builds and verifies artifacts only, then prints the local install command. |
| `--arch ARCH` | Debian target architecture. Defaults to `arm64`. |
| `--source-dir DIR` | Uses an existing checkout instead of cloning or creating a temporary checkout. |
| `--artifacts-dir DIR` | Writes snapshot artifacts to a specific directory. |
| `--help` | Prints usage and all flags. |

Snapshot caveats:

- Snapshot artifacts are unreleased and unsigned. The installer verifies
  local checksums, but there is no GitHub Release, release workflow
  cosign identity, or GitHub attestation provenance.
- Snapshot installs are not release-validation evidence. Run the release
  workflow build-only path and the clean-room Jetson validation before
  treating a version as production-ready.
- Native Jetson builds avoid cross-compile setup. Cross-compiling from
  x86 to Jetson requires a Jetson sysroot, `TP_JETSON_CC`,
  `TP_JETSON_CXX`, and a vcpkg toolchain; the builder chainloads
  `cmake/toolchains/aarch64-jetson.cmake`.
- The command builds full local packages, so it needs the same compiler,
  Rust, CMake/Ninja, debhelper, and target SDK dependencies as release
  artifact builds.

## Manual Fallback Variables

Use these variables only when following the manual fallback flow below:

```bash
export TP_VERSION=0.1.1
export TP_TAG="v${TP_VERSION}"
export TP_DEBIAN_VERSION="${TP_VERSION}-1"
export TP_ARCH=arm64
export TP_REPO=tensorplate/tensorplate
export TP_RELEASE_URL="https://github.com/${TP_REPO}/releases/download/${TP_TAG}"
export TP_MANIFEST="tensorplate-${TP_TAG}-artifacts.json"
```

Use one shell session for the commands below so these variables remain in
scope.

## Manual Fallback: Download Release Assets

```bash
mkdir -p "/tmp/tensorplate-${TP_TAG}"
cd "/tmp/tensorplate-${TP_TAG}"

curl -fL -O "${TP_RELEASE_URL}/install.sh"
curl -fL -O "${TP_RELEASE_URL}/tensorplate-common_${TP_DEBIAN_VERSION}_all.deb"
curl -fL -O "${TP_RELEASE_URL}/tensorplate-agent_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb"
curl -fL -O "${TP_RELEASE_URL}/tensorplate-serving_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb"
curl -fL -O "${TP_RELEASE_URL}/tensorplate-observability_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb"
curl -fL -O "${TP_RELEASE_URL}/tensorplate-cli_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb"
curl -fL -O "${TP_RELEASE_URL}/tensorplate-backend-python-pytorch_${TP_DEBIAN_VERSION}_all.deb"
curl -fL -O "${TP_RELEASE_URL}/${TP_MANIFEST}"
curl -fL -O "${TP_RELEASE_URL}/SHA256SUMS"
curl -fL -O "${TP_RELEASE_URL}/SHA256SUMS.cosign.bundle"
```

Do not use `curl | sh` install paths. Download, inspect, and verify the
assets first.

## Verify Signature And Checksums

Verify the cosign signature over `SHA256SUMS` first (authenticity), then the
checksums (integrity). This requires the `cosign` binary; install it from
<https://docs.sigstore.dev/cosign/installation>.

```bash
cosign verify-blob \
  --bundle SHA256SUMS.cosign.bundle \
  --certificate-identity-regexp "^https://github.com/${TP_REPO}/\.github/workflows/release\.yml@refs/tags/v[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  SHA256SUMS

sha256sum -c SHA256SUMS
```

The signature must report `Verified OK` and every asset must report `OK`.
Stop if either fails. Optionally confirm build provenance for a package:

```bash
gh attestation verify "tensorplate-agent_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb" \
  --repo "${TP_REPO}"
```

## Install Core Packages

```bash
sudo apt update
sudo apt install "./tensorplate-common_${TP_DEBIAN_VERSION}_all.deb" \
  "./tensorplate-agent_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb" \
  "./tensorplate-serving_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb" \
  "./tensorplate-observability_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb" \
  "./tensorplate-cli_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb"
```

The install creates the `tensorplate` system user and group, lays out
`/etc/tensorplate`, `/var/lib/tensorplate`, `/var/log/tensorplate`,
`/run/tensorplate`, `/usr/share/tensorplate/backends`, and
`/usr/share/tensorplate/platform`, installs
systemd units for the agent and observability service, and installs the
serving worker without a standalone serving service.

## Run Doctor Before Starting Services

```bash
tensorplate doctor
```

Expected profile before services start:

- install layout and config findings are `ok`.
- `agent_service_state` and `observability_service_state` may be warning
  until the units are enabled.
- `agent_reachable` may fail until the agent is started.
- `python_pytorch_backend` may be missing until the optional backend
  package is installed.

Stop if install layout, config files, core package, systemd unit, or
serving binary findings fail.

## Start Services

```bash
sudo systemctl enable --now tensorplate-agent
sudo systemctl enable --now tensorplate-observability
tensorplate doctor
```

After startup, `agent_reachable`, `agent_state`, and service-state
findings should be `ok`.

## Optional Python/PyTorch Backend

Install the optional package when validating or running a
`python_pytorch` bundle:

```bash
sudo apt install "./tensorplate-backend-python-pytorch_${TP_DEBIAN_VERSION}_all.deb"
```

This package installs the TensorPlate backend entrypoint and descriptor.
It does not install PyTorch. Install the Jetson-compatible PyTorch stack
for the descriptor's Python interpreter, then restart the agent:

```bash
tensorplate doctor
sudo systemctl restart tensorplate-agent
```

See [`python-pytorch-backend.md`](./python-pytorch-backend.md) for the
runtime probe behavior and troubleshooting.

## Verify Local-Only Endpoint Defaults

Confirm the agent Unix socket and service state:

```bash
test -S /run/tensorplate/agent.sock
systemctl status tensorplate-agent --no-pager
systemctl status tensorplate-observability --no-pager
```

Do not expose serving or agent endpoints beyond the device for the v0.1
release line.

## Upgrade And Reinstall

For reinstall of the selected release:

```bash
sudo apt install --reinstall "./tensorplate-common_${TP_DEBIAN_VERSION}_all.deb" \
  "./tensorplate-agent_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb" \
  "./tensorplate-serving_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb" \
  "./tensorplate-observability_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb" \
  "./tensorplate-cli_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb"
```

Reinstall preserves `/etc/tensorplate`, `/var/lib/tensorplate`, and logs.
Future upgrades must pass the package upgrade preflight described in
[`lifecycle.md`](./lifecycle.md).

## Uninstall

Remove packages but preserve state:

```bash
sudo systemctl stop tensorplate-agent tensorplate-observability
sudo apt remove tensorplate-agent tensorplate-serving tensorplate-observability tensorplate-cli tensorplate-backend-python-pytorch
```

Purge package-managed config and state when intentionally resetting the
device:

```bash
sudo apt purge tensorplate-agent tensorplate-serving tensorplate-observability tensorplate-cli tensorplate-common tensorplate-backend-python-pytorch
```

The package lifecycle does not delete the `tensorplate` system user and
group.

## Troubleshooting

| Symptom | Action |
| --- | --- |
| Checksum mismatch | Delete the asset and download again. If it repeats, stop and file a release issue. |
| Signature verification failed | Stop. Re-download `SHA256SUMS` and `SHA256SUMS.cosign.bundle`; if it repeats, do not install and report it privately per [`SECURITY.md`](../../SECURITY.md). |
| Cosign bootstrap failed | Install cosign from <https://docs.sigstore.dev/cosign/installation> and rerun, or pass `--allow-unsigned` only for air-gapped/bootstrap installs at your own risk. |
| Unsupported OS | The installer aborts by default unless `--force-os` is passed. v0.1 supports JetPack 6.x / L4T 36.x. |
| Unsupported hardware or architecture | The installer warns and prompts in interactive mode. Use `--strict-hardware` to make this fatal, or `--yes` for unattended validated fleets. |
| `path_layout = fail` | Reinstall `tensorplate-common`; attach `tensorplate doctor --output json` if it persists. |
| `config_files = fail` | Reinstall the owning package or restore the dpkg conffile. |
| Service start failure | Inspect `journalctl -u tensorplate-agent` or `journalctl -u tensorplate-observability`; do not edit `/usr/lib/tensorplate` manually. |
| Endpoint bind failure | Check for stale sockets or non-default config under `/etc/tensorplate`. Defaults must remain local-only. |
| Package version mismatch | Install all core packages from the same GitHub Release and verify `SHA256SUMS` again. |
| Python backend missing | Install `tensorplate-backend-python-pytorch`, install the platform PyTorch stack, run doctor, then restart the agent. |
