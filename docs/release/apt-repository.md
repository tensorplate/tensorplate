# TensorPlate APT repository

The stable TensorPlate APT channel:

| | |
| --- | --- |
| URL | `https://packages.tensorplate.com/apt` |
| Suite / component | `jammy` / `main` |
| Architectures | `arm64` (Jetson runtime + CLI), `amd64` (Ubuntu x86_64 runtime + CLI) |

Hosts configure it once through the `tensorplate-apt-source` bootstrap
package (archive keyring + Deb822 source file); after that, new TensorPlate
releases are discovered through a normal `apt update` against the same URL.
No runtime version is ever encoded in the repository path.

## Trust model

- Repository metadata (`InRelease`, `Release` + `Release.gpg`) is signed
  with the TensorPlate archive key. Clients verify it with the keyring
  installed at `/usr/share/keyrings/tensorplate-archive-keyring.gpg` via
  Deb822 `Signed-By`; `apt-key` is never used.
- The repository is generated exclusively from GitHub Release assets that
  pass `SHA256SUMS` verification and a cosign signature check binding them
  to the release workflow identity.
- Publication fails closed: if generated metadata does not verify against
  the shipped keyring, if any package listed in `SHA256SUMS` is missing
  (no partial publications), or if a pool file would change contents under
  the same name (released bytes are immutable; fixes bump versions).
- Only final `vX.Y.Z` releases are published. Release candidates never
  reach the stable channel.

## How publication runs

When a final GitHub Release is published,
[`.github/workflows/apt-repo.yml`](../../.github/workflows/apt-repo.yml)
builds and signs the repository tree with
[`tools/release/publish-apt-repo.sh`](../../tools/release/publish-apt-repo.sh)
and syncs it to object storage — `pool/` before `dists/`, so live metadata
never references a missing package. Maintainers can republish the
repository from the existing signed release assets at any time;
provisioning and operational details live in the maintainer release-ops
runbook.

## Recovery

GitHub Release assets remain the checksum-covered, cosign-signed fallback
artifact source. The repository is fully reproducible from them, so loss
of the hosting bucket is recoverable without rebuilding any package.

## Validation checklist (per publication)

- [ ] `sudo apt update` on a clean configured host succeeds with **no**
      trust warnings (`NO_PUBKEY`, "is not signed", insecure-repository).
- [ ] `apt-cache policy tensorplate` shows the released version as
      candidate on both Jetson arm64 and Ubuntu x86_64. The metapackage is
      built per runtime architecture, so checking only one leaves the other
      architecture's channel unverified.
- [ ] A host installed from the previous release sees the new version via
      plain `apt update` (no bootstrap reinstall).
- [ ] `gpgv --keyring /usr/share/keyrings/tensorplate-archive-keyring.gpg
      InRelease` passes for the published `InRelease`.
