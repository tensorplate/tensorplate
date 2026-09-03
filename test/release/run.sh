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
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

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

# Patch tags live on the per-minor maintenance line, not per-version
# release branches.
cut_dry_run="$("$script" cut --version 0.1.2 --final --dry-run)"
printf '%s\n' "$cut_dry_run" | grep -q 'release branch: release/0.1' || {
  echo "FAIL: cut must default to the release/0.1 maintenance line" >&2
  exit 1
}

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

# The amd64 C++ build must pin the DWARF version. jammy's dwz (0.14) cannot
# read DWARF 5's .debug_addr section, and dh_dwz turns that into a hard
# dpkg-buildpackage failure — so dropping this flag breaks the release job at
# package-build time, which no PR check exercises because that job only runs
# on release tags.
# Strip comment lines before matching: the comment explaining WHY the flag is
# there also contains the flag, so a bare search is satisfied by the prose and
# would keep passing after the real argument was deleted.
amd64_cxx_build="$(sed -n '/Build amd64 serving worker/,/cmake --build/p' .github/workflows/release.yml |
  grep -v '^[[:space:]]*#')"
[ -n "$amd64_cxx_build" ] || { echo "FAIL: could not find the amd64 serving worker build in release.yml" >&2; exit 1; }
printf '%s\n' "$amd64_cxx_build" | grep -Eq '^[[:space:]]*-DCMAKE_CXX_FLAGS=.*-gdwarf-[0-9]' || {
  echo "FAIL: the amd64 serving worker build must pass -gdwarf-<n> via CMAKE_CXX_FLAGS; jammy's dwz cannot read DWARF 5's .debug_addr and dh_dwz turns that into a hard package-build failure" >&2
  exit 1
}

mkdir -p "$tmp/artifacts"
for pkg in tensorplate-common tensorplate-backend-python-pytorch tensorplate-apt-source; do
  printf 'fixture artifact for %s\n' "$pkg" > "$tmp/artifacts/${pkg}_0.1.0-1_all.deb"
done
for pkg in \
  tensorplate-agent \
  tensorplate-serving \
  tensorplate-observability \
  tensorplate-cli \
  tensorplate; do
  printf 'fixture artifact for %s\n' "$pkg" > "$tmp/artifacts/${pkg}_0.1.0-1_arm64.deb"
done
# The complete x86_64 runtime set ships alongside the arm64 target.
for pkg in \
  tensorplate-agent \
  tensorplate-serving \
  tensorplate-observability \
  tensorplate-cli \
  tensorplate; do
  printf 'fixture amd64 artifact for %s\n' "$pkg" > "$tmp/artifacts/${pkg}_0.1.0-1_amd64.deb"
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
printf 'stray artifact\n' > "$tmp/artifacts-stray-arch/tensorplate-cli_0.1.0-1_riscv64.deb"
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
printf 'stray artifact\n' > "$tmp/artifacts-stray-pkg/tensorplate-common_0.1.0-1_amd64.deb"
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
mkdir -p "$tmp/snapshot-artifacts"
for pkg in tensorplate-common tensorplate-backend-python-pytorch tensorplate-apt-source; do
  printf 'fixture snapshot artifact for %s\n' "$pkg" > "$tmp/snapshot-artifacts/${pkg}_${snapshot_version}-1_all.deb"
done
for pkg in \
  tensorplate-agent \
  tensorplate-serving \
  tensorplate-observability \
  tensorplate-cli \
  tensorplate; do
  printf 'fixture snapshot artifact for %s\n' "$pkg" > "$tmp/snapshot-artifacts/${pkg}_${snapshot_version}-1_arm64.deb"
done
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
assert any("~dev.20260604.deadbeef1234-1" in artifact["file"] for artifact in manifest["artifacts"])
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
# candidate for. Built as plain X.Y.Z it was not: same dpkg version, so
# `apt` saw nothing to upgrade and anyone who installed a candidate was
# stranded on it, with `tensorplate --version` reporting the final
# release's string.
(
  workflow=".github/workflows/release.yml"

  # Exercise the workflow's own derivation rather than a copy of it.
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

  # The derivation must still be the one the workflow ships.
  for fragment in 'deb_version="${version}~rc.${BASH_REMATCH[1]}"' \
                  'python_version="${version}rc${BASH_REMATCH[1]}"'; do
    grep -qF "$fragment" "$workflow" || {
      echo "FAIL: release.yml no longer derives versions as this test assumes: $fragment" >&2
      exit 1
    }
  done

  read -r final_v final_deb final_py <<<"$(derive v0.2.1)"
  read -r rc_v rc_deb rc_py <<<"$(derive v0.2.1-rc.1)"

  [[ "$final_deb" == "0.2.1" && "$final_py" == "0.2.1" ]] || {
    echo "FAIL: a final tag must not gain a prerelease suffix (got $final_deb / $final_py)" >&2
    exit 1
  }
  [[ "$rc_deb" == "0.2.1~rc.1" ]] || {
    echo "FAIL: rc deb version must be 0.2.1~rc.1, got $rc_deb" >&2; exit 1; }
  [[ "$rc_py" == "0.2.1rc1" ]] || {
    echo "FAIL: rc python version must be PEP 440 0.2.1rc1, got $rc_py" >&2; exit 1; }
  [[ "$rc_v" == "$final_v" ]] || {
    echo "FAIL: the source version must be the same for a candidate and its release" >&2; exit 1; }
  [[ "$rc_deb" != "$final_deb" ]] || {
    echo "FAIL: candidate and final produce the same package version" >&2; exit 1; }

  # The build script must accept the Debian form and refuse the semver one:
  # `-rc.1` in an upstream version would be read as a Debian revision.
  grep -qF 'die "--version must be X.Y.Z or X.Y.Z~rc.N for release builds"' \
    tools/release/build-release-artifacts.sh || {
    echo "FAIL: build-release-artifacts.sh does not accept the candidate version form" >&2
    exit 1
  }

  # dpkg takes the version from the changelog, not from --version, so the
  # staging path must run for candidates and not only for snapshots.
  grep -qE 'elif \[\[ "\$VERSION" == \*"~"\* \]\]; then' \
    tools/release/build-release-artifacts.sh || {
    echo "FAIL: candidate builds do not stage their version into debian/changelog" >&2
    exit 1
  }

  # The ordering the whole fix rests on. dpkg is present on the CI runners
  # that run this suite; locally it may not be, and an unchecked claim is
  # reported rather than passed over in silence.
  if command -v dpkg >/dev/null 2>&1; then
    dpkg --compare-versions "${rc_deb}-1" lt "${final_deb}-1" || {
      echo "FAIL: ${rc_deb}-1 must sort below ${final_deb}-1 or there is no upgrade path" >&2
      exit 1
    }
    echo "candidate versions sort below their release (dpkg-verified)"
  else
    echo "candidate version derivation checked; dpkg absent, ordering NOT verified here"
  fi
)

printf 'release script checks green\n'
