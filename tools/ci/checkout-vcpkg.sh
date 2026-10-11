#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Checks vcpkg out at the builtin-baseline of vcpkg.json, bootstraps it and
# exports VCPKG_ROOT to the steps after this one.
#
# vcpkg reads each port version the baseline names from the versions/ files
# of the checkout's working tree, so a checkout older than the baseline fails
# at configure. A hosted image's own checkout is as old as the image, which
# made the baseline depend on the oldest image in rotation. Fetching the one
# commit is enough: the baseline's ports are in its own tree.
#
# Usage: tools/ci/checkout-vcpkg.sh   (from the repository root; needs
#        RUNNER_TEMP and GITHUB_ENV)

set -Eeuo pipefail

readonly URL='https://github.com/microsoft/vcpkg.git'

fail() {
  printf '::error::%s\n' "$*"
  exit 1
}

baseline="$(python3 - vcpkg.json <<'PY'
import json
import re
import sys

with open(sys.argv[1], encoding="utf-8") as manifest_file:
    baseline = json.load(manifest_file).get("builtin-baseline")
if not isinstance(baseline, str) or not re.fullmatch("[0-9a-f]{40}", baseline):
    sys.exit("vcpkg.json names no builtin-baseline of 40 lowercase hex digits")
print(baseline)
PY
)" || fail "vcpkg.json names no builtin-baseline of 40 lowercase hex digits"

root="${RUNNER_TEMP}/vcpkg"
git init --quiet -- "$root"
git -C "$root" remote add origin "$URL"
git -C "$root" fetch --quiet --depth 1 origin "$baseline" ||
  fail "could not fetch the vcpkg commit ${baseline} that vcpkg.json names as its builtin-baseline"
git -C "$root" checkout --quiet --detach FETCH_HEAD
"${root}/bootstrap-vcpkg.sh" -disableMetrics ||
  fail "bootstrapping vcpkg at ${baseline} failed"

echo "VCPKG_ROOT=${root}" >>"$GITHUB_ENV"
printf 'vcpkg %s in %s\n' "$(git -C "$root" rev-parse HEAD)" "$root"
