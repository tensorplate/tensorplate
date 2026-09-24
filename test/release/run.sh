#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Lightweight host checks for the release driver. These checks do not create
# tags, publish releases, or require root.

set -Eeuo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
cd "$repo_root"

script="tools/release/tensorplate-release.sh"
build_script="tools/release/build-release-artifacts.sh"
source_install_script="packaging/scripts/build-install-from-source.sh"
publish_apt_script="tools/release/publish-apt-repo.sh"
publish_homebrew_script="tools/release/publish-homebrew-formula.sh"
verify_homebrew_formulas="test/release/verify_homebrew_formulas.sh"
verify_artifact_identity="test/release/test_artifact_identity.py"
verify_published_release="test/release/test_published_release.py"
verify_release_asset_names="test/release/test_release_asset_names.py"
verify_build_source_identity="test/release/test_build_source_identity.py"
verify_build_configuration="test/release/test_build_configuration.py"
verify_changelog_fold="test/release/test_changelog_fold.py"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# A fixture package: the control fields dpkg-deb -f reads, as text.
fixture_deb() {
  local path="$1" package="$2" version="$3" arch="$4" note="$5"
  printf 'Package: %s\nVersion: %s\nArchitecture: %s\nDescription: %s\n' \
    "$package" "$version" "$arch" "$note" >"$path"
}

# Stage packages into an artifacts directory the way a release build does:
# with build-release-artifacts.sh's own staging functions, lifted out of it
# because the script itself compiles the runtime. A fixture that named its
# own files would pass whatever names the build published.
stage_like_release() {
  local dest="$1" deb_version="$2" functions
  shift 2
  functions="$(sed -n '/^release_asset_name() {$/,/^}$/p;/^stage_release_debs() {$/,/^}$/p' "$build_script")"
  if [[ "$functions" != *"release_asset_name() {"*"stage_release_debs() {"* ]]; then
    echo "FAIL: $build_script no longer defines release_asset_name and stage_release_debs" >&2
    return 1
  fi
  (
    DEB_VERSION="$deb_version"
    # shellcheck disable=SC2329  # called by the eval'd stage_release_debs
    die() { echo "FAIL: release staging: $*" >&2; exit 1; }
    eval "$functions"
    : "$DEB_VERSION"  # read by stage_release_debs
    stage_release_debs "$dest" "$@"
  )
}

bash -n "$script"
bash -n "$build_script"
bash -n "$source_install_script"
"$script" --help >/dev/null
"$script" prepare --version 0.1.0 --dry-run >/dev/null
"$script" cut --version 0.1.0 --final --dry-run >/dev/null
"$script" cut --version 0.1.0 --rc 1 --dry-run >/dev/null
"$build_script" --help >/dev/null
"$source_install_script" --help >/dev/null
bash -n "$publish_apt_script"
"$publish_apt_script" --help >/dev/null
bash -n "$publish_homebrew_script"
"$publish_homebrew_script" --help >/dev/null
"$verify_homebrew_formulas"
python3 "$verify_artifact_identity"
python3 "$verify_published_release"
python3 "$verify_release_asset_names"
python3 "$verify_build_source_identity"
python3 "$verify_build_configuration"
python3 "$verify_changelog_fold"

# Manifest generation reads each package's control Version with dpkg-deb,
# and every fixture package below is a control-style text file. This
# stand-in reads one as dpkg-deb reads a package (test_artifact_identity.py
# holds the same one; test_published_release.py repeats its cases with real
# packages where dpkg is installed). It goes on PATH only after the Python
# suites above, which would otherwise take it for the real dpkg-deb.
mkdir -p "$tmp/stub-bin"
cat >"$tmp/stub-bin/dpkg-deb" <<'STUB'
#!/bin/sh
if [ "$#" -ne 3 ] || [ "$1" != -f ]; then
  echo "dpkg-deb stand-in: unexpected arguments: $*" >&2
  exit 9
fi
if ! grep -q '^Package: ' "$2" 2>/dev/null; then
  printf "dpkg-deb: error: '%s' is not a Debian format archive\n" "$2" >&2
  exit 2
fi
sed -n "s/^$3: //p" "$2"
STUB
chmod +x "$tmp/stub-bin/dpkg-deb"
export PATH="$tmp/stub-bin:$PATH"

# The trunk is protected: it moves only by merged pull request, so `cut`
# tags a commit that is already on it and changes nothing. The ordinary
# invocation -- no flags -- must therefore select the trunk and promise no
# branch creation, no commit, and no push of the protected branch. This
# replaces an assertion that pinned the opposite (a release/MAJOR.MINOR
# default), which is the model this release retires: that default made the
# plain command create a branch whose tag release.yml rejects for not
# descending from the trunk.
cut_dry_run="$("$script" cut --version 0.1.2 --final --dry-run)"
printf '%s\n' "$cut_dry_run" | grep -q 'trunk branch: develop' || {
  echo "FAIL: cut must default to the trunk" >&2
  exit 1
}
if printf '%s\n' "$cut_dry_run" | grep -q 'release/'; then
  echo "FAIL: cut must not name a release/X.Y maintenance branch" >&2
  exit 1
fi
# --- `cut` behaviour, executed rather than quoted ------------------------
#
# The dry-run assertions above read `cut`'s own printf text. Assertions
# that quoted "cut never edits or commits" and "Would not push develop"
# were checked against behavioural mutations and caught neither: inserting
# `git commit --allow-empty` before `git tag`, and `git push origin
# "$RELEASE_BRANCH"` beside the tag push, both left the whole driver
# green, because the printf they read is documentation. What follows runs
# the real `cut --execute` against a throwaway repo whose origin refuses
# branch pushes the way the trunk ruleset does, and observes the tag, the
# commit graph, and the refs origin actually received.
cut_repo="$tmp/cut-sandbox"
cut_origin="$tmp/cut-origin.git"
mkdir -p "$cut_repo"
for f in CMakeLists.txt Cargo.toml Cargo.lock vcpkg.json CHANGELOG.md \
         packaging/VERSION packaging/debian/changelog packaging/scripts/install.sh \
         protocol/rust/src/lib.rs include/tensorplate/version.hpp.in; do
  mkdir -p "$cut_repo/$(dirname "$f")"
  cp "$repo_root/$f" "$cut_repo/$f"
done
mkdir -p "$cut_repo/config/schemas" "$cut_repo/protocol/schemas" "$cut_repo/docs/release/notes"
cp "$repo_root"/config/schemas/*.json "$cut_repo/config/schemas/"
cp "$repo_root"/protocol/schemas/*.json "$cut_repo/protocol/schemas/"
cut_version="$(tr -d '[:space:]' < "$repo_root/packaging/VERSION")"
printf '# TensorPlate v%s\n\nSandbox notes.\n' "$cut_version" \
  > "$cut_repo/docs/release/notes/v${cut_version}.md"

git init -q --bare -b develop "$cut_origin"
(
  cd "$cut_repo"
  git init -q -b develop .
  git config user.email release-test@example.invalid
  git config user.name "release test"
  git remote add origin "$cut_origin"
  git add -- CMakeLists.txt Cargo.toml Cargo.lock vcpkg.json CHANGELOG.md \
    packaging protocol config include docs
  git commit -qm "sandbox base"
  # Finalize the surfaces in the sandbox so the metadata gate is satisfied
  # whatever state the real tree is in.
  "$repo_root/$script" prepare --version "$cut_version" --execute \
    --confirm "PREPARE-v${cut_version}" >/dev/null
  git add -- CMakeLists.txt Cargo.toml Cargo.lock vcpkg.json CHANGELOG.md packaging
  git commit -qm "prepare v${cut_version}" --allow-empty
  release_commit="$(git rev-parse HEAD)"
  # The hazard this guards: an unrelated PR merges between the preparation
  # merge and the maintainer's pull, so the trunk head is no longer the
  # release commit.
  printf '\nunrelated trunk change\n' >> docs/release/notes/"v${cut_version}.md"
  git add -- docs
  git commit -qm "unrelated trunk commit"
  trunk_head="$(git rev-parse HEAD)"
  git push -q origin develop

  # From here origin models the protected trunk: the ruleset names no
  # bypass actor, so no branch ref may move. Only tags may.
  cat > "$cut_origin/hooks/pre-receive" <<'HOOK'
#!/bin/sh
while read -r _old _new ref; do
  case "$ref" in
    refs/tags/*) ;;
    *) echo "protected trunk: push to $ref rejected" >&2; exit 1 ;;
  esac
done
exit 0
HOOK
  chmod +x "$cut_origin/hooks/pre-receive"
  git fetch -q origin

  cut_sandbox() {
    "$repo_root/$script" cut --version "$cut_version" \
      --release-branch develop --expect-commit "$release_commit" "$@"
  }

  # 1. Standing on the trunk head rather than the release commit must stop
  #    the cut. Ancestry alone does not catch this: trunk_head IS an
  #    ancestor of origin/develop, and `prepare` is idempotent, so the
  #    metadata gate reads as final on it too.
  [[ "$(git rev-parse HEAD)" == "$trunk_head" ]] || {
    echo "FAIL: sandbox should be standing on the trunk head" >&2; exit 1; }
  if out="$(cut_sandbox --rc 1 --execute --confirm "CUT-v${cut_version}-rc.1" 2>&1)"; then
    echo "FAIL: cut tagged the trunk head instead of the release commit" >&2
    exit 1
  fi
  printf '%s\n' "$out" | grep -q 'not the expected release commit' || {
    echo "FAIL: cut stopped for the wrong reason: $out" >&2; exit 1; }
  [[ -z "$(git tag --list)" ]] || {
    echo "FAIL: a rejected cut still created a tag" >&2; exit 1; }

  # 2. On the release commit: the tag appears, and nothing else moves.
  git reset -q --hard "$release_commit"
  commits_before="$(git rev-list --count HEAD)"
  cut_sandbox --rc 1 --execute --confirm "CUT-v${cut_version}-rc.1" >/dev/null
  [[ "$(git rev-parse HEAD)" == "$release_commit" ]] || {
    echo "FAIL: cut moved HEAD; it must author nothing" >&2; exit 1; }
  [[ "$(git rev-list --count HEAD)" == "$commits_before" ]] || {
    echo "FAIL: cut added a commit; it must author nothing" >&2; exit 1; }
  [[ -z "$(git status --porcelain)" ]] || {
    echo "FAIL: cut left the worktree dirty; it must edit nothing" >&2; exit 1; }
  [[ "$(git rev-list -n1 "v${cut_version}-rc.1")" == "$release_commit" ]] || {
    echo "FAIL: the tag is not on the release commit" >&2; exit 1; }
  # The condition release.yml asserts before it publishes. A cut that
  # authored a commit would tag a commit no branch contains, and fail here.
  git merge-base --is-ancestor "v${cut_version}-rc.1^{commit}" refs/remotes/origin/develop || {
    echo "FAIL: the tagged commit is not contained in origin/develop" >&2; exit 1; }

  # 3. --push moves the tag and nothing else. The release commit is behind
  #    the trunk head here, so a cut that also pushed the branch would be
  #    rejected -- non-fast-forward locally, and by the protection hook if
  #    it forced. Both surface as a failing cut.
  origin_trunk_before="$(git -C "$cut_origin" rev-parse refs/heads/develop)"
  refs_before="$(git -C "$cut_origin" for-each-ref --format='%(refname)' | sort)"
  cut_sandbox --rc 2 --execute --push --confirm "CUT-v${cut_version}-rc.2" >/dev/null
  [[ "$(git -C "$cut_origin" rev-parse refs/heads/develop)" == "$origin_trunk_before" ]] || {
    echo "FAIL: cut --push moved the protected trunk on origin" >&2; exit 1; }
  refs_after="$(git -C "$cut_origin" for-each-ref --format='%(refname)' | sort)"
  new_refs="$(comm -13 <(printf '%s\n' "$refs_before") <(printf '%s\n' "$refs_after"))"
  [[ "$new_refs" == "refs/tags/v${cut_version}-rc.2" ]] || {
    echo "FAIL: cut --push sent origin more than the tag: ${new_refs:-<nothing>}" >&2; exit 1; }

  # 4. A commit origin/develop does not contain must not be tagged: that is
  #    the tag release.yml rejects, and the reason `cut` stopped authoring.
  git commit -q --allow-empty -m "local commit origin has never seen"
  local_head="$(git rev-parse HEAD)"
  if out="$("$repo_root/$script" cut --version "$cut_version" --release-branch develop \
      --expect-commit "$local_head" --rc 3 --execute \
      --confirm "CUT-v${cut_version}-rc.3" 2>&1)"; then
    echo "FAIL: cut tagged a commit origin/develop does not contain" >&2
    exit 1
  fi
  printf '%s\n' "$out" | grep -q 'is not an ancestor of origin/develop' || {
    echo "FAIL: cut stopped for the wrong reason: $out" >&2; exit 1; }
  git tag --list | grep -q "v${cut_version}-rc.3" && {
    echo "FAIL: a rejected cut still created a tag" >&2; exit 1; }

  # 5. Pending release metadata stops the cut, and says which files are
  #    pending -- `cut` must never finalize them itself.
  git reset -q --hard "$release_commit"
  if out="$("$repo_root/$script" cut --version 9.9.9 --release-branch develop \
      --rc 1 --execute --confirm "CUT-v9.9.9-rc.1" 2>&1)"; then
    echo "FAIL: cut tagged a tree whose release metadata is not final" >&2
    exit 1
  fi
  printf '%s\n' "$out" | grep -q 'is not final for 9.9.9' || {
    echo "FAIL: cut stopped for the wrong reason: $out" >&2; exit 1; }
  printf '%s\n' "$out" | grep -q 'files still needing release preparation' || {
    echo "FAIL: cut did not name the files still needing preparation" >&2; exit 1; }
  [[ -z "$(git status --porcelain)" ]] || {
    echo "FAIL: a refused cut left release metadata changes behind" >&2; exit 1; }
)

# publish-apt-repo argument and path validation fails closed.
if "$publish_apt_script" --output "$tmp/apt-out" --signing-key /nonexistent >/dev/null 2>&1; then
  echo "FAIL: publish-apt-repo must require --assets-dir" >&2
  exit 1
fi
if "$publish_apt_script" --assets-dir /nonexistent --output "$tmp/apt-out" --signing-key /nonexistent >/dev/null 2>&1; then
  echo "FAIL: publish-apt-repo must reject a missing assets directory" >&2
  exit 1
fi
# A SHA256SUMS .deb entry whose file is absent must abort publication
# (a partial download must never publish a partial package set). The
# check needs the script's tool prerequisites, so probe only where
# they exist; container CI covers it otherwise.
if command -v sha256sum >/dev/null 2>&1 && command -v gpg >/dev/null 2>&1 &&
   command -v dpkg-scanpackages >/dev/null 2>&1 && command -v apt-ftparchive >/dev/null 2>&1; then
  mkdir -p "$tmp/apt-partial"
  printf 'fixture deb\n' > "$tmp/apt-partial/tensorplate-common_0.1.0-1_all.deb"
  ( cd "$tmp/apt-partial" && sha256sum tensorplate-common_0.1.0-1_all.deb > SHA256SUMS )
  printf '%064d  tensorplate-cli_0.1.0-1_arm64.deb\n' 0 >> "$tmp/apt-partial/SHA256SUMS"
  if "$publish_apt_script" --assets-dir "$tmp/apt-partial" --output "$tmp/apt-partial-out" \
      --signing-key "$tmp/apt-partial/SHA256SUMS" --verify-keyring "$tmp/apt-partial/SHA256SUMS" \
      --allow-unverified-assets >/dev/null 2>&1; then
    echo "FAIL: publish-apt-repo must reject SHA256SUMS entries with missing .deb files" >&2
    exit 1
  fi
fi
grep -q 'name: Release' .github/workflows/release.yml
grep -q 'tools/release/build-release-artifacts.sh' .github/workflows/release.yml
grep -q 'draft="true"' .github/workflows/release.yml
grep -q 'publish-github' docs/release/runbook.md
grep -q 'gh release edit --draft=false --latest' docs/release/runbook.md
grep -q 'Protected environments with \*\*required reviewers\*\*' docs/release/runbook.md

# The secondary runtime set is declared in three places: the two release
# scripts and the copy list in the release workflow's amd64 job. Nothing else
# makes them agree, and drift is silent in the dangerous direction — a package
# the workflow stops copying is simply never staged, and collection then fails
# only at release time.
array_block() {
  awk -v name="$2" '
    $0 ~ ("^readonly " name "=\\(") { inside = 1; next }
    inside && /^[[:space:]]*\)/ { exit }
    inside { print $1 }
  ' "$1"
}
collect_set="$(array_block tools/release/build-release-artifacts.sh SECONDARY_ARCH_PACKAGES | sort)"
enforce_set="$(array_block tools/release/tensorplate-release.sh SECONDARY_ARCH_PACKAGES | sort)"
[ -n "$collect_set" ] || { echo "FAIL: SECONDARY_ARCH_PACKAGES not found in build-release-artifacts.sh" >&2; exit 1; }
if [ "$collect_set" != "$enforce_set" ]; then
  echo "FAIL: SECONDARY_ARCH_PACKAGES differs between the release scripts" >&2
  diff <(echo "$collect_set") <(echo "$enforce_set") >&2 || true
  exit 1
fi
workflow_copy_list="$(sed -n '/for pkg in tensorplate /,/^ *done$/p' .github/workflows/release.yml)"
[ -n "$workflow_copy_list" ] || { echo "FAIL: could not find the amd64 copy list in release.yml" >&2; exit 1; }
# Normalize everything that is not part of a package name to a single space
# so line continuations and the loop's trailing `;` do not defeat the match.
workflow_words=" $(printf '%s' "$workflow_copy_list" | tr -c 'A-Za-z0-9-' ' ' | tr -s ' ') "
for pkg in $collect_set; do
  case "$workflow_words" in
    *" ${pkg} "*) ;;
    *) echo "FAIL: release.yml's amd64 job does not copy ${pkg}" >&2; exit 1 ;;
  esac
done

mkdir -p "$tmp/artifacts"
for pkg in tensorplate-common tensorplate-backend-python-pytorch tensorplate-apt-source; do
  fixture_deb "$tmp/artifacts/${pkg}_0.1.0-1_all.deb" "$pkg" 0.1.0-1 all "fixture artifact for $pkg"
done
for pkg in \
  tensorplate-agent \
  tensorplate-serving \
  tensorplate-observability \
  tensorplate-cli \
  tensorplate; do
  fixture_deb "$tmp/artifacts/${pkg}_0.1.0-1_arm64.deb" "$pkg" 0.1.0-1 arm64 "fixture artifact for $pkg"
done
# The complete x86_64 runtime set ships alongside the arm64 target.
for pkg in \
  tensorplate-agent \
  tensorplate-serving \
  tensorplate-observability \
  tensorplate-cli \
  tensorplate; do
  fixture_deb "$tmp/artifacts/${pkg}_0.1.0-1_amd64.deb" "$pkg" 0.1.0-1 amd64 "fixture amd64 artifact for $pkg"
done
printf 'fixture installer\n' > "$tmp/artifacts/install.sh"
# The SDK distribution rides in the same signed manifest, and verify requires
# it on the publish path.
printf 'fixture wheel\n' > "$tmp/artifacts/tensorplate_python-0.1.0-py3-none-any.whl"
printf 'fixture sdist\n' > "$tmp/artifacts/tensorplate_python-0.1.0.tar.gz"

"$script" manifest \
  --version 0.1.0 \
  --tag v0.1.0 \
  --artifacts-dir "$tmp/artifacts" \
  --manifest "$tmp/tensorplate-v0.1.0-artifacts.json" \
  --checksums "$tmp/SHA256SUMS" >/dev/null

python3 - "$tmp/tensorplate-v0.1.0-artifacts.json" "$tmp/SHA256SUMS" <<'PY'
import json
import sys
from pathlib import Path

manifest = json.loads(Path(sys.argv[1]).read_text())
checksums = Path(sys.argv[2]).read_text().splitlines()
assert manifest["release"]["version"] == "0.1.0"
assert manifest["release"]["tag"] == "v0.1.0"
assert len(manifest["artifacts"]) == 16  # 8 primary + 5 amd64 + install.sh + wheel + sdist
assert len(checksums) == 17  # manifest self-digest + 16 artifacts
assert any(artifact["file"] == "tensorplate-common_0.1.0-1_all.deb" for artifact in manifest["artifacts"])
assert any(artifact["file"] == "tensorplate-apt-source_0.1.0-1_all.deb" for artifact in manifest["artifacts"])
assert any(artifact["file"] == "tensorplate_0.1.0-1_arm64.deb" for artifact in manifest["artifacts"])

# The whole x86_64 runtime set is present, and none of it is mislabelled as
# a desktop CLI asset: an operator reads target_os to decide what to install.
amd64 = {a["package"]: a for a in manifest["artifacts"] if a.get("architecture") == "amd64"}
assert set(amd64) == {
    "tensorplate-agent",
    "tensorplate-serving",
    "tensorplate-observability",
    "tensorplate-cli",
    "tensorplate",
}, sorted(amd64)
for package, artifact in amd64.items():
    assert "x86_64" in artifact["target_os"], (package, artifact["target_os"])
    assert "CLI" not in artifact["target_os"], (package, artifact["target_os"])
    assert "JetPack" not in artifact["target_os"], (package, artifact["target_os"])

# The primary target block still describes the Jetson arm64 target; the
# secondary architecture is carried per artifact, exactly as the desktop CLI
# always was. Consumers keyed on target.architecture must not shift.
assert manifest["target"]["architecture"] == "arm64"

# install.sh selects packages by (architecture in ("all", host_arch)) and
# requires EXACTLY ONE match per package. Now that a manifest carries two
# runtime architectures, prove it stays unambiguously resolvable from either
# host — an extra or missing match makes the installer refuse or, worse,
# install the wrong architecture's binary.
runtime_packages = [
    "tensorplate-common",
    "tensorplate-agent",
    "tensorplate-serving",
    "tensorplate-observability",
    "tensorplate-cli",
    "tensorplate-backend-python-pytorch",
    "tensorplate",
]
for host_arch in ("arm64", "amd64"):
    for package in runtime_packages:
        matches = [
            a for a in manifest["artifacts"]
            if a.get("package") == package
            and a.get("architecture") in ("all", host_arch)
        ]
        assert len(matches) == 1, (host_arch, package, [m["file"] for m in matches])
        # And the one it resolves to is never the other architecture's build.
        assert matches[0]["architecture"] in ("all", host_arch), (host_arch, matches[0])
PY

# A complete two-architecture release artifact set must verify clean. Without
# this the rejection cases below would still pass if the gate rejected
# everything.
"$script" verify \
  --skip-tag-verify \
  --version 0.1.0 \
  --tag v0.1.0 \
  --artifacts-dir "$tmp/artifacts" \
  --manifest "$tmp/tensorplate-v0.1.0-artifacts.json" \
  --checksums "$tmp/SHA256SUMS" >/dev/null

# Publish-grade releases require the COMPLETE amd64 runtime set. A partial
# set is the dangerous case: it generates and verifies clean under a rule
# that only checks one representative package. Drop each member in turn
# (snapshot flows below stay exempt).
for missing in \
  tensorplate-agent \
  tensorplate-serving \
  tensorplate-observability \
  tensorplate-cli \
  tensorplate; do
  rm -rf "$tmp/artifacts-partial-amd64"
  mkdir -p "$tmp/artifacts-partial-amd64"
  cp "$tmp/artifacts/"* "$tmp/artifacts-partial-amd64/"
  rm "$tmp/artifacts-partial-amd64/${missing}_0.1.0-1_amd64.deb"
  if "$script" manifest \
    --version 0.1.0 \
    --tag v0.1.0 \
    --artifacts-dir "$tmp/artifacts-partial-amd64" \
    --manifest "$tmp/partial-amd64-artifacts.json" \
    --checksums "$tmp/partial-amd64-SHA256SUMS" >/dev/null 2>&1; then
    echo "FAIL: manifest must reject a release artifact set without ${missing} amd64" >&2
    exit 1
  fi
done

# A package at an architecture the secondary runtime set does not declare is
# a staging mistake, not a bonus asset. The CLI is the discriminating case:
# the previous rule let tensorplate-cli publish at ANY extra architecture, so
# a fixture using any other package would be rejected by the old rule too and
# would prove nothing about the new one.
mkdir -p "$tmp/artifacts-stray-arch"
cp "$tmp/artifacts/"* "$tmp/artifacts-stray-arch/"
fixture_deb "$tmp/artifacts-stray-arch/tensorplate-cli_0.1.0-1_riscv64.deb" tensorplate-cli 0.1.0-1 riscv64 "stray artifact"
if "$script" manifest \
  --version 0.1.0 \
  --tag v0.1.0 \
  --artifacts-dir "$tmp/artifacts-stray-arch" \
  --manifest "$tmp/stray-arch-artifacts.json" \
  --checksums "$tmp/stray-arch-SHA256SUMS" >/dev/null 2>&1; then
  echo "FAIL: manifest must reject the CLI at an undeclared architecture" >&2
  exit 1
fi

# And a package that is not in the secondary set at all, at the secondary
# architecture. tensorplate-common is Architecture: all and is shared, so an
# amd64 build of it means something went wrong in staging.
mkdir -p "$tmp/artifacts-stray-pkg"
cp "$tmp/artifacts/"* "$tmp/artifacts-stray-pkg/"
fixture_deb "$tmp/artifacts-stray-pkg/tensorplate-common_0.1.0-1_amd64.deb" tensorplate-common 0.1.0-1 amd64 "stray artifact"
if "$script" manifest \
  --version 0.1.0 \
  --tag v0.1.0 \
  --artifacts-dir "$tmp/artifacts-stray-pkg" \
  --manifest "$tmp/stray-pkg-artifacts.json" \
  --checksums "$tmp/stray-pkg-SHA256SUMS" >/dev/null 2>&1; then
  echo "FAIL: manifest must reject a package outside the secondary runtime set" >&2
  exit 1
fi

# verify's package-name check is architecture-blind: an arm64 serving worker
# satisfies "tensorplate-serving is present". Strip only the amd64 serving
# worker — leaving its arm64 sibling in place — and refresh the checksum
# self-digest, so this exercises the per-architecture gate itself rather
# than a checksum mismatch or a missing package name.
python3 - "$tmp/tensorplate-v0.1.0-artifacts.json" "$tmp/SHA256SUMS" <<'PY'
import hashlib
import json
import sys
from pathlib import Path

manifest_path, checksums_path = map(Path, sys.argv[1:])
tampered_manifest = manifest_path.with_name("tampered-artifacts.json")
tampered_checksums = checksums_path.with_name("tampered-SHA256SUMS")
manifest = json.loads(manifest_path.read_text())
manifest["artifacts"] = [
    a for a in manifest["artifacts"]
    if not (a.get("package") == "tensorplate-serving" and a.get("architecture") == "amd64")
]
assert any(
    a.get("package") == "tensorplate-serving" and a.get("architecture") == "arm64"
    for a in manifest["artifacts"]
), "the arm64 sibling must survive or this tests the wrong rule"
tampered_manifest.write_text(json.dumps(manifest, indent=2) + "\n")
digest = hashlib.sha256(tampered_manifest.read_bytes()).hexdigest()
lines = [f"{digest}  {tampered_manifest.name}\n"]
lines.extend(f"{a['sha256']}  {a['file']}\n" for a in manifest["artifacts"])
tampered_checksums.write_text("".join(lines))
PY
if "$script" verify \
  --skip-tag-verify \
  --version 0.1.0 \
  --tag v0.1.0 \
  --artifacts-dir "$tmp/artifacts" \
  --manifest "$tmp/tampered-artifacts.json" \
  --checksums "$tmp/tampered-SHA256SUMS" >/dev/null 2>&1; then
  echo "FAIL: verify must reject a manifest missing the amd64 serving worker" >&2
  exit 1
fi

snapshot_version="0.1.0~dev.20260604.deadbeef1234"
snapshot_tag="snapshot-develop-deadbeef1234"
mkdir -p "$tmp/snapshot-artifacts" "$tmp/snapshot-built"
for pkg in tensorplate-common tensorplate-backend-python-pytorch tensorplate-apt-source; do
  fixture_deb "$tmp/snapshot-built/${pkg}_${snapshot_version}-1_all.deb" "$pkg" "${snapshot_version}-1" all "fixture snapshot artifact for $pkg"
done
for pkg in \
  tensorplate-agent \
  tensorplate-serving \
  tensorplate-observability \
  tensorplate-cli \
  tensorplate; do
  fixture_deb "$tmp/snapshot-built/${pkg}_${snapshot_version}-1_arm64.deb" "$pkg" "${snapshot_version}-1" arm64 "fixture snapshot artifact for $pkg"
done
stage_like_release "$tmp/snapshot-artifacts" "$snapshot_version" "$tmp/snapshot-built"/*.deb || exit 1
printf 'fixture snapshot installer\n' > "$tmp/snapshot-artifacts/install.sh"

"$script" manifest \
  --allow-snapshot-version \
  --version "$snapshot_version" \
  --tag "$snapshot_tag" \
  --release-branch develop \
  --artifacts-dir "$tmp/snapshot-artifacts" \
  --manifest "$tmp/tensorplate-${snapshot_tag}-artifacts.json" \
  --checksums "$tmp/snapshot-SHA256SUMS" >/dev/null

"$script" verify \
  --allow-snapshot-version \
  --skip-tag-verify \
  --version "$snapshot_version" \
  --tag "$snapshot_tag" \
  --artifacts-dir "$tmp/snapshot-artifacts" \
  --manifest "$tmp/tensorplate-${snapshot_tag}-artifacts.json" \
  --checksums "$tmp/snapshot-SHA256SUMS" >/dev/null

python3 - "$tmp/tensorplate-${snapshot_tag}-artifacts.json" "$tmp/snapshot-SHA256SUMS" <<'PY'
import json
import sys
from pathlib import Path

manifest = json.loads(Path(sys.argv[1]).read_text())
checksums = Path(sys.argv[2]).read_text().splitlines()
release = manifest["release"]
assert release["version"] == "0.1.0~dev.20260604.deadbeef1234"
assert release["tag"] == "snapshot-develop-deadbeef1234"
assert release["provenance"] == "local-source-snapshot"
assert release["unreleased"] is True
assert release["source_kind"] == "local-source-branch"
assert "local-source-snapshot" in release["labels"]
debs = [artifact for artifact in manifest["artifacts"] if artifact.get("package")]
assert len(debs) == 8, debs
# Named as GitHub would publish them, recorded at the version dpkg reports.
for artifact in debs:
    assert artifact["file"].startswith(
        f'{artifact["package"]}_0.1.0.dev.20260604.deadbeef1234-1_'
    ), artifact
    assert artifact["version"] == "0.1.0~dev.20260604.deadbeef1234-1", artifact
assert not [line for line in checksums if "~" in line], checksums
assert len(checksums) == 10  # manifest self-digest + 8 packages + install.sh
PY

if command -v dpkg >/dev/null 2>&1; then
  dpkg --compare-versions "${snapshot_version}-1" lt "0.1.0-1"
fi

# --- prepare is idempotent for the same version -------------------------
#
# Two regressions, one after the other. `prepare` used to REWRITE the top
# changelog stanza, so preparing 0.2.1 relabelled the published 0.1.2 entry
# and kept its body. Making it prepend then made a matching stanza an
# error -- and `cmd_cut` always runs prepare, so an already-prepared head
# could not be cut at all. It has to prepend once and then do nothing.
#
# Run against a minimal repo holding only the files prepare touches, so
# this exercises the real script rather than a copy of its logic.
prep="$tmp/prep"
mkdir -p "$prep/packaging/debian" "$prep/packaging/scripts"
for f in CMakeLists.txt Cargo.toml vcpkg.json CHANGELOG.md \
         packaging/VERSION packaging/debian/changelog packaging/scripts/install.sh; do
  mkdir -p "$prep/$(dirname "$f")"
  cp "$repo_root/$f" "$prep/$f"
done
(
  cd "$prep"
  git init -q -b prep-line .
  git add -A
  git -c user.email=t@example.com -c user.name=t commit -qm base
  # The stanza currently on top is the one a rewriting prepare would
  # relabel. Asserting on a FIXED older version instead would pass while
  # the top entry was silently overwritten -- which is how the erasing
  # behaviour survived the first version of this test.
  was_on_top="$(head -n 1 packaging/debian/changelog)"
  stanzas_before="$(grep -c '^tensorplate (' packaging/debian/changelog)"
  for attempt in 1 2; do
    "$repo_root/$script" prepare --version 9.9.9 --prep-branch prep-line \
      --execute --confirm PREPARE-v9.9.9 >/dev/null 2>&1 || {
        echo "FAIL: prepare attempt $attempt errored; an already-prepared head must still be preparable" >&2
        exit 1
      }
    git add -A
    git -c user.email=t@example.com -c user.name=t commit -qm "prepare $attempt" --allow-empty
  done
  stanzas="$(grep -c '^tensorplate (9.9.9-1)' packaging/debian/changelog)"
  [[ "$stanzas" == "1" ]] || {
    echo "FAIL: expected exactly one 9.9.9 stanza after two prepares, got $stanzas" >&2
    exit 1
  }
  grep -qF "$was_on_top" packaging/debian/changelog || {
    echo "FAIL: preparing a new version overwrote the stanza that was on top: $was_on_top" >&2
    exit 1
  }
  grep -q '^tensorplate (0.1.2-1)' packaging/debian/changelog || {
    echo "FAIL: preparing a new version erased the published 0.1.2 stanza" >&2
    exit 1
  }
  stanzas_after="$(grep -c '^tensorplate (' packaging/debian/changelog)"
  [[ "$stanzas_after" == "$((stanzas_before + 1))" ]] || {
    echo "FAIL: expected exactly one new stanza, went from $stanzas_before to $stanzas_after" >&2
    exit 1
  }
  grep -q 'TP_INSTALL_DEFAULT_VERSION:-9.9.9}' packaging/scripts/install.sh || {
    echo "FAIL: prepare left the installer default behind the release version" >&2
    exit 1
  }
)

# A release candidate must be distinguishable from the release it is a
# candidate for, and must survive every interface between the tag and the
# installed package. Built as plain X.Y.Z it was not: same dpkg version,
# so `apt` saw nothing to upgrade and anyone who installed a candidate was
# stranded on it.
#
# These execute the interfaces rather than grepping for them. An earlier
# version of this block asserted two assignment strings and passed while
# the candidate flow failed at its first artifact build.
(
  workflow=".github/workflows/release.yml"

  derive() {
    bash -c '
      tag="$1"
      version="${tag#v}"; version="${version%%-*}"
      deb_version="$version"; python_version="$version"
      if [[ "$tag" =~ -rc\.([1-9][0-9]*)$ ]]; then
        deb_version="${version}~rc.${BASH_REMATCH[1]}"
        python_version="${version}rc${BASH_REMATCH[1]}"
      fi
      printf "%s %s %s" "$version" "$deb_version" "$python_version"' _ "$1"
  }
  for fragment in 'deb_version="${version}~rc.${BASH_REMATCH[1]}"' \
                  'python_version="${version}rc${BASH_REMATCH[1]}"'; do
    grep -qF "$fragment" "$workflow" || {
      echo "FAIL: release.yml no longer derives versions as this test assumes: $fragment" >&2
      exit 1; }
  done

  read -r canon deb py <<<"$(derive v0.2.1-rc.1)"
  read -r f_canon f_deb f_py <<<"$(derive v0.2.1)"
  [[ "$deb" == "0.2.1~rc.1" && "$py" == "0.2.1rc1" && "$canon" == "0.2.1" ]] || {
    echo "FAIL: candidate identities wrong: $canon / $deb / $py" >&2; exit 1; }
  [[ "$f_canon" == "$canon" ]] || {
    echo "FAIL: a candidate and its release must share the canonical version" >&2; exit 1; }
  [[ "$f_deb" == "0.2.1" && "$f_py" == "0.2.1" ]] || {
    echo "FAIL: a final tag must not gain a prerelease suffix" >&2; exit 1; }
  [[ "$deb" != "$f_deb" ]] || { echo "FAIL: candidate and final share a package version" >&2; exit 1; }

  # 1. A real candidate must survive manifest generation AND verification.
  #    Checking require_version alone passed while the manifest layer still
  #    rejected every candidate artifact by name.
  fx="$(mktemp -d)"; art="$fx/artifacts"; mkdir -p "$art" "$fx/built"
  for pkg in tensorplate-common tensorplate-agent tensorplate-serving \
             tensorplate-observability tensorplate-cli \
             tensorplate-backend-python-pytorch tensorplate-apt-source tensorplate; do
    case "$pkg" in
      tensorplate-common|tensorplate-backend-python-pytorch|tensorplate-apt-source) a=all ;;
      *) a=arm64 ;;
    esac
    fixture_deb "$fx/built/${pkg}_${deb}-1_${a}.deb" "$pkg" "${deb}-1" "$a" "candidate fixture"
  done
  for pkg in tensorplate tensorplate-agent tensorplate-serving \
             tensorplate-observability tensorplate-cli; do
    fixture_deb "$fx/built/${pkg}_${deb}-1_amd64.deb" "$pkg" "${deb}-1" amd64 "candidate fixture"
  done
  stage_like_release "$art" "$deb" "$fx/built"/*.deb || { rm -rf "$fx"; exit 1; }
  : >"$art/tensorplate_python-${py}-py3-none-any.whl"
  : >"$art/tensorplate_python-${py}.tar.gz"
  : >"$art/install.sh"

  tools/release/tensorplate-release.sh manifest \
    --version "$canon" --deb-version "$deb" --python-version "$py" \
    --tag v0.2.1-rc.1 --artifacts-dir "$art" \
    --manifest "$fx/manifest.json" --checksums "$fx/SHA256SUMS" --arch arm64 \
    >/dev/null 2>&1 || {
      echo "FAIL: manifest generation rejects a real candidate artifact set" >&2
      rm -rf "$fx"; exit 1; }

  tools/release/tensorplate-release.sh verify \
    --version "$canon" --deb-version "$deb" --python-version "$py" \
    --tag v0.2.1-rc.1 --artifacts-dir "$art" \
    --manifest "$fx/manifest.json" --checksums "$fx/SHA256SUMS" --skip-tag-verify \
    >/dev/null 2>&1 || {
      echo "FAIL: verification rejects the candidate manifest it just generated" >&2
      rm -rf "$fx"; exit 1; }

  # The manifest records the canonical version; only the artifacts carry
  # the candidate spelling.
  python3 -c "
import json, sys
m = json.load(open('$fx/manifest.json'))
if m['release']['version'] != '$canon':
    sys.exit(f\"FAIL: manifest records {m['release']['version']!r}, not the canonical version\")
" || { rm -rf "$fx"; exit 1; }

  # Control: a fixture that cannot fail proves nothing. Under a final tag
  # the derived package version is the canonical one, so these candidate
  # artifacts must be refused by name.
  if tools/release/tensorplate-release.sh manifest \
      --version "$canon" --tag "v$canon" --artifacts-dir "$art" \
      --manifest "$fx/control.json" --checksums "$fx/control.sums" --arch arm64 \
      >/dev/null 2>&1; then
    echo "FAIL: the manifest layer accepted candidate artifacts under a final tag" >&2
    rm -rf "$fx"; exit 1
  fi
  # The driver must enforce the tuple itself: manifest, verify and publish
  # are supported entry points for recovery, and were fail-open. A verifier
  # given RC9 identities against an RC1 tag exited 0 with "manifest verified".
  if tools/release/tensorplate-release.sh verify \
      --version "$canon" --deb-version 0.2.1~rc.9 --python-version 0.2.1rc9 \
      --tag v0.2.1-rc.1 --artifacts-dir "$art" \
      --manifest "$fx/manifest.json" --checksums "$fx/SHA256SUMS" --skip-tag-verify \
      >/dev/null 2>&1; then
    echo "FAIL: the verifier accepted package/wheel versions contradicting the tag" >&2
    rm -rf "$fx"; exit 1
  fi
  if tools/release/tensorplate-release.sh manifest \
      --version "$canon" --deb-version "$deb" --python-version "$py" \
      --tag v0.2.1-rc.2 --artifacts-dir "$art" \
      --manifest "$fx/mismatch.json" --checksums "$fx/mismatch.sums" --arch arm64 \
      >/dev/null 2>&1; then
    echo "FAIL: manifest generation accepted an RC2 tag against RC1 artifacts" >&2
    rm -rf "$fx"; exit 1
  fi

  # Each artifact's metadata records its own version, the way the Debian
  # entries already do; recording the canonical one made the manifest
  # describe tensorplate_python-0.2.1rc1 as version 0.2.1.
  python3 - "$fx/manifest.json" "$py" <<'PYSDK' || { rm -rf "$fx"; exit 1; }
import json, sys
manifest_path, expected = sys.argv[1], sys.argv[2]
manifest = json.load(open(manifest_path))
sdk = [a for a in manifest["artifacts"] if "tensorplate_python-" in a.get("file", "")]
if not sdk:
    sys.exit("FAIL: the RC manifest carries no SDK artifacts")
wrong = [a for a in sdk if a.get("version") != expected]
if wrong:
    sys.exit(
        "FAIL: SDK metadata records %r, not the artifact version %r"
        % (wrong[0].get("version"), expected)
    )
PYSDK
  rm -rf "$fx"

  # 2. The installer must accept what the build stamps as its default.
  #    install.sh refuses the Debian form, so the tag is what gets stamped.
  ( VERSION_INPUT="v0.2.1-rc.1"
    die() { echo "FAIL: install.sh rejects the stamped candidate default: $*" >&2; exit 1; }
    if [[ "$VERSION_INPUT" == v* ]]; then
      TAG="$VERSION_INPUT"; RELEASE_VERSION="${VERSION_INPUT#v}"; RELEASE_VERSION="${RELEASE_VERSION%%-*}"
    else RELEASE_VERSION="$VERSION_INPUT"; TAG="v${VERSION_INPUT}"; fi
    [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-rc\.[1-9][0-9]*)?$ ]] || die "$TAG"
    [[ "$RELEASE_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "$RELEASE_VERSION" ) || exit 1
  grep -q 'install_default="${TAG:-$VERSION}"' tools/release/build-release-artifacts.sh || {
    echo "FAIL: the installer is not stamped with the tag form" >&2; exit 1; }

  # 3. Package filenames and the secondary-arch collector must agree, and
  #    both must use the Debian version.
  grep -q 'name "${pkg}_${DEB_VERSION}-\*_${SECONDARY_ARCH}.deb"' \
    tools/release/build-release-artifacts.sh || {
    echo "FAIL: the secondary-arch collector does not look for the Debian version" >&2; exit 1; }
  # The primary collector too: changelog staging names the built packages
  # with the Debian version, so a canonical pattern matches nothing.
  grep -q 'name "${pkg}_${DEB_VERSION}-\*_\*.deb"' \
    tools/release/build-release-artifacts.sh || {
    echo "FAIL: the primary collector does not look for the Debian version" >&2; exit 1; }
  grep -q '${pkg}_${DEB_VERSION}-\*_${TARGET_ARCH}.deb|${pkg}_${DEB_VERSION}-\*_all.deb' \
    tools/release/build-release-artifacts.sh || {
    echo "FAIL: the primary collector case patterns still use the canonical version" >&2; exit 1; }

  # The tag and the package version are one identity. Validating only a
  # shared base accepted `TAG=v0.2.1 DEB_VERSION=0.2.1~rc.1` and
  # `TAG=v0.2.1-rc.2 DEB_VERSION=0.2.1~rc.1`, either of which ships Rust
  # binaries (identity from the tag) contradicting the packages (identity
  # from the Debian version).
  #
  # This invokes the real builder. An earlier version reimplemented the
  # check inline and passed with the builder's own validation removed.
  tuple_out() {
    local scratch; scratch="$(mktemp -d)"
    tools/release/build-release-artifacts.sh --version 0.2.1 --tag "$1" --deb-version "$2" \
      --artifacts-dir "$scratch/a" --manifest "$scratch/m.json" \
      --checksums "$scratch/s" 2>&1 | head -1
    rm -rf "$scratch"
  }
  for bad in "v0.2.1|0.2.1~rc.1" "v0.2.1-rc.2|0.2.1~rc.1"; do
    if ! tuple_out "${bad%%|*}" "${bad##*|}" | grep -q 'contradicts --tag'; then
      echo "FAIL: the builder accepted contradictory identities ${bad%%|*} / ${bad##*|}" >&2
      exit 1
    fi
  done
  if tuple_out v0.2.1-rc.1 '0.2.1~rc.1' | grep -q 'contradicts --tag'; then
    echo "FAIL: the builder rejected a consistent candidate pair" >&2
    exit 1
  fi

  # Ordering: the Rust crates are compiled by the `cargo` call, so the
  # override has to be exported before it. Exported afterwards, ARM Rust
  # binaries reported the final version while ARM C++ reported the candidate.
  export_line="$(grep -n 'export TP_RELEASE_VERSION' tools/release/build-release-artifacts.sh | head -1 | cut -d: -f1)"
  cargo_line="$(grep -n '^cargo "' tools/release/build-release-artifacts.sh | head -1 | cut -d: -f1)"
  [[ -n "$export_line" && -n "$cargo_line" && "$export_line" -lt "$cargo_line" ]] || {
    echo "FAIL: TP_RELEASE_VERSION (line ${export_line:-none}) must be exported before cargo runs (line ${cargo_line:-none})" >&2
    exit 1; }

  # The Rust identity must stay a numeric-leading semver for every build
  # kind. Deriving it from the tag gave snapshots `snapshot-<branch>-<sha>`,
  # and protocol's loose parser takes the segment before the first hyphen,
  # so that parsed as 0.0.0 and a snapshot agent rejected the shipped
  # backend and otherwise compatible bundles on version-floor checks.
  #
  # Evaluate the expression the script actually ships, not a copy of it.
  export_expr="$(grep -m1 '^export TP_RELEASE_VERSION=' tools/release/build-release-artifacts.sh)"
  [[ -n "$export_expr" ]] || {
    echo "FAIL: build-release-artifacts.sh no longer exports TP_RELEASE_VERSION" >&2; exit 1; }
  grep -q 'TP_RELEASE_VERSION="${TAG#v}"' tools/release/build-release-artifacts.sh && {
    echo "FAIL: the Rust identity is derived from the tag again; snapshot tags are not versions" >&2
    exit 1; }
  for case in "0.2.1|0.2.1" "0.2.1~rc.1|0.2.1-rc.1" "0.2.1~dev.20260903.abc123|0.2.1-dev.20260903.abc123"; do
    got="$(DEB_VERSION="${case%%|*}" bash -c "$export_expr"'; printf %s "$TP_RELEASE_VERSION"')"
    [[ "$got" == "${case##*|}" ]] || {
      echo "FAIL: package version ${case%%|*} yields runtime identity '$got', expected ${case##*|}" >&2
      exit 1; }
    [[ "$got" =~ ^[0-9] ]] || {
      echo "FAIL: runtime identity '$got' does not begin with a number; it parses as 0.0.0" >&2
      exit 1; }
  done

  # Both architectures must give the C++ build its suffix, or a candidate
  # ships Rust and C++ binaries that disagree.
  grep -q 'DTP_RUNTIME_VERSION_SUFFIX="$runtime_suffix"' "$workflow" || {
    echo "FAIL: the amd64 C++ build receives no runtime version suffix" >&2; exit 1; }
  grep -q 'DEB_VERSION: ${{ needs.meta.outputs.deb_version }}' "$workflow" || {
    echo "FAIL: the amd64 job does not receive the Debian version" >&2; exit 1; }

  # 4. Both changelog paths must stamp the Debian version, or dpkg names the
  #    package after the final release regardless of what was passed.
  stage_dir="$(mktemp -d)"; mkdir -p "$stage_dir/packaging/debian"
  printf 'tensorplate (0.2.1-1) unstable; urgency=medium\n\n  * Release.\n' \
    >"$stage_dir/packaging/debian/changelog"
  # Read the staged line inside the subshell: write_staged_changelog
  # installs an EXIT trap that restores the tree's changelog, which in a
  # real build fires only after the packages are built.
  repo_root="$PWD"
  staged="$( cd "$stage_dir"
    CHANGELOG_BACKUP=""; VERSION=0.2.1; DEB_VERSION=0.2.1~rc.1
    : "$CHANGELOG_BACKUP" "$VERSION" "$DEB_VERSION"  # consumed by the evaluated bodies
    eval "$(sed -n '/^restore_staged_changelog()/,/^}/p;/^write_staged_changelog()/,/^}$/p' \
      "$repo_root/tools/release/build-release-artifacts.sh")"
    write_staged_changelog unstable
    head -1 packaging/debian/changelog )"
  printf '%s' "$staged" | grep -q '0\.2\.1~rc\.1-1' || {
    echo "FAIL: the ARM build does not stamp the candidate version into debian/changelog" >&2
    rm -rf "$stage_dir"; exit 1; }
  head -1 "$stage_dir/packaging/debian/changelog" | grep -q '0\.2\.1-1' || {
    echo "FAIL: the tree's changelog was not restored after the build" >&2
    rm -rf "$stage_dir"; exit 1; }
  rm -rf "$stage_dir"

  python3 - "$workflow" <<'PYAMD'
import subprocess, sys, tempfile, pathlib, yaml
w = yaml.safe_load(open(sys.argv[1]))
step = next(s for s in w["jobs"]["build_packages_amd64"]["steps"]
            if s.get("name") == "Build amd64 runtime packages")
body = step["run"].split("packaging/scripts/build-deb.sh")[0]
d = tempfile.mkdtemp(); pathlib.Path(d, "packaging/debian").mkdir(parents=True)
pathlib.Path(d, "packaging/debian/changelog").write_text(
    "tensorplate (0.2.1-1) unstable; urgency=medium\n\n  * Release.\n")
r = subprocess.run(["bash", "-c", body], cwd=d,
                   env={"DEB_VERSION": "0.2.1~rc.1", "PATH": "/usr/bin:/bin"},
                   capture_output=True, text=True)
head = pathlib.Path(d, "packaging/debian/changelog").read_text().splitlines()[0]
if r.returncode != 0 or "0.2.1~rc.1-1" not in head:
    sys.exit(f"FAIL: the amd64 job does not stamp the candidate version (got {head!r})")
PYAMD

  # 5. The binaries must say which build they are. Cargo metadata cannot:
  #    a candidate is built from the tree that says 0.2.1, so without the
  #    override `tensorplate --version` printed the final release's string.
  if command -v cargo >/dev/null 2>&1; then
    for site in cli/src/lib.rs observability/src/lib.rs protocol/rust/src/lib.rs \
                agent/src/main.rs observability/src/main.rs; do
      grep -q 'TP_RELEASE_VERSION' "$site" || {
        echo "FAIL: $site does not honour the release-version override" >&2; exit 1; }
    done
    out="$(TP_RELEASE_VERSION=0.2.1-rc.1 cargo run -q -p tensorplate-cli --bin tensorplate -- --version 2>/dev/null | head -1)"
    [[ "$out" == *"0.2.1-rc.1"* ]] || {
      echo "FAIL: a candidate build reports '$out'; it must name the candidate" >&2; exit 1; }
    plain="$(cargo run -q -p tensorplate-cli --bin tensorplate -- --version 2>/dev/null | head -1)"
    [[ "$plain" != *"-rc."* ]] || {
      echo "FAIL: a build with no override leaked a candidate version: '$plain'" >&2; exit 1; }
    echo "candidate binaries report their own version (built and executed)"
  else
    echo "FAIL: cargo is required to check that a candidate build reports its version" >&2
    exit 1
  fi

  if command -v dpkg >/dev/null 2>&1; then
    dpkg --compare-versions "${deb}-1" lt "${f_deb}-1" || {
      echo "FAIL: ${deb}-1 must sort below ${f_deb}-1 or there is no upgrade path" >&2; exit 1; }
    echo "candidate versions sort below their release (dpkg-verified)"
  else
    echo "candidate version derivation checked; dpkg absent, ordering NOT verified here"
  fi
)

# The evidence gate must be reachable from the release workflow, and
# every build must sit behind it. A gate that exists but nothing depends
# on is an optional step wearing a gate's name -- and the failure is
# silent, because the workflow still runs green while shipping
# unevidenced Production claims.
(
  workflow=".github/workflows/release.yml"
  # Declared, not assumed. CI provided PyYAML transitively while the
  # documented standalone run of this suite failed on a developer machine
  # with a bare ModuleNotFoundError -- and that run is the one the release
  # runbook tells you to make before tagging.
  python3 -c 'import yaml' 2>/dev/null || {
    echo "FAIL: this suite needs PyYAML to read the release workflow." >&2
    echo "      Install the release tooling dependencies with:" >&2
    echo "        python3 -m pip install -r tools/release/requirements.txt" >&2
    exit 1
  }
  python3 - "$workflow" <<'PYCHECK'
import sys, yaml

with open(sys.argv[1], encoding="utf-8") as handle:
    workflow = yaml.safe_load(handle)
jobs = workflow.get("jobs", {})

gate = jobs.get("evidence_gate")
if gate is None:
    sys.exit("FAIL: release.yml has no evidence_gate job")

runs = "\n".join(
    str(step.get("run", "")) for step in gate.get("steps", [])
)
if "check-evidence-bundles.sh" not in runs:
    sys.exit("FAIL: the evidence gate does not run the completeness check")

def needs_of(name):
    declared = jobs.get(name, {}).get("needs", [])
    return [declared] if isinstance(declared, str) else list(declared)

def gated(name, seen=None):
    """Whether a job sits behind the gate, directly or through its deps."""
    seen = seen or set()
    if name in seen:
        return False
    seen.add(name)
    for dependency in needs_of(name):
        if dependency == "evidence_gate" or gated(dependency, seen):
            return True
    return False

# Every job that builds or publishes an artifact. `meta` resolves
# metadata and the gate itself obviously cannot depend on itself.
exempt = {"meta", "evidence_gate"}
ungated = sorted(name for name in jobs if name not in exempt and not gated(name))
if ungated:
    sys.exit(
        "FAIL: these release jobs do not sit behind the evidence gate: "
        + ", ".join(ungated)
    )
print("evidence gate blocks every build and publish job")
PYCHECK
)

# The gate's enforcement policy, executed rather than read.
#
# Which tags the gate blocks is decided by a few lines of shell inside the
# workflow, and getting it wrong fails in both directions. Enforcing on a
# release candidate deadlocked the release: the harnesses validate published
# candidate artifacts, so no candidate could publish until evidence existed,
# and none could exist until a candidate had published. Not enforcing on the
# final tag would ship a Production claim nobody evidenced. So the step's own
# script is lifted out of release.yml and run against a stub checker for
# every combination that matters.
(
  workflow=".github/workflows/release.yml"
  python3 - "$workflow" "$tmp/gate-step.sh" <<'PYGATE'
import sys, yaml

workflow_path, out_path = sys.argv[1:]
workflow = yaml.safe_load(open(workflow_path, encoding="utf-8"))
steps = workflow["jobs"]["evidence_gate"]["steps"]
runs = [s["run"] for s in steps if "check-evidence-bundles.sh" in str(s.get("run", ""))]
if len(runs) != 1:
    sys.exit(f"FAIL: expected exactly one gate step running the checker, found {len(runs)}")
open(out_path, "w", encoding="utf-8").write(runs[0])
PYGATE

  # A checker stand-in whose verdict each case chooses. The step invokes it by
  # its repository path, so the stub lives at that path inside a scratch tree.
  mkdir -p "$tmp/gate-tree/tools/release"
  printf '#!/bin/sh\nexit "${TP_GATE_STUB_STATUS}"\n' >"$tmp/gate-tree/tools/release/check-evidence-bundles.sh"
  chmod +x "$tmp/gate-tree/tools/release/check-evidence-bundles.sh"

  gate_exit() {
    local publish="$1" prerelease="$2" checker="${3:-0}" status=0
    (
      cd "$tmp/gate-tree"
      PUBLISH="$publish" PRERELEASE="$prerelease" VERSION="0.2.1" \
        TP_GATE_STUB_STATUS="$checker" \
          bash --noprofile --norc -e -o pipefail "$tmp/gate-step.sh" >/dev/null 2>&1
    ) || status=$?
    printf '%s' "$status"
  }

  expect() {
    local what="$1" want="$2" got="$3"
    if [ "$want" != "$got" ]; then
      echo "FAIL: evidence gate: ${what}: expected exit ${want}, got ${got}" >&2
      exit 1
    fi
  }

  # Evidence complete: nothing is ever blocked.
  expect "a final release with complete evidence publishes"      0 "$(gate_exit true false 0)"
  expect "a candidate with complete evidence publishes"          0 "$(gate_exit true true 0)"
  expect "a build-only run with complete evidence proceeds"      0 "$(gate_exit false false 0)"

  # Evidence incomplete: only the final release is refused.
  expect "a final release with incomplete evidence is refused"   1 "$(gate_exit true false 1)"
  expect "a candidate with incomplete evidence still publishes"  0 "$(gate_exit true true 1)"
  expect "a build-only run with incomplete evidence proceeds"    0 "$(gate_exit false false 1)"
  # Fail closed on a missing value. If the metadata job ever stops emitting
  # `prerelease`, a final release must not be mistaken for a candidate and
  # waved through -- the exemption has to be earned by saying "true".
  expect "a release with no prerelease value is treated as final" 1 "$(gate_exit true "" 1)"

  # A checker that did not reach a verdict is never waived, on any tag.
  expect "a final release whose checker did not run is refused"  2 "$(gate_exit true false 2)"
  expect "a candidate whose checker did not run is refused"      2 "$(gate_exit true true 2)"
  expect "a build-only run whose checker did not run is refused" 2 "$(gate_exit false false 2)"

  # Match the Actions shell's inherited errexit above: invoking plain bash
  # hid the candidate deadlock because the workflow's own `set -uo pipefail`
  # did not turn errexit off. Only the documented incomplete-evidence status
  # is waivable. Preserve every other failure, including unexpected checker
  # errors, launch failures (126/127), and termination (137).
  for checker_status in 3 126 127 137 255; do
    expect "final release preserves checker failure ${checker_status}" \
      "$checker_status" "$(gate_exit true false "$checker_status")"
    expect "candidate preserves checker failure ${checker_status}" \
      "$checker_status" "$(gate_exit true true "$checker_status")"
    expect "build-only run preserves checker failure ${checker_status}" \
      "$checker_status" "$(gate_exit false false "$checker_status")"
  done

  # The real checker must distinguish incomplete evidence from a Python
  # parser failure. A stub returning 2 cannot catch an exception escaping
  # with status 1 and being waived by this same workflow step.
  cp tools/release/check-evidence-bundles.sh "$tmp/gate-tree/tools/release/check-evidence-bundles.sh"
  mkdir -p "$tmp/gate-tree/config/platform/rows" "$tmp/gate-tree/config/schemas"
  cp config/schemas/lifecycle_report.json "$tmp/gate-tree/config/schemas/lifecycle_report.json"
  cat >"$tmp/gate-row.json" <<'JSON'
{
  "row_id": "synthetic-row",
  "support_level": "Production",
  "provenance": "recorded",
  "evidence": { "location": "evidence/synthetic-row/" }
}
JSON
  cp "$tmp/gate-row.json" "$tmp/gate-tree/config/platform/rows/synthetic-row.json"
  expect "the real checker refuses final releases with missing evidence" 1 "$(gate_exit true false)"
  expect "the real checker's incomplete verdict permits candidates" 0 "$(gate_exit true true)"
  expect "the real checker's incomplete verdict permits build-only runs" 0 "$(gate_exit false false)"

  printf '{\n' >"$tmp/gate-tree/config/platform/rows/synthetic-row.json"
  expect "a candidate cannot waive malformed registry JSON" 2 "$(gate_exit true true)"
  expect "a build-only run cannot waive malformed registry JSON" 2 "$(gate_exit false false)"

  cp "$tmp/gate-row.json" "$tmp/gate-tree/config/platform/rows/synthetic-row.json"
  printf '{\n' >"$tmp/gate-tree/config/schemas/lifecycle_report.json"
  expect "a candidate cannot waive malformed schema JSON" 2 "$(gate_exit true true)"
  expect "a build-only run cannot waive malformed schema JSON" 2 "$(gate_exit false false)"

  echo "evidence gate enforces on final releases only, and never waives a checker that did not run"
)

# --- schema version metadata walk, against crafted schema trees -----------
#
# The sandbox cut above runs check_version_files over the real schemas, so it
# is the positive control. This lifts the same walk out of the driver and
# runs it over a copy of the schemas with one fault at a time: a document on
# its own version track may list versions only at its root, and every other
# schema stays pinned to the protocol version.
(
  walk="$(awk '/^  if python3 - "\$protocol" <<.PY.$/ { on = 1; next } on && /^PY$/ { exit } on' "$script")"
  [[ "$walk" == *'state_tracks = {"protocol/schemas/agent_state.json"}'* ]] || {
    echo "FAIL: could not lift the schema version walk out of $script" >&2; exit 1; }
  protocol_version="$(sed -n 's/^pub const PROTOCOL_VERSION: &str = "\(.*\)";$/\1/p' protocol/rust/src/lib.rs)"
  [[ -n "$protocol_version" ]] || { echo "FAIL: PROTOCOL_VERSION not found" >&2; exit 1; }
  tree="$tmp/schema-walk"

  reset_tree() {
    rm -rf "$tree"
    mkdir -p "$tree/config/schemas" "$tree/protocol/schemas"
    cp config/schemas/*.json "$tree/config/schemas/"
    cp protocol/schemas/*.json "$tree/protocol/schemas/"
  }
  mutate() {
    python3 - "$tree/$1" "$2" <<'PY'
import json
import sys

path, edit = sys.argv[1], sys.argv[2]
with open(path, encoding="utf-8") as handle:
    d = json.load(handle)
exec(edit)
with open(path, "w", encoding="utf-8") as handle:
    json.dump(d, handle)
PY
  }
  expect_walk() {
    local what="$1" want="$2" needle="${3:-}" out status=0
    out="$(cd "$tree" && python3 -c "$walk" "$protocol_version" 2>&1)" || status=$?
    if [[ "$want" == pass ]]; then
      [[ $status -eq 0 ]] || { echo "FAIL: schema walk: ${what}: ${out}" >&2; exit 1; }
    elif [[ $status -eq 0 || "$out" != *"$needle"* ]]; then
      echo "FAIL: schema walk: ${what}: expected a failure naming '${needle}', got status ${status}: ${out}" >&2
      exit 1
    fi
  }

  reset_tree
  expect_walk "the repository's schemas" pass

  mutate protocol/schemas/agent_state.json 'd["properties"]["schema_version"] = {"type": "string", "const": "0.1"}'
  expect_walk "a state-track root without its version list" fail \
    "protocol/schemas/agent_state.json: root schema_version must be a non-empty enum"
  reset_tree
  mutate protocol/schemas/agent_state.json 'd["properties"]["schema_version"]["enum"] = []'
  expect_walk "a state-track root with an empty version list" fail \
    "protocol/schemas/agent_state.json: root schema_version must be a non-empty enum"
  reset_tree
  mutate protocol/schemas/agent_state.json 'd["properties"]["schema_version"]["const"] = "0.1"'
  expect_walk "a state-track root that also pins one version" fail \
    "protocol/schemas/agent_state.json: root schema_version must be a non-empty enum"
  reset_tree
  mutate protocol/schemas/agent_state.json 'd["properties"]["schema_version"]["enum"] = ["0.1", 0.2]'
  expect_walk "a state-track root listing a non-string version" fail \
    "protocol/schemas/agent_state.json: root schema_version must be a non-empty enum"
  reset_tree
  mutate protocol/schemas/agent_state.json 'd["allOf"][0]["if"]["properties"]["schema_version"]["const"] = "0.9"'
  expect_walk "a state-track branch naming a version the root does not list" fail \
    "protocol/schemas/agent_state.json: schema_version const '0.9' is outside the file's versions"
  reset_tree
  mutate protocol/schemas/agent_state.json 'd["allOf"][0]["if"]["properties"]["schema_version"] = {"enum": ["0.1", "0.2"]}'
  expect_walk "a state-track branch listing versions" fail \
    "protocol/schemas/agent_state.json: only the root schema_version may list versions"
  reset_tree
  mutate protocol/schemas/agent_state.json 'd["allOf"][0]["if"]["properties"]["schema_version"] = True'
  expect_walk "a state-track branch accepting any version" fail \
    "protocol/schemas/agent_state.json: schema_version must be an object schema, found True"
  reset_tree
  mutate protocol/schemas/deploy_transaction.json 'd["properties"]["schema_version"] = True'
  expect_walk "a schema accepting any version" fail \
    "protocol/schemas/deploy_transaction.json: schema_version must be an object schema, found True"
  reset_tree
  mutate protocol/schemas/deploy_transaction.json 'd["properties"]["schema_version"] = {"type": "string", "enum": ["0.1", "0.2"]}'
  expect_walk "a schema outside the state-track list listing versions" fail \
    "protocol/schemas/deploy_transaction.json: schema_version const None is not '${protocol_version}'"
  reset_tree
  mutate protocol/schemas/deploy_transaction.json 'd["properties"]["schema_version"] = {"type": "string", "const": "0.2"}'
  expect_walk "a schema outside the state-track list at another version" fail \
    "protocol/schemas/deploy_transaction.json: schema_version const '0.2' is not '${protocol_version}'"

  echo "schema version walk admits a version list only on state-track documents"
)

printf 'release script checks green\n'
