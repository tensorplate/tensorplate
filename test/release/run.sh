#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Lightweight host checks for the release driver. These checks do not create
# tags, publish releases, or require root.

set -Eeuo pipefail

repo_root="$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)"
cd "$repo_root"

script="tools/release/tensorplate-release.sh"
build_script="tools/release/build-release-artifacts.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

bash -n "$script"
bash -n "$build_script"
"$script" --help >/dev/null
"$script" prepare --version 0.1.0 --dry-run >/dev/null
"$script" cut --version 0.1.0 --final --dry-run >/dev/null
"$script" cut --version 0.1.0 --rc 1 --dry-run >/dev/null
"$build_script" --help >/dev/null
grep -q 'name: Release' .github/workflows/release.yml
grep -q 'tools/release/build-release-artifacts.sh' .github/workflows/release.yml
grep -q 'draft="true"' .github/workflows/release.yml
grep -q 'gh release edit "${TP_TAG}" --draft=false --latest' docs/release/runbook.md

mkdir -p "$tmp/artifacts"
for pkg in \
  tensorplate-common \
  tensorplate-agent \
  tensorplate-serving \
  tensorplate-observability \
  tensorplate-cli \
  tensorplate-backend-python-pytorch; do
  printf 'fixture artifact for %s\n' "$pkg" > "$tmp/artifacts/${pkg}_0.1.0-1_arm64.deb"
done
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
assert len(manifest["artifacts"]) == 7  # 6 packages + install.sh
assert len(checksums) == 8  # manifest self-digest + 7 artifacts
PY

printf 'release script checks green\n'
