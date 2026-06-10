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
