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
grep -q 'gh release edit "${TP_TAG}" --draft=false --latest' docs/release/runbook.md

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
printf 'fixture artifact for tensorplate-cli desktop\n' > "$tmp/artifacts/tensorplate-cli_0.1.0-1_amd64.deb"
printf 'fixture installer\n' > "$tmp/artifacts/install.sh"

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
assert len(manifest["artifacts"]) == 10  # 8 packages + desktop CLI + install.sh
assert len(checksums) == 11  # manifest self-digest + 10 artifacts
assert any(artifact["file"] == "tensorplate-common_0.1.0-1_all.deb" for artifact in manifest["artifacts"])
assert any(artifact["file"] == "tensorplate-apt-source_0.1.0-1_all.deb" for artifact in manifest["artifacts"])
assert any(artifact["file"] == "tensorplate_0.1.0-1_arm64.deb" for artifact in manifest["artifacts"])
assert any(artifact["file"] == "tensorplate-cli_0.1.0-1_amd64.deb" for artifact in manifest["artifacts"])
PY

# Publish-grade releases require the amd64 desktop CLI: an arm64-only CLI
# must not satisfy the requirement on either the manifest-generation or
# the verification path (snapshot flows below stay exempt).
mkdir -p "$tmp/artifacts-no-amd64"
cp "$tmp/artifacts/"* "$tmp/artifacts-no-amd64/"
rm "$tmp/artifacts-no-amd64/tensorplate-cli_0.1.0-1_amd64.deb"
if "$script" manifest \
  --version 0.1.0 \
  --tag v0.1.0 \
  --artifacts-dir "$tmp/artifacts-no-amd64" \
  --manifest "$tmp/no-amd64-artifacts.json" \
  --checksums "$tmp/no-amd64-SHA256SUMS" >/dev/null 2>&1; then
  echo "FAIL: manifest must reject a release artifact set without the amd64 CLI" >&2
  exit 1
fi

python3 - "$tmp/tensorplate-v0.1.0-artifacts.json" "$tmp/SHA256SUMS" <<'PY'
# Strip the amd64 CLI from an otherwise valid manifest and refresh the
# checksum self-digest so verification exercises the amd64 gate itself
# rather than a checksum mismatch.
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
    if not (a.get("package") == "tensorplate-cli" and a.get("architecture") == "amd64")
]
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
  echo "FAIL: verify must reject a manifest without the amd64 CLI" >&2
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

printf 'release script checks green\n'
