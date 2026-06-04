# TensorPlate release runbook

This runbook owns the public release process for TensorPlate versioned
releases. It is reusable across patch, minor, and later release lines. Set
the release variables first; examples below use `0.1.0`.

```bash
export TP_VERSION=0.1.0
export TP_TAG="v${TP_VERSION}"
export TP_RELEASE_BRANCH="release/${TP_TAG}"
export TP_RELEASE_DIR="dist/release/${TP_TAG}"
export TP_MANIFEST="${TP_RELEASE_DIR}/tensorplate-${TP_TAG}-artifacts.json"
export TP_CHECKSUMS="${TP_RELEASE_DIR}/SHA256SUMS"
export TP_SIGNOFF="${TP_RELEASE_DIR}/signoff.md"
export TP_PREFLIGHT="${TP_RELEASE_DIR}/preflight.md"
export TP_RELEASE_NOTES="docs/release/notes/${TP_TAG}.md"
```

The release has two separated phases:

1. Add or update release tooling/docs in a normal implementation PR.
2. After that PR merges, cut and publish the release from a clean release
   branch and annotated tag.

The tooling PR is not the release.

## Required Owners

Record sign-offs in a copy of
[`signoff-template.md`](./signoff-template.md).

| Role | Responsibility |
| --- | --- |
| Release owner | Drives the release branch, tag, GitHub Release, and evidence archive. |
| Runtime reviewer | Reviews C++ runtime, serving worker, version surfaces, and adapter risk. |
| Agent CLI reviewer | Reviews agent, CLI, deploy, rollback, doctor, and status/log command posture. |
| Packaging reviewer | Reviews `.deb` artifacts, package metadata, maintainer scripts, and checksums. |
| Validation reviewer | Reviews release-gate and clean-room evidence. |
| Security reviewer | Reviews `SECURITY.md`, local endpoint posture, and advisory risk. |
| Docs reviewer | Reviews install guide, quickstart, release notes, and known limitations. |

## Implementation PR

The implementation PR may add or update release tooling, release docs,
install docs, validation procedure, support posture, and changelog notes.

Allowed validation:

```bash
tools/release/tensorplate-release.sh --help
tools/release/tensorplate-release.sh prepare --version "${TP_VERSION}" --dry-run
test/release/run.sh
```

Forbidden in the implementation PR:

- Creating the release branch.
- Removing development suffixes as a final release commit.
- Creating release-candidate or final tags.
- Publishing a GitHub Release.
- Treating local build output as public release evidence.

## Final Release Operation

### 1. Confirm Prerequisites

The release owner stops immediately unless all prerequisites are true:

- Required validation gate is `pass` or signed `conditional-pass`.
- Required CI for the release commit is green.
- Security review is complete.
- Packaging artifacts can be built from the release commit.
- Public install guide and quickstart are reviewed.
- Clean-room validation target is ready.
- No release blocker is open without a signed conditional pass.

### 2. Verify The Release Runner

The automated path is tag-driven:

1. The maintainer runs `tools/release/tensorplate-release.sh cut`.
2. The script creates or switches `release/vX.Y.Z`, prepares version
   metadata, commits it, creates an annotated source tag, and pushes the
   branch + tag.
3. `.github/workflows/release.yml` builds the `.deb` packages from that
   tag, generates the manifest/checksums, and creates the GitHub Release
   with those assets attached.

RC tags create public prereleases. Final tags create draft GitHub
Releases by default so assets can be verified and clean-room validation
can run before publication.

The workflow must run on the release target architecture. By default it
requires a self-hosted runner labeled:

```json
["self-hosted", "linux", "ARM64", "tensorplate-release"]
```

If the repository uses different labels, set the repository variable
`TENSORPLATE_RELEASE_RUNNER` to a JSON array of labels, for example:

```json
["self-hosted", "linux", "ARM64", "jetson-orin"]
```

The release runner must have:

- `sudo` access for installing Debian build dependencies.
- Rust via `rustup`, CMake, Ninja, a C++ compiler, debhelper, `dh-exec`,
  `dpkg-buildpackage`, `nlohmann-json3-dev`, and GitHub CLI `gh`.
- The target SDK stack needed for release validation. For v0.1.0 that
  means JetPack/CUDA/TensorRT on `arm64`.
- A configured vcpkg checkout via `VCPKG_ROOT`, `VCPKG_INSTALLATION_ROOT`,
  or a system `nlohmann_json` package.

### 3. Cut The Release Source Tag

Preview the local operation:

```bash
tools/release/tensorplate-release.sh cut \
  --version "${TP_VERSION}" \
  --final \
  --dry-run
```

Cut and push the final release source tag:

```bash
tools/release/tensorplate-release.sh cut \
  --version "${TP_VERSION}" \
  --final \
  --execute \
  --push \
  --confirm "CUT-${TP_TAG}"
```

For a release candidate:

```bash
tools/release/tensorplate-release.sh cut \
  --version "${TP_VERSION}" \
  --rc 1 \
  --execute \
  --push \
  --confirm "CUT-${TP_TAG}-rc.1"
```

The script refuses dirty worktrees, existing tags, and unexpected release
metadata edits. It pushes the branch before the tag; the tag push is what
starts the release workflow.

### 4. Build Release Assets Without Publishing

Before pushing a final tag or creating a public prerelease, run the
`Release` workflow manually:

- `tag`: `${TP_TAG}`
- `publish`: `false`
- `source_ref`: `${TP_RELEASE_BRANCH}`

The build-only run must:

- Build Rust release binaries.
- Build the C++ serving worker.
- Run `test/packaging/run.sh`.
- Build all required `.deb` packages.
- Copy `install.sh`.
- Generate `tensorplate-${TP_TAG}-artifacts.json` and `SHA256SUMS`.
- Upload the `tensorplate-${TP_TAG}-release-assets` workflow artifact.
- Stop before creating a GitHub Release.

Download the workflow artifact and smoke-test the installer from the
artifact directory before publication:

```bash
sudo bash install.sh
sudo bash install.sh --cli-only
```

Run the `--cli-only` smoke only when the artifact bundle includes a
matching desktop CLI package for that host architecture.

### 5. Watch CI Build And Publish Assets

Open the `Release` workflow run for `${TP_TAG}`. It must:

- Verify `${TP_TAG}` is annotated.
- Verify the tag commit is contained in `${TP_RELEASE_BRANCH}`.
- Build Rust release binaries.
- Build the C++ serving worker.
- Run `test/packaging/run.sh`.
- Build all required `.deb` packages.
- Generate `tensorplate-${TP_TAG}-artifacts.json` and `SHA256SUMS`.
- Create the GitHub Release and attach the `.deb` packages, `install.sh`,
  manifest, and checksum file. RC tags are public prereleases; final tags
  are drafts until the release owner publishes them.

The workflow refuses to replace an existing GitHub Release. If it fails
after creating no release, fix the release branch, cut a new RC tag, or
delete only the failed unpublished tag according to the tag policy.

### 6. Download And Verify CI Assets

After the workflow succeeds, download the assets from the GitHub Release
and verify checksums from a clean machine:

```bash
mkdir -p "${TP_RELEASE_DIR}"
gh release download "${TP_TAG}" \
  --dir "${TP_RELEASE_DIR}" \
  --pattern '*.deb' \
  --pattern 'install.sh' \
  --pattern 'SHA256SUMS' \
  --pattern "tensorplate-${TP_TAG}-artifacts.json"
cd "${TP_RELEASE_DIR}"
sha256sum -c SHA256SUMS
```

Review the manifest for package names, versions, architecture, release
commit, source tag, checksums, and validation links.

### 7. Run Clean-Room Validation

Run [`docs/validation/clean-room-release-smoke.md`](../validation/clean-room-release-smoke.md)
from the GitHub Release assets. The validation must download release
assets and verify checksums before install. Local source-tree binaries or
local package build directories invalidate the evidence.

Record the result in `${TP_RELEASE_DIR}/clean-room.md`. If the result is
`block`, do not publish or announce the release. For an RC, fix the
blocker and cut the next RC tag. For a final release, leave the draft
unpublished and cut a corrected release tag. If the result is
`conditional-pass`, the risk, mitigation, owner, and follow-up issue must
also appear in release notes.

### 8. Record Sign-Off And Evidence

```bash
cp docs/release/signoff-template.md "${TP_SIGNOFF}"
cp docs/release/evidence-template.md "${TP_RELEASE_DIR}/evidence.md"
```

Fill the copied files with final decisions and links to:

- The `Release` workflow run.
- The GitHub Release URL.
- The downloaded manifest/checksum verification transcript.
- The clean-room validation report.
- Reviewer approvals.

### 9. Publish And Announce

For a final release, publish the draft only after release evidence is
accepted:

```bash
gh release edit "${TP_TAG}" --draft=false --latest
```

Then publish the announcement using the release notes as the source of
truth. The announcement must not claim support beyond release evidence.

### 10. Monitor After Release

For the first release window:

- Watch install failures and `tensorplate doctor` reports.
- Triage security reports privately according to `SECURITY.md`.
- Track artifact download or checksum problems.
- Open follow-up issues for conditional risks and docs corrections.
- Use the hotfix process in [`post-release.md`](./post-release.md).

## Stop-The-Release Criteria

Stop and mark the release blocked if any item is true:

- Required validation gate is not pass or signed conditional-pass.
- Required CI is unavailable or not green.
- The release branch has unreviewed commits.
- Version metadata or changelog is inconsistent.
- The final tag already exists.
- Artifacts are missing, checksums mismatch, or manifest commit/tag data
  does not match the release.
- Required sign-off is missing.
- Clean-room validation uses local build-tree paths.
- Clean-room install, doctor, service start, deploy, inference, status,
  logs, metrics, or rollback fails without signed conditional-pass.
- Release notes overstate supported hardware, model classes, backends, or
  security posture.
