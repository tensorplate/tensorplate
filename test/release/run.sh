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
  fx="$(mktemp -d)"; art="$fx/artifacts"; mkdir -p "$art"
  for pkg in tensorplate-common tensorplate-agent tensorplate-serving \
             tensorplate-observability tensorplate-cli \
             tensorplate-backend-python-pytorch tensorplate-apt-source tensorplate; do
    case "$pkg" in
      tensorplate-common|tensorplate-backend-python-pytorch|tensorplate-apt-source) a=all ;;
      *) a=arm64 ;;
    esac
    : >"$art/${pkg}_${deb}-1_${a}.deb"
  done
  for pkg in tensorplate tensorplate-agent tensorplate-serving \
             tensorplate-observability tensorplate-cli; do
    : >"$art/${pkg}_${deb}-1_amd64.deb"
  done
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

  # Control: without the package identity, the manifest layer must refuse
  # the same artifacts. A fixture that cannot fail proves nothing.
  if tools/release/tensorplate-release.sh manifest \
      --version "$canon" --tag v0.2.1-rc.1 --artifacts-dir "$art" \
      --manifest "$fx/control.json" --checksums "$fx/control.sums" --arch arm64 \
      >/dev/null 2>&1; then
    echo "FAIL: the manifest layer accepted candidate artifacts under the canonical version" >&2
    rm -rf "$fx"; exit 1
  fi
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

printf 'release script checks green\n'
