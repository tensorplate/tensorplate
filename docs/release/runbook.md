# TensorPlate release runbook

This runbook owns the public release process for TensorPlate versioned
releases. It is reusable across patch, minor, and later release lines. Set
the release variables first; examples below use `0.1.0`.

```bash
export TP_VERSION=0.1.0
# A release is cut as a candidate first and as a final tag second, and
# every step below names the tag it is operating on. Set TP_RC to the
# candidate number for a candidate pass; unset it for the final pass.
# Re-export the block when you switch passes. Defining TP_TAG as
# `v${TP_VERSION}` alone made steps 4-6 name a tag that does not exist
# while the release is on the candidate path: `git rev-parse` exits 128,
# `git push` reports "src refspec does not match any", and the download
# step asks for a release nobody cut.
export TP_RC=1                 # unset TP_RC for the final tag
export TP_TAG="v${TP_VERSION}${TP_RC:+-rc.${TP_RC}}"
# Releases are tagged off the trunk. No release branch is cut, ever:
# keeping a maintenance line in sync with the trunk, and patching an old
# line while the trunk moves on, is the cost this trades away. The trunk
# is protected and moves only by merged pull request, which is why the
# release commit below is a merged PR and `cut` only tags it.
export TP_TRUNK_BRANCH=develop
export TP_RELEASE_DIR="dist/release/${TP_TAG}"
export TP_MANIFEST="${TP_RELEASE_DIR}/tensorplate-${TP_TAG}-artifacts.json"
export TP_CHECKSUMS="${TP_RELEASE_DIR}/SHA256SUMS"
export TP_SIGNOFF="${TP_RELEASE_DIR}/signoff.md"
export TP_PREFLIGHT="${TP_RELEASE_DIR}/preflight.md"
# Release notes are per version, never per tag: every candidate and the
# final tag publish the same file, which is why `release.yml` and the
# release driver both derive it from the version.
export TP_RELEASE_NOTES="docs/release/notes/v${TP_VERSION}.md"
```

Each candidate gets its own `TP_RELEASE_DIR`, and the manifest inside it
carries the candidate's tag — `tensorplate-v0.2.1-rc.1-artifacts.json`, not
the final name. Step 6 downloads by that name, so re-export the block
before switching passes rather than editing one variable.

The release has three separated phases:

1. Add or update release tooling/docs in a normal implementation PR.
2. Finalize the version surfaces in a **preparation PR** and merge it to
   the trunk (step 3a). The trunk is protected, so the release commit can
   only arrive this way.
3. Cut an annotated tag on that merged trunk commit and publish from it
   (steps 3b onward).

Neither PR is the release. The tag is.

## Required Owners

Record sign-offs in a copy of
[`signoff-template.md`](./signoff-template.md).

| Role | Responsibility |
| --- | --- |
| Release owner | Drives the release commit, tag, GitHub Release, and evidence archive. |
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

The implementation PR does **not** finalize the version surfaces. That is
the preparation PR in step 3a, and `prepare` writes every one of them, so
they are reviewed together, merged together, and tagged as one commit:

- `CMakeLists.txt` — `project(... VERSION X.Y.Z)` and an empty
  `TP_RUNTIME_VERSION_SUFFIX`. The protocol/bundle `TP_*_VERSION_*` macros
  stay on the protocol track (e.g. `0` / `1` for `0.1`) and are never
  derived from the runtime version.
- `Cargo.toml` — `[workspace.package] version` and the `tensorplate-protocol`
  path-dependency version.
- `Cargo.lock` — every `tensorplate-*` crate version (third-party crates
  untouched).
- `vcpkg.json` — `version-string`.
- `packaging/VERSION`.
- `packaging/scripts/install.sh` — the installer's default version.
- `packaging/debian/changelog` — a new top stanza `tensorplate (X.Y.Z-1)`
  above the previous entries.
- `CHANGELOG.md` — open `## [X.Y.Z] - YYYY-MM-DD` under `[Unreleased]`,
  and move whatever `[Unreleased]` holds into it, leaving `[Unreleased]`
  empty. The tag is cut from a trunk commit, so everything under
  `[Unreleased]` at that commit ships in it. Entries merged after an
  earlier preparation PR therefore make the changelog pending again, and
  the cut refuses until a further preparation PR folds them in.

`sdk/python/pyproject.toml` keeps its `.dev0` development version; no
release step rewrites it, because the wheel version is injected at build.

### Release notes

`docs/release/notes/vX.Y.Z.md` **does** ship in the implementation PR. It
is not a version surface: `prepare` does not write it, it is absent from
the approved-file list `prepare` is allowed to touch, and
`ensure_prepare_diff_scope` would reject it if the preparation PR carried
it. Writing release notes is also review work, not a mechanical bump, so
it wants the ordinary review cycle rather than the narrow one in step 3a.
Leave it out of both PRs and the cut fails closed with
`release notes file is missing: docs/release/notes/vX.Y.Z.md (required by
the publish path)`.

It is a **hard tag prerequisite** the publish path requires
(`release.yml` `--notes-file`; the preflight). Its
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

### Validating the implementation PR

Validate with `prepare --version X.Y.Z --dry-run` and `test/release/run.sh`.
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

- Creating a release branch. Releases are tagged off the trunk; no release
  branch is cut.
- Finalizing version metadata. Removing development suffixes and promoting
  the changelog belong to the preparation PR in step 3a, which is reviewed
  on its own and merged immediately before the tag. Carrying them in an
  implementation PR leaves the trunk claiming a release nobody has cut, for
  as long as it takes to cut it.
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
- **Every Production row rests on a recorded run.** Verify with:

  ```
  cargo test -p tensorplate-platform --test registry_fixtures -- --ignored
  ```

  This is deliberately not a PR-blocking check, because a row may sit at
  Production with spec-authored values for an entire development cycle
  while its evidence run is scheduled. It is blocking *here*: a Production
  claim whose match key nobody has observed on the hardware must not ship.
  The failure names each offending row.

  Two ways to clear it, both honest — record the evidence, or downgrade the
  row until it exists. Downgrading is cheaper than it sounds:
  `is_supported_combination` admits Production **and** Preview, so a Preview
  row still deploys. It changes the published claim, not what runs.

### 2. Verify The Release Runner

The publish path is tag-driven:

1. The version metadata for `${TP_VERSION}` is already final on the trunk,
   put there by the merged preparation PR (step 3a).
2. The maintainer runs `tools/release/tensorplate-release.sh cut` from a
   refreshed trunk checkout. It verifies the checked-out commit is one
   `origin/${TP_TRUNK_BRANCH}` already contains and that its release
   metadata is final, then creates the annotated source tag on that exact
   commit. It edits nothing, commits nothing, and never pushes the trunk.
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
release sudoers allowance for the `gha-runner` account, and the bounded
apt wrapper that allowance names:

```bash
sudo tools/release/jetson-runner-control.sh off
```

`on` installs `/usr/local/sbin/tensorplate-apt`, a root-owned wrapper that
runs apt under a hard time bound, and grants the runner account `NOPASSWD`
on that path and `install` -- not on `apt-get` itself, and deliberately not
on `timeout`, which would be a root shell. **Re-run `off` then `on` after
updating this checkout** if the wrapper or the allowance changed, or the
runner keeps whatever the last `on` installed.

Do not leave this persistent self-hosted runner online for general OSS PR
CI. Normal pull-request CI should remain on GitHub-hosted runners; the
Jetson runner is for trusted release jobs that require JetPack/CUDA/
TensorRT on the target architecture.

### 3. Put The Release Commit On The Trunk, Then Tag It

The trunk is protected: `develop` requires a pull request and the ruleset
names no bypass actor, so nothing in this step pushes a commit to it. The
release commit arrives the ordinary way, as a merged PR, and `cut` only
tags what is already there.

The order is load-bearing. Preparing version metadata locally and tagging
it before that PR merged put the tag on a commit the squash merge then
replaced with a different SHA: the tag pointed at a commit no branch
contained, and the workflow's trunk-ancestry check (step 5) rejected it.

#### 3a. Finalize the version surfaces in a preparation PR

The preparation PR is per version, not per tag: it finalizes the
`${TP_VERSION}` surfaces once, and every candidate and the final tag are
cut from that same commit.

Skip to 3b when the trunk already carries final metadata for
`${TP_VERSION}` — `prepare` is then a no-op and there is nothing to merge.
`cut` checks this itself and stops with the remaining files named, so
guessing wrong costs one command, not a bad tag. The PR can also be
smaller than the file list below when earlier work already finalized some
surfaces: `prepare` writes whatever is still pending and leaves the rest
alone, so a preparation PR that promotes only `CHANGELOG.md` is a normal
outcome, not a sign something was missed.

```bash
git switch --create "release-prep-v${TP_VERSION}" "origin/${TP_TRUNK_BRANCH}"
tools/release/tensorplate-release.sh prepare \
  --version "${TP_VERSION}" \
  --prep-branch "release-prep-v${TP_VERSION}" \
  --execute \
  --confirm "PREPARE-v${TP_VERSION}"
git add -- CMakeLists.txt Cargo.toml Cargo.lock vcpkg.json \
  packaging/VERSION packaging/debian/changelog \
  packaging/scripts/install.sh CHANGELOG.md
git commit -m "Prepare v${TP_VERSION} release"
gh pr create --base "${TP_TRUNK_BRANCH}" --title "Prepare v${TP_VERSION} release"
```

`prepare` refuses to touch anything outside that file list, so the PR is
exactly the version surfaces and nothing else. `docs/release/notes/vX.Y.Z.md`
is deliberately not in it — that file ships in the implementation PR, above.

Merge it under the ordinary review and CI rules. Squash or merge commit
both work: the tag is created afterwards, on whatever commit the merge
actually produced. **Record that commit** — step 3b tags it by name, and
it is the only thing that distinguishes the release commit from whatever
else lands on the trunk next:

```bash
export TP_RELEASE_COMMIT="$(gh pr view "release-prep-v${TP_VERSION}" \
  --json mergeCommit --jq '.mergeCommit.oid')"
echo "${TP_RELEASE_COMMIT}"
```

When 3a was skipped because the metadata was already final, there is no
merge commit to read. The release commit is then the reviewed trunk commit
whose CI you confirmed green in step 1; set `TP_RELEASE_COMMIT` to that SHA
explicitly rather than letting step 3b take whatever the trunk holds.

#### 3b. Stand on the release commit and tag it

```bash
git switch "${TP_TRUNK_BRANCH}"
git pull --ff-only
git rev-parse HEAD           # compare against ${TP_RELEASE_COMMIT}
```

`git pull` brings the trunk's **head**, which is not necessarily the
release commit. Any PR that merged between the preparation merge and this
pull is now HEAD, and nothing downstream will notice: that commit is still
an ancestor of the trunk, so the ancestry check passes, and `prepare` is
idempotent, so the metadata gate still reads as final. The tag would land
on a commit nobody reviewed for this release and CI would publish it.
That is why 3a recorded `TP_RELEASE_COMMIT`. If HEAD is not that commit,
stand on it — the trunk is never pushed by this procedure, so the reset is
local only:

```bash
git reset --hard "${TP_RELEASE_COMMIT}"
```

`cut` tags a commit behind the trunk head deliberately and says so
(`HEAD is behind origin/${TP_TRUNK_BRANCH}; tagging that older trunk
commit`). Pass `--expect-commit` so the check is enforced rather than
remembered.

Preview the local operation:

```bash
tools/release/tensorplate-release.sh cut \
  --version "${TP_VERSION}" \
  --release-branch "${TP_TRUNK_BRANCH}" \
  --expect-commit "${TP_RELEASE_COMMIT}" \
  --final \
  --dry-run
```

Cut the tag locally. Use `--final` for the final tag and `--rc N` for a
candidate; `${TP_TAG}` already carries whichever one the variable block at
the top selected, so the confirmation token is the same line either way:

```bash
tools/release/tensorplate-release.sh cut \
  --version "${TP_VERSION}" \
  --release-branch "${TP_TRUNK_BRANCH}" \
  --expect-commit "${TP_RELEASE_COMMIT}" \
  --final \
  --execute \
  --confirm "CUT-${TP_TAG}"
```

For a release candidate, with `TP_RC` set to its number:

```bash
tools/release/tensorplate-release.sh cut \
  --version "${TP_VERSION}" \
  --release-branch "${TP_TRUNK_BRANCH}" \
  --expect-commit "${TP_RELEASE_COMMIT}" \
  --rc "${TP_RC}" \
  --execute \
  --confirm "CUT-${TP_TAG}"
```

`cut` refuses a dirty worktree, a checkout that is not on the trunk, a
commit `origin/${TP_TRUNK_BRANCH}` does not already contain, a HEAD that
is not `--expect-commit`, an existing tag, and release metadata that is
not yet final. The ancestry condition is the same one the publish workflow
asserts in step 5, so a tag CI would reject is never created in the first
place.

Nothing is pushed here, and nothing needs to be: the preparation PR already
moved the trunk. Confirm the commit the tag names — the next step builds
from it:

```bash
git rev-list -n1 "${TP_TAG}"
```

### 4. Build Release Assets Without Publishing

Before pushing a final tag or creating a public prerelease, run the
`Release` workflow manually:

- `tag`: `${TP_TAG}`
- `publish`: `false`
- `source_ref`: the tag's commit, **not** a branch name:

  ```bash
  git rev-parse "${TP_TAG}^{commit}"
  ```

A branch name moves. Dispatching on `${TP_TRUNK_BRANCH}` validates
whatever the trunk holds when each job starts, so a PR merging mid-run
rehearses a tree that is not the one the tag names — and the jobs resolve
the ref independently, so two of them can build two different trees under
one release identity. The workflow resolves `source_ref` once and hands
every dependent job the resolved commit, and passing the tag's own commit
makes the rehearsal build exactly what the tag will publish.

Leaving `source_ref` empty checks out `${TP_TAG}` instead, which requires
the tag to be pushed already — the opposite of what this step is for.

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

Publish-grade bundles always carry the `amd64` runtime set, so run both the
full-runtime and `--cli-only` smokes on an Ubuntu x86_64 host. Only a
single-architecture local-source snapshot can lack a matching package.

### 5. Watch CI Build And Publish Assets

After build-only validation passes, push the annotated tag. The tag push is
what starts the publish workflow from the tag ref:

```bash
git push origin "${TP_TAG}"
```

Open the `Release` workflow run for `${TP_TAG}`. It must:

- Verify `${TP_TAG}` is annotated.
- Verify annotated tag and trunk ancestry: the tag commit is an ancestor
  of `origin/${TP_TRUNK_BRANCH}`.
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
- Read back the asset names the new release serves and fail unless they
  are exactly the names `SHA256SUMS` lists, plus `SHA256SUMS` and its
  bundle. GitHub stores an asset under a name of its own choosing (it
  serves `~` as `.`), and a release whose signed list names a file it does
  not serve cannot be installed or verified. A final release is still a
  draft at this point; a candidate is already public, and a failure here
  means it must be superseded by the next RC.

The workflow refuses to replace an existing GitHub Release. If it fails
after creating no release, fix the trunk, cut a new RC tag, or delete only
the failed unpublished tag according to the tag policy.

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
# A candidate's package file names spell its `~rc.N` as `.rc.N`, the name
# GitHub serves; the package's own Version keeps the tilde.
gh attestation verify "tensorplate-agent_${TP_VERSION}${TP_RC:+.rc.${TP_RC}}-1_arm64.deb" \
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
- **`publish-homebrew`** (env `homebrew`) — renders the five component
  formulas and the `tensorplate` meta-formula from
  `packaging/homebrew/Formula/`, then opens one auto-merge PR in
  [`tensorplate/homebrew-tap`](https://github.com/tensorplate/homebrew-tap).
  Every formula points at the same tagged source archive and checksum. The job
  finishing means "PR opened with auto-merge armed", not "merged": the tap CI
  (audit + build-from-source + `brew test` on Apple Silicon) gates the merge,
  so the tap goes live eventually-consistently. Re-runnable.
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
- The trunk carries unreviewed commits at or below the tag commit.
- The tag commit is not already contained in `origin/${TP_TRUNK_BRANCH}`.
- The tag commit is not `${TP_RELEASE_COMMIT}`, the commit step 3a
  recorded. Pass `cut --expect-commit` so this is enforced rather than
  eyeballed: ancestry alone does not catch a trunk that moved between the
  preparation merge and the pull.
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
