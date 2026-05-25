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

### 2. Create Or Verify The Release Branch

```bash
git fetch origin
git switch --create "${TP_RELEASE_BRANCH}" origin/develop
git status --short --branch
```

If the release branch already exists, verify it points at the reviewed
post-merge release baseline before continuing.

### 3. Prepare Release Metadata

Preview changes:

```bash
tools/release/tensorplate-release.sh prepare --version "${TP_VERSION}" --dry-run
```

Apply metadata changes only on the release branch:

```bash
tools/release/tensorplate-release.sh prepare \
  --version "${TP_VERSION}" \
  --execute \
  --confirm "PREPARE-${TP_TAG}"
```

Review that only approved release metadata files changed, then commit the
release metadata update.

### 4. Build Artifacts

Build from the release commit:

```bash
cargo build --release --bin tensorplate-agent --bin tensorplate-observability --bin tensorplate
cmake --build build/release --target tensorplate-serving
./packaging/scripts/build-deb.sh
```

Collect required `.deb` assets under `${TP_RELEASE_DIR}`. Artifact names
and manifest rules are defined in [`artifacts.md`](./artifacts.md).

### 5. Generate Manifest And Checksums

```bash
tools/release/tensorplate-release.sh manifest \
  --version "${TP_VERSION}" \
  --tag "${TP_TAG}" \
  --artifacts-dir "${TP_RELEASE_DIR}" \
  --manifest "${TP_MANIFEST}" \
  --checksums "${TP_CHECKSUMS}"
```

Review the manifest for package names, versions, architecture, release
commit, source tag, checksums, and validation links.

### 6. Record Sign-Off

```bash
mkdir -p "${TP_RELEASE_DIR}"
cp docs/release/signoff-template.md "${TP_SIGNOFF}"
cp docs/release/evidence-template.md "${TP_RELEASE_DIR}/evidence.md"
```

Fill the copied files with final decisions. The release script rejects
`TODO`, `TBD`, `PLACEHOLDER`, `BLOCKED`, missing reviewers, missing gate
state, or a final decision other than `pass` / `conditional-pass`.

### 7. Run Preflight

```bash
tools/release/tensorplate-release.sh preflight \
  --version "${TP_VERSION}" \
  --artifacts-dir "${TP_RELEASE_DIR}" \
  --manifest "${TP_MANIFEST}" \
  --checksums "${TP_CHECKSUMS}" \
  --signoff "${TP_SIGNOFF}" \
  --report "${TP_PREFLIGHT}"
```

Preflight must pass before tag creation. A failure is actionable and must
be fixed in the owning area, not patched around in release automation.

### 8. Create The Annotated Tag

For a release candidate:

```bash
tools/release/tensorplate-release.sh tag \
  --version "${TP_VERSION}" \
  --rc 1 \
  --artifacts-dir "${TP_RELEASE_DIR}" \
  --manifest "${TP_MANIFEST}" \
  --checksums "${TP_CHECKSUMS}" \
  --signoff "${TP_SIGNOFF}" \
  --confirm "CREATE-${TP_TAG}-rc.1"
```

Failed RCs are never retagged. Fix the blocker, rebuild artifacts,
regenerate manifest/checksums, rerun required validation, and create the
next RC tag.

For the final release:

```bash
tools/release/tensorplate-release.sh tag \
  --version "${TP_VERSION}" \
  --final \
  --artifacts-dir "${TP_RELEASE_DIR}" \
  --manifest "${TP_MANIFEST}" \
  --checksums "${TP_CHECKSUMS}" \
  --signoff "${TP_SIGNOFF}" \
  --confirm "CREATE-${TP_TAG}"
```

Inspect tag metadata:

```bash
git show "${TP_TAG}"
```

Push the branch and tag only after review:

```bash
git push origin "${TP_RELEASE_BRANCH}"
git push origin "${TP_TAG}"
```

### 9. Create The GitHub Release Draft

Generate the guarded draft command first:

```bash
tools/release/tensorplate-release.sh publish \
  --version "${TP_VERSION}" \
  --tag "${TP_TAG}" \
  --artifacts-dir "${TP_RELEASE_DIR}" \
  --manifest "${TP_MANIFEST}" \
  --checksums "${TP_CHECKSUMS}" \
  --release-notes "${TP_RELEASE_NOTES}" \
  --dry-run
```

After review, create the draft:

```bash
tools/release/tensorplate-release.sh publish \
  --version "${TP_VERSION}" \
  --tag "${TP_TAG}" \
  --artifacts-dir "${TP_RELEASE_DIR}" \
  --manifest "${TP_MANIFEST}" \
  --checksums "${TP_CHECKSUMS}" \
  --release-notes "${TP_RELEASE_NOTES}" \
  --execute \
  --confirm "PUBLISH-${TP_TAG}"
```

The draft must contain packages, manifest, checksums, release notes,
install guide, validation links, supported hardware/OS statement, known
limitations, support policy, security policy, and rollback guidance.

### 10. Run Clean-Room Validation

Run [`docs/validation/clean-room-release-smoke.md`](../validation/clean-room-release-smoke.md)
from the GitHub Release assets. The validation must download release
assets and verify checksums before install. Local source-tree binaries or
local package build directories invalidate the evidence.

Record the result in `${TP_RELEASE_DIR}/clean-room.md`. If the result is
`block`, do not publish the release. If the result is `conditional-pass`,
the risk, mitigation, owner, and follow-up issue must also appear in
release notes.

### 11. Publish And Announce

Publish the GitHub Release only after clean-room validation is accepted.
Then publish the announcement using the release notes as the source of
truth. The announcement must not claim support beyond release evidence.

### 12. Monitor After Release

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
