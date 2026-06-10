# TensorPlate APT bootstrap payloads

This directory owns the payloads shipped by the `tensorplate-apt-source`
bootstrap package. Installing that package is **one-time APT source setup,
not a TensorPlate runtime installation**: it configures where APT discovers
TensorPlate packages and nothing else.

| File | Installed to | Purpose |
| --- | --- | --- |
| `tensorplate.sources` | `/etc/apt/sources.list.d/tensorplate.sources` | Deb822 source for the stable repository `https://packages.tensorplate.com/apt` (`jammy/main`, `amd64`+`arm64`). Never references a runtime version, so future releases (0.1.x, 0.2.x, …) are discovered through a normal `apt update`. |
| `tensorplate-archive-keyring.asc` | `/usr/share/keyrings/tensorplate-archive-keyring.gpg` (dearmored at package build time by `debian/rules`) | Public key that APT uses to verify the repository's signed metadata (`Signed-By`). The armored form is committed so key changes are reviewable in diffs; `gpg --dearmor` is deterministic. |

## What the package does and does not do

- Installs exactly the two files above. The source file is a conffile, so
  operator edits survive upgrades.
- The `postinst` fails closed if either file is missing or empty.
- It does **not** install runtime packages, start services, mutate
  TensorPlate runtime state, or run `apt update`. No TensorPlate runtime
  package depends on it.

TensorPlate-ready Jetson images ship with the package preinstalled; stock
Ubuntu/Jetson systems install it once (from GitHub Release assets) as the
bootstrap step before `sudo apt update && sudo apt install tensorplate`.

## Staging placeholder key

The committed keyring is currently a clearly-labeled **STAGING PLACEHOLDER**
generated for build/test wiring. The production archive signing key is
provisioned by the signed-APT-repository work (issue #41); swapping it in
is a one-file change to `tensorplate-archive-keyring.asc`.

`tools/release/build-release-artifacts.sh` refuses to build publish-grade
release artifacts while the placeholder marker is present, so a release
cannot ship the placeholder trust root by accident. Key **rotation** is a
follow-up runbook owned by the release docs; v0.1.2 does not rotate keys.

## Validation

- Static checks: `test/packaging/verify_apt_source.sh` (part of
  `test/packaging/run.sh`).
- Build/install integration: build the arch-independent packages with
  `packaging/scripts/build-deb.sh -A`, then install the
  `tensorplate-apt-source` `.deb` in a clean Ubuntu 22.04 container and
  verify both paths exist and `apt-get update --print-uris` resolves
  `packages.tensorplate.com/apt` URIs.
