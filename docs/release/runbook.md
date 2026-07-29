# TensorPlate release runbook

This runbook owns the public release process for TensorPlate versioned
releases. It is reusable across patch, minor, and later release lines. Set
the release variables first; examples below use `0.1.0`.

```bash
export TP_VERSION=0.1.0
export TP_TAG="v${TP_VERSION}"
# One maintenance branch per minor line (release/0.1, release/0.2, ...);
# every patch of that line is committed and tagged there.
export TP_RELEASE_BRANCH="release/${TP_VERSION%.*}"
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

### Version surfaces

The implementation PR also bumps the release version across **every** surface,
because `prepare` does not bump them all on a finalized maintenance line: its
`prepare_python` step clears the `-dev` suffix but leaves `CMakeLists.txt`'s
`project(... VERSION X.Y.Z)` and the `Cargo.lock` `tensorplate-*` crate
versions unchanged (those only carry a `-dev` form on a `develop` cut). Bump,
by hand, all of:

- `CMakeLists.txt` — `project(... VERSION X.Y.Z)` (leave the protocol/bundle
  `TP_*_VERSION_*` macros on the protocol track, e.g. `0` / `1` for `0.1`).
- `Cargo.toml` — `[workspace.package] version` and the `tensorplate-protocol`
  path-dependency version.
- `Cargo.lock` — every `tensorplate-*` crate version (third-party crates
  untouched).
- `vcpkg.json` — `version-string`.
- `packaging/VERSION`.
- `packaging/debian/changelog` — the top stanza `tensorplate (X.Y.Z-1)`.
- `CHANGELOG.md` — promote `[Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD`.
- `sdk/python/pyproject.toml` — the `.dev0` development version (cosmetic; the
  wheel version is injected at build).

Also add `docs/release/notes/vX.Y.Z.md` — a **hard tag prerequisite** the
publish path requires (`release.yml` `--notes-file`; the preflight). Its
supported-environment section must **link
[`docs/release/support-matrix.md`](support-matrix.md)**, not restate it.
That file is generated from `config/platform/` and guarded by a golden
test, so the platforms a release claims are the rows `doctor` matches and
deploy admission enforces. Prose restating them drifts, and a release note
that overstates supported hardware is a documented release blocker below.
A row change invalidates more than this one file, so regenerate both
goldens after any edit under `config/platform/`:

```
UPDATE_GOLDEN=1 cargo test -p tensorplate-platform --test support_matrix
UPDATE_GOLDEN=1 cargo test -p tensorplate-cli --test doctor_host_section
```

The second guards the `doctor` host section, whose candidate lists change
whenever a row is added, removed, or rescoped.
Then
validate with `prepare --version X.Y.Z --dry-run` and `test/release/run.sh`.
`preflight`/`cut` run `check_version_files`, which fails closed on any stale
surface (including a partially bumped `Cargo.lock`), so a missed surface stops
the cut rather than shipping a misversioned build.

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
- `docs/release/notes/vX.Y.Z.md` exists (a hard tag prerequisite, enforced by
  `preflight`/`cut` and required by the publish path's `--notes-file`).
- The self-hosted release runner is available for the build, and all four
  publish environments (`pypi`, `apt`, `homebrew`, `github-release`) each have
  a required reviewer (a reviewer-less environment publishes without a hold).
- Clean-room validation target is ready.
- No release blocker is open without a signed conditional pass.

### 2. Verify The Release Runner

The publish path is tag-driven:

1. The maintainer runs `tools/release/tensorplate-release.sh cut`.
2. The script switches to (or creates, once per minor line) the
   maintenance branch `release/X.Y`, prepares version metadata, commits
   it, and creates an annotated source tag on that branch.
3. `.github/workflows/release.yml` builds the `.deb` packages from that
   tag, generates the manifest/checksums, and creates the GitHub Release
   with those assets attached after the tag is pushed.

RC tags create public prereleases. Final tags create draft GitHub
Releases by default so assets can be verified and clean-room validation
can run before publication.

The workflow must run on the release target architecture and must build
the real TensorRT execution path for v0.1.x packages. GitHub's hosted
`ubuntu-22.04-arm` runner is acceptable only if the workflow also
provides a JetPack-compatible CUDA/TensorRT development SDK and the CMake
configure log contains `TensorRT SDK detected; building real TensorRT
adapter execution path`. A hosted ARM build without that SDK is not a
publishable v0.1.x release build, because it produces a TensorRT adapter
that advertises the backend but returns `Unsupported` at engine load.

Final publication still requires clean-room validation on the Jetson Orin
Nano 8GB Super / JetPack 6.x floor, because a hosted runner is not a
JetPack/L4T system.

Future release lines should provide a dedicated self-hosted runner labeled:

```json
["self-hosted", "linux", "ARM64", "tensorplate-release"]
```

The package build runner must have:

- `sudo` access for installing Debian build dependencies.
- Rust via `rustup`, CMake, Ninja, a C++ compiler, debhelper, `dh-exec`,
  `dpkg-buildpackage`, `nlohmann-json3-dev`, and GitHub CLI `gh`.
- A configured vcpkg checkout via `VCPKG_ROOT`, `VCPKG_INSTALLATION_ROOT`,
  or a system `nlohmann_json` package.
- JetPack-compatible CUDA/TensorRT development headers and libraries when
  building v0.1.x release packages with `TP_ENABLE_TENSORRT=ON`. Release
  artifact builds default `TP_REQUIRE_TENSORRT_SDK=ON` so they fail during
  CMake configure instead of producing non-functional TensorRT packages.
- Outbound network access to Sigstore (Fulcio/Rekor) and the GitHub
  attestation API so the publish path can keyless-sign `SHA256SUMS` and
  record build provenance. The repository must allow artifact attestations.
  `cosign` itself is installed by the workflow.

The future dedicated self-hosted runner should also have the target SDK
stack needed for release validation. For v0.1.x that means
JetPack/CUDA/TensorRT on `arm64`.

For the current Jetson release runner, keep the runner offline and
unprivileged except during trusted release builds. The operator helper is
secret-free and may be copied to `/usr/local/sbin/tensorplate-runner` on
the Jetson, or run from the checked-out repository:

```bash
sudo tools/release/jetson-runner-control.sh status
sudo tools/release/jetson-runner-control.sh on
```

After the build-only or publish workflow finishes, turn the runner back
off. This stops and disables the systemd service and removes the temporary
release sudoers allowance for the `gha-runner` account:

```bash
sudo tools/release/jetson-runner-control.sh off
```

Do not leave this persistent self-hosted runner online for general OSS PR
CI. Normal pull-request CI should remain on GitHub-hosted runners; the
Jetson runner is for trusted release jobs that require JetPack/CUDA/
TensorRT on the target architecture.

### 3. Cut The Release Source Tag

Preview the local operation:

```bash
tools/release/tensorplate-release.sh cut \
  --version "${TP_VERSION}" \
  --final \
  --dry-run
```

Cut the final release source tag locally:

```bash
tools/release/tensorplate-release.sh cut \
  --version "${TP_VERSION}" \
  --final \
  --execute \
  --confirm "CUT-${TP_TAG}"
```

For a release candidate:

```bash
tools/release/tensorplate-release.sh cut \
  --version "${TP_VERSION}" \
  --rc 1 \
  --execute \
  --confirm "CUT-${TP_TAG}-rc.1"
```

The script refuses dirty worktrees, existing tags, and unexpected release
metadata edits. Push the release branch for build-only validation, but do
not push the tag yet:

```bash
git push origin "${TP_RELEASE_BRANCH}"
```

### 4. Build Release Assets Without Publishing

Before pushing a final tag or creating a public prerelease, run the
`Release` workflow manually:

- `tag`: `${TP_TAG}`
- `publish`: `false`
- `source_ref`: `${TP_RELEASE_BRANCH}`

The build-only run must:

- Build Rust release binaries.
- Build the C++ serving worker.
- Build the complete `amd64` runtime package set — agent, serving worker,
  observability, CLI, and the metapackage (hosted job).
- Build the `tensorplate-python` wheel + sdist at the release version.
- Run `test/packaging/run.sh`.
- Build all required `.deb` packages.
- Copy `install.sh`.
- Generate `tensorplate-${TP_TAG}-artifacts.json` and `SHA256SUMS`.
- Upload the `tensorplate-${TP_TAG}-release-assets-unsigned` workflow
  artifact (the signed `…-release-assets` name is produced only on the
  publish path).
- Skip signing and provenance, which run only on the publish path; the
  uploaded assets are unsigned.
- Stop before creating a GitHub Release.

Download the workflow artifact and smoke-test the installer from the
artifact directory before publication. The installer does **not** auto-detect
sibling artifacts: without `--local-artifacts` it downloads the pinned
published release, which does not exist yet for the tag under validation.
Build-only assets are also unsigned, so pass `--allow-unsigned`:

```bash
sudo bash install.sh --local-artifacts "$(pwd)" --allow-unsigned
sudo bash install.sh --local-artifacts "$(pwd)" --cli-only --allow-unsigned
```

Run the `--cli-only` smoke only when the artifact bundle includes a
matching desktop CLI package for that host architecture.

### 5. Watch CI Build And Publish Assets

After build-only validation passes, push the annotated tag. The tag push is
what starts the publish workflow from the tag ref:

```bash
git push origin "${TP_TAG}"
```

Open the `Release` workflow run for `${TP_TAG}`. It must:

- Verify `${TP_TAG}` is annotated.
- Verify the tag commit is contained in `${TP_RELEASE_BRANCH}`.
- Build Rust release binaries.
- Build the C++ serving worker.
- Build the complete `amd64` runtime package set and the
  `tensorplate-python` wheel + sdist.
- Run `test/packaging/run.sh`.
- Build all required `.deb` packages.
- Generate `tensorplate-${TP_TAG}-artifacts.json` and `SHA256SUMS`.
- Sign `SHA256SUMS` with keyless cosign and record SLSA build provenance
  for the packages, installer, wheel/sdist, manifest, and checksums.
- Create the GitHub Release and attach the `.deb` packages, `install.sh`,
  the wheel + sdist, manifest, checksum file, and `SHA256SUMS.cosign.bundle`.
  RC tags are public prereleases; final tags are created as **drafts**, then
  the approval-gated `publish-github` job un-drafts them (Step 9).

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
  --pattern 'tensorplate_python-*.whl' \
  --pattern 'tensorplate_python-*.tar.gz' \
  --pattern 'SHA256SUMS' \
  --pattern 'SHA256SUMS.cosign.bundle' \
  --pattern "tensorplate-${TP_TAG}-artifacts.json"
cd "${TP_RELEASE_DIR}"
cosign verify-blob \
  --bundle SHA256SUMS.cosign.bundle \
  --certificate-identity-regexp "^https://github.com/tensorplate/tensorplate/\.github/workflows/release\.yml@refs/tags/v[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  SHA256SUMS
sha256sum -c SHA256SUMS
gh attestation verify "tensorplate-agent_${TP_VERSION}-1_arm64.deb" \
  --repo tensorplate/tensorplate
```

Review the manifest for package names, versions, architecture, release
commit, source tag, checksums, and validation links. Verify the cosign
signature (authenticity) and provenance attestation before checksums; a
checksum match alone does not prove the assets came from the release
workflow.

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

For a final tag, the `Release` workflow signs and attests the build, creates
the GitHub Release as a **draft**, and then fans out to four parallel publish
jobs — `publish-pypi`, `publish-apt`, `publish-homebrew`, and
`publish-github` — each **paused on its own protected deployment environment**
(`pypi`, `apt`, `homebrew`, `github-release`). Every channel publishes the same
cosign-signed, checksum-covered artifacts from this one build; release
candidates are gated out of all four.

Approve each channel from the Actions run page only after release evidence is
accepted. Channels are independent — approve in any order, or hold any one:

- **`publish-pypi`** (env `pypi`) — uploads the signed wheel + sdist via PyPI
  Trusted Publishing. PyPI is **immutable**: approve last-mile and with full
  intent; a published version cannot be replaced.
- **`publish-apt`** (env `apt`) — builds, signs, and syncs the stable APT
  repository from this run's signed assets, then validate per
  [`apt-repository.md`](./apt-repository.md). Re-runnable on failure.
- **`publish-homebrew`** (env `homebrew`) — opens an auto-merge formula-bump PR
  in [`tensorplate/homebrew-tap`](https://github.com/tensorplate/homebrew-tap).
  The job finishing means "PR opened with auto-merge armed", not "merged": the
  tap CI (audit + build-from-source + `brew test` on Apple Silicon) gates the
  merge, so the tap goes live eventually-consistently. Re-runnable.
- **`publish-github`** (env `github-release`) — un-drafts the GitHub Release
  (`gh release edit --draft=false --latest`), making the assets public.

Holding one channel does not block the others. A held or failed `publish-apt` /
`publish-homebrew` can be re-run to completion from the run page; `publish-apt`
also has the manual `apt-repo.yml` (`workflow_dispatch`) republish/recovery
path. PyPI is not retried for a version that already published.

Then publish the announcement using the release notes as the source of truth.
The announcement must not claim support beyond release evidence.

#### Release publishing environments (one-time external setup)

The parallel flow requires these to exist before the first final tag, mirroring
the external-setup discipline of earlier releases:

- Protected environments with **required reviewers**: `pypi` (already present),
  `apt`, `homebrew`, and `github-release`. The reviewer approval on each is the
  per-channel go-live gate; an environment left without a reviewer publishes
  without a hold.
- A **`HOMEBREW_TAP_TOKEN`** secret — a fine-grained PAT or (preferred) GitHub
  App token with `contents` + `pull_requests` write **scoped to
  `tensorplate/homebrew-tap` only**.
- In `tensorplate/homebrew-tap`: **auto-merge enabled** and a **required status
  check** set, so the bump PR waits for the tap CI before merging. The tap
  `main` must require **0 approving reviews** (or grant the bump bot a bypass
  actor): the automation cannot approve its own PR, so a required review would
  leave the bump open forever and the Homebrew channel would silently never go
  live after approval. The required CI check stays the merge gate.
- Opt-in repository variables: `PUBLISH_SDK_TO_PYPI=true` (PyPI),
  `PUBLISH_HOMEBREW_FORMULA=true` (Homebrew); APT runs when `TP_APT_REPO_DEST`
  is set. The existing `TP_APT_*` vars/secrets carry over unchanged.

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
- The `cosign verify-blob` signature over `SHA256SUMS`, or any
  `gh attestation verify`, fails — a checksum match alone does not prove the
  assets came from the release workflow.
- Required sign-off is missing.
- Clean-room validation uses local build-tree paths.
- Clean-room install, doctor, service start, deploy, inference, status,
  logs, metrics, or rollback fails without signed conditional-pass.
- Release notes overstate supported hardware, model classes, backends, or
  security posture.
