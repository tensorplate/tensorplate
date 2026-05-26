# TensorPlate release artifacts, manifest, and publication flow

TensorPlate public releases publish native Debian package artifacts plus a
manifest and SHA256 checksums. Release pages may also attach validated
sample bundles, but the required assets are the `.deb` packages, artifact
manifest, and `SHA256SUMS`.

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

Expected asset names follow Debian binary-package naming:

```text
tensorplate-common_${TP_VERSION}-1_arm64.deb
tensorplate-agent_${TP_VERSION}-1_arm64.deb
tensorplate-serving_${TP_VERSION}-1_arm64.deb
tensorplate-observability_${TP_VERSION}-1_arm64.deb
tensorplate-cli_${TP_VERSION}-1_arm64.deb
tensorplate-backend-python-pytorch_${TP_VERSION}-1_arm64.deb
tensorplate-${TP_TAG}-artifacts.json
SHA256SUMS
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
to build `arm64` release packages on a non-`arm64` runner.

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
    }
  ]
}
```

`SHA256SUMS` uses standard two-column format:

```text
<sha256>  <artifact-file-name>
```

External users verify downloads with:

```bash
sha256sum -c SHA256SUMS
```

On macOS verification hosts, use `shasum -a 256 -c SHA256SUMS`.

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
checksum mismatches.

## GitHub Release Attachment Procedure

1. Cut the annotated source tag with `tools/release/tensorplate-release.sh cut`.
2. Push the reviewed release branch and tag.
3. Let the `Release` workflow build all `.deb` packages, manifest, and
   `SHA256SUMS`.
4. Let the workflow create the GitHub Release and attach all assets. RC
   tags publish as public prereleases; final tags create draft releases.
5. Confirm the release notes include supported hardware/OS, known
   limitations, install guide, validation links, support policy, security
   policy, and rollback guidance.
6. Run clean-room validation from the GitHub Release assets, not from local
   package build paths.
7. Publish the final draft, then announce the release, only after the
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
