# TensorPlate APT repository publication

The stable APT channel at `https://packages.tensorplate.com/apt` serves the
`jammy/main` suite for `arm64` and `amd64`. Installed hosts trust it through
the keyring shipped by `tensorplate-apt-source`
(`/usr/share/keyrings/tensorplate-archive-keyring.gpg`); the repository
publishes signed `InRelease`, `Release`, and `Release.gpg` metadata plus
per-architecture `Packages`/`Packages.gz` indexes over an accumulating
`pool/`, so future releases are discovered through a normal `apt update`
against the same URL.

## How publication runs

[` .github/workflows/apt-repo.yml`](../../.github/workflows/apt-repo.yml)
builds, signs, and syncs the repository. It runs:

- **Automatically** when a final GitHub Release is published (the
  `release: released` event — fired when the draft created by the release
  workflow is made public). Release candidates never reach the stable
  channel: GitHub does not emit `released` for prereleases and the workflow
  additionally rejects non-`vX.Y.Z` tags.
- **Manually** via `workflow_dispatch` with a published tag, to republish
  repository metadata without rebuilding packages, or with a `dest`
  override to publish to a staging bucket.

The heavy lifting is
[`tools/release/publish-apt-repo.sh`](../../tools/release/publish-apt-repo.sh),
which is destination-agnostic and locally testable. It fails closed at
every step:

1. Release assets must match `SHA256SUMS`, every `.deb` must be listed,
   and the cosign bundle for `SHA256SUMS` must verify against the release
   workflow identity (waivable only with `--allow-unverified-assets`, for
   staging/container tests).
2. The previous `pool/` is carried forward (`--existing-pool`), and a pool
   file name may never change contents — rebuilds must bump the version.
3. Generated metadata must verify (`gpgv`) against the public keyring the
   bootstrap package ships, so CI can never publish metadata installed
   hosts cannot validate — including signing with the wrong key.

## Configuration contract

| Kind | Name | Meaning |
| --- | --- | --- |
| variable | `TP_APT_REPO_DEST` | Production destination, `s3://bucket/prefix`. The job skips (does not fail) while unset. |
| variable | `TP_APT_AWS_REGION` | Optional region; defaults to `us-east-1`. |
| variable | `TP_APT_S3_ENDPOINT` | Optional endpoint URL for S3-compatible storage (Cloudflare R2, MinIO, …). |
| secret | `TP_APT_AWS_ACCESS_KEY_ID` / `TP_APT_AWS_SECRET_ACCESS_KEY` | Credentials scoped to the repository bucket only. |
| secret | `TP_APT_SIGNING_KEY` | Armored OpenPGP **private** archive signing key. |
| secret | `TP_APT_SIGNING_KEY_PASSPHRASE` | Optional passphrase for the signing key. |

The CDN/site in front of the bucket must serve the bucket contents under
`https://packages.tensorplate.com/apt/`. No CDN invalidation hook is
required: the sync sets `Cache-Control: public, max-age=31536000, immutable`
on `pool/` (package files never change contents) and
`public, max-age=60` on `dists/` (metadata refreshes within a minute).
Sync order is always `pool/` before `dists/` so live metadata never
references a missing package.

## Provisioning the production signing key (one-time)

The committed
[`packaging/apt/tensorplate-archive-keyring.asc`](../../packaging/apt/README.md)
is a staging placeholder until this procedure is done. Release builds
refuse to ship the placeholder, and the publish script refuses metadata
that the shipped keyring cannot verify — both gates clear only after this
swap.

On a trusted offline host:

```bash
export GNUPGHOME="$(mktemp -d)"
gpg --batch --pinentry-mode loopback --passphrase '' --quick-generate-key \
  "TensorPlate APT Archive Signing Key <packages@tensorplate.com>" ed25519 sign never
gpg --armor --export packages@tensorplate.com  > tensorplate-archive-keyring.asc
gpg --armor --export-secret-keys packages@tensorplate.com > apt-signing-key.private.asc
```

1. Store `apt-signing-key.private.asc` as the `TP_APT_SIGNING_KEY` GitHub
   secret (and in the organization's offline key escrow). It must never be
   committed or copied to developer machines.
2. Replace `packaging/apt/tensorplate-archive-keyring.asc` with the new
   public key in a reviewed PR (one-file diff; the packaging suite and the
   release-build placeholder gate validate the swap).
3. Delete `$GNUPGHOME` and the local private key copy.

Key **rotation** is intentionally out of scope for v0.1.2; a rotation
runbook (new key shipped alongside the old one in the bootstrap package
before metadata switches over) is follow-up work.

## Staging publication

Run the `APT Repository` workflow manually with `dest` pointed at a
staging bucket (for example `s3://tensorplate-apt-staging/apt`), fronted by
any host. Validate from clean hosts with a sources file pointing at the
staging URL. The same script also runs fully locally — see the container
recipe in `test/packaging`'s style: build packages, generate `SHA256SUMS`,
sign with an ephemeral key, and point a Deb822 `file:` source at the
output (this is exercised in CI evidence for the publication PR).

## Validation checklist (per publication)

- [ ] `sudo apt update` on a clean configured host succeeds with **no**
      trust warnings (`NO_PUBKEY`, "is not signed", insecure-repository).
- [ ] `apt-cache policy tensorplate` shows the released version as
      candidate on Jetson arm64; `tensorplate-cli` on amd64.
- [ ] A host installed from the previous release sees the new version via
      plain `apt update` (no bootstrap reinstall).
- [ ] `gpgv --keyring /usr/share/keyrings/tensorplate-archive-keyring.gpg
      InRelease` passes for the published `InRelease`.
