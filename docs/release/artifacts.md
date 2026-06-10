# TensorPlate release artifacts, manifest, and publication flow

TensorPlate public releases publish native Debian package artifacts,
`install.sh`, an artifact manifest, SHA256 checksums, and a cosign
signature over those checksums. Release pages may also attach validated
sample bundles, but the required assets are the `.deb` packages, installer,
artifact manifest, `SHA256SUMS`, and `SHA256SUMS.cosign.bundle`.
Releases may additionally attach desktop-only `tensorplate-cli` packages
for architectures such as `amd64`; those assets are consumed by
`install.sh --cli-only` and are not part of the Jetson runtime package set.

Use these variables in examples:

```bash
export TP_VERSION=0.1.0
export TP_TAG="v${TP_VERSION}"
export TP_RELEASE_DIR="dist/release/${TP_TAG}"
export TP_MANIFEST="${TP_RELEASE_DIR}/tensorplate-${TP_TAG}-artifacts.json"
export TP_CHECKSUMS="${TP_RELEASE_DIR}/SHA256SUMS"
```

## Required Packages

The release must include one `.deb` asset for each package:

| Package | Required | Notes |
| --- | --- | --- |
| `tensorplate-common` | yes | Shared users, paths, and maintainer-script helpers. |
| `tensorplate-agent` | yes | Device control plane and worker supervision service. |
| `tensorplate-serving` | yes | Serving worker binary, supervised by the agent. |
| `tensorplate-observability` | yes | Independent status and metrics monitor service. |
| `tensorplate-cli` | yes | Operator CLI binary `tensorplate`. |
| `tensorplate-backend-python-pytorch` | yes | Optional backend package. It does not vendor PyTorch. |
| `tensorplate-apt-source` | yes | One-time APT source bootstrap: archive keyring + stable Deb822 source. Installs no runtime component. |
| `tensorplate` | yes | Jetson full-runtime metapackage (arm64 only); empty payload, strict-versioned depends on the runtime set. |

Expected asset names follow Debian binary-package naming:

```text
tensorplate-common_${TP_VERSION}-1_all.deb
tensorplate-agent_${TP_VERSION}-1_arm64.deb
tensorplate-serving_${TP_VERSION}-1_arm64.deb
tensorplate-observability_${TP_VERSION}-1_arm64.deb
tensorplate-cli_${TP_VERSION}-1_arm64.deb
tensorplate-backend-python-pytorch_${TP_VERSION}-1_all.deb
tensorplate-apt-source_${TP_VERSION}-1_all.deb
tensorplate_${TP_VERSION}-1_arm64.deb
tensorplate-cli_${TP_VERSION}-1_amd64.deb    # CLI-only desktop asset (required for publish-grade builds)
install.sh
tensorplate-${TP_TAG}-artifacts.json
SHA256SUMS
SHA256SUMS.cosign.bundle
```

## Build Flow

The normal release build runs in `.github/workflows/release.yml` after
the maintainer pushes an annotated release tag. The workflow invokes:

```bash
tools/release/build-release-artifacts.sh \
  --version "${TP_VERSION}" \
  --tag "${TP_TAG}" \
  --artifacts-dir "${TP_RELEASE_DIR}" \
  --manifest "${TP_MANIFEST}" \
  --checksums "${TP_CHECKSUMS}" \
  --arch arm64
```

The release runner must match the target architecture. The script refuses
to build `arm64` release packages on a non-`arm64` runner. If a separately
built desktop `tensorplate-cli` package, such as `amd64`, is pre-staged in
the repository parent directory with the other Debian outputs, the release
build copies it into the artifact set and the manifest marks it as a
desktop CLI asset.

For local diagnostics only, the equivalent steps are:

```bash
cargo build --release --bin tensorplate-agent --bin tensorplate-observability --bin tensorplate
cmake -S . -B build/release -G Ninja -DTP_BUILD_TESTS=OFF -DTP_BUILD_EXAMPLES=OFF
cmake --build build/release --target tp_serving_worker
./packaging/scripts/build-deb.sh
```

Do not attach local diagnostic builds to a public GitHub Release.

## Manifest Format

Generate manifest and checksums:

```bash
tools/release/tensorplate-release.sh manifest \
  --version "${TP_VERSION}" \
  --tag "${TP_TAG}" \
  --artifacts-dir "${TP_RELEASE_DIR}" \
  --manifest "${TP_MANIFEST}" \
  --checksums "${TP_CHECKSUMS}"
```

The manifest is JSON with this stable shape:

```json
{
  "schema": "https://tensorplate.com/schemas/release-artifact-manifest-v1.json",
  "release": {
    "project": "tensorplate",
    "version": "X.Y.Z",
    "tag": "vX.Y.Z",
    "commit": "<git-sha>",
    "branch": "release/vX.Y.Z",
    "generated_at_utc": "YYYY-MM-DDTHH:MM:SSZ"
  },
  "target": {
    "hardware_floor": "Jetson Orin Nano 8GB Super",
    "os": "Ubuntu 22.04 / JetPack 6.x (L4T 36.x)",
    "architecture": "arm64"
  },
  "validation": {
    "gate_report": "docs/validation/orin-release-validation.md",
    "clean_room_report": "dist/release/vX.Y.Z/clean-room.md"
  },
  "artifacts": [
    {
      "file": "tensorplate-cli_X.Y.Z-1_arm64.deb",
      "package": "tensorplate-cli",
      "version": "X.Y.Z-1",
      "architecture": "arm64",
      "target_os": "Ubuntu 22.04 / JetPack 6.x (L4T 36.x)",
      "size_bytes": 123,
      "sha256": "<64-hex-digest>"
    },
    {
      "file": "install.sh",
      "kind": "installer",
      "version": "X.Y.Z",
      "target_os": "Ubuntu 22.04 / JetPack 6.x (L4T 36.x)",
      "size_bytes": 123,
      "sha256": "<64-hex-digest>"
    }
  ]
}
```

`SHA256SUMS` uses standard two-column format. It covers the manifest file
itself plus every file listed in the manifest, including `install.sh`:

```text
<sha256>  tensorplate-vX.Y.Z-artifacts.json
<sha256>  <artifact-file-name>
```

External users verify downloads with:

```bash
sha256sum -c SHA256SUMS
```

On macOS verification hosts, use `shasum -a 256 -c SHA256SUMS`.
The primary `install.sh` path also performs checksum verification itself:
it verifies `install.sh` first, then verifies the manifest and selected
package assets before installing.

## Verification

After the annotated tag exists locally:

```bash
tools/release/tensorplate-release.sh verify \
  --version "${TP_VERSION}" \
  --tag "${TP_TAG}" \
  --artifacts-dir "${TP_RELEASE_DIR}" \
  --manifest "${TP_MANIFEST}" \
  --checksums "${TP_CHECKSUMS}"
```

Verification fails if the tag is not annotated, a required package is
missing, manifest metadata drifts from the requested version/tag, or any
checksum mismatches. The required runtime package set must include the
target architecture or `all`; additional architecture-specific `.deb`
assets are currently allowed only for `tensorplate-cli`.

## Signing and Provenance

On the publish path, the `Release` workflow signs and attests the assets so
consumers can verify authenticity, not just integrity:

- It signs `SHA256SUMS` with keyless [cosign](https://docs.sigstore.dev/cosign/installation)
  (GitHub Actions OIDC) and attaches the self-contained Sigstore bundle as
  `SHA256SUMS.cosign.bundle`.
- It records SLSA build provenance for every `.deb`, `install.sh`, the
  manifest, and `SHA256SUMS` with
  [`actions/attest-build-provenance`](https://github.com/actions/attest-build-provenance).

Both bind the assets to the release workflow identity on the `vX.Y.Z[-rc.N]`
tag. Build-only validation runs (`publish=false`) do not sign or attest, so
their workflow-artifact bundle is unsigned and the installer smoke test must
use `--allow-unsigned`. Consumers verify with `cosign verify-blob` and
`gh attestation verify` as documented in
[`SECURITY.md`](../../SECURITY.md) and the external install guide.

## GitHub Release Attachment Procedure

1. Cut the local annotated source tag with `tools/release/tensorplate-release.sh cut`.
2. Push only the reviewed release branch.
3. For pre-publication validation, run the `Release` workflow manually
   with `publish=false`. It builds all `.deb` packages, copies
   `install.sh`, generates the manifest and `SHA256SUMS`, uploads the
   `tensorplate-${TP_TAG}-release-assets` workflow artifact, and stops
   before creating a GitHub Release.
4. Download that workflow artifact on the validation target and exercise
   the installer locally. Build-only assets are unsigned, so pass
   `--allow-unsigned`: `sudo bash install.sh --allow-unsigned`, `sudo bash
   install.sh --cli-only --allow-unsigned` when a desktop CLI asset is
   present, and any optional backend path being released.
5. Push the annotated tag to sign `SHA256SUMS`, record build provenance,
   create the GitHub Release, and attach all assets including
   `SHA256SUMS.cosign.bundle`. Manual `workflow_dispatch publish=true` is
   allowed only when the workflow itself is run from that release tag ref.
   RC tags publish as public prereleases; final tags create draft releases.
6. Confirm the release notes include supported hardware/OS, known
   limitations, install guide, validation links, support policy, security
   policy, and rollback guidance.
7. Run clean-room validation from the GitHub Release assets, not from local
   package build paths.
8. Publish the final draft, then announce the release, only after the
   clean-room decision is `pass` or an explicitly signed
   `conditional-pass`.

The manual `publish` subcommand is retained as a fallback for maintainers
when CI publication is unavailable:

```bash
tools/release/tensorplate-release.sh publish \
  --version "${TP_VERSION}" \
  --tag "${TP_TAG}" \
  --artifacts-dir "${TP_RELEASE_DIR}" \
  --manifest "${TP_MANIFEST}" \
  --checksums "${TP_CHECKSUMS}" \
  --release-notes "docs/release/notes/${TP_TAG}.md" \
  --dry-run
```

Use `publish --execute --confirm "PUBLISH-${TP_TAG}"` only after the
dry-run command has been reviewed. The script creates a draft release; it
never force-pushes, deletes tags, rewrites tags, or silently replaces
final assets.
