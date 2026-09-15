#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# The macOS offline-runtime stages: the helper's profile, derivation,
# readback and classification logic; the harness's offline-runtime stage,
# cleanup and signal handling against a fake host; and, on macOS only,
# the preflight against the real sandbox-exec. Touches no launchd job,
# Homebrew state or network: every probe socket is pinned to lo0.

set -Eeuo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
helper="${repo_root}/tools/validation/macos_offline_runtime.py"
tests="${repo_root}/test/packaging/verify_macos_offline_runtime.py"
fake_host="${repo_root}/test/packaging/fixtures/macos-offline/fake_host.py"

for file in "$helper" "$tests" "$fake_host"; do
  [[ -f "$file" ]] || {
    printf 'FAIL: missing %s\n' "$file" >&2
    exit 1
  }
done
[[ -x "$fake_host" ]] || {
  printf 'FAIL: %s must be executable; the stage tests run it by name\n' "$fake_host" >&2
  exit 1
}

python3 -m py_compile "$helper" "$tests" "$fake_host"
python3 "$tests"

printf 'verify_macos_offline_runtime: ok\n'
